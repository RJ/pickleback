use crate::{
    buffer_pool::{BufPool, PooledBuffer},
    cursor::CursorExtras,
    MessageId, PicklebackError,
};
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Read, Write};

#[derive(Debug, Clone)]
pub(crate) struct Fragment {
    pub index: u16,
    pub num_fragments: u16,
    pub parent_id: MessageId,
}

impl Fragment {
    fn is_last(&self) -> bool {
        self.index == self.num_fragments - 1
    }
    fn header_size(&self, size_mode: MessageSizeMode) -> usize {
        (match size_mode {
            MessageSizeMode::Small => 2,
            MessageSizeMode::Large => 4,
        }) + if self.is_last() { 2 } else { 0 }
    }
    pub fn write_header(
        &self,
        mut writer: impl std::io::Write,
        payload_len: u16,
        size_mode: MessageSizeMode,
    ) -> Result<(), PicklebackError> {
        match size_mode {
            MessageSizeMode::Small => {
                writer.write_u8(self.index as u8)?;
                writer.write_u8(self.num_fragments as u8)?;
            }
            MessageSizeMode::Large => {
                writer.write_u16::<NetworkEndian>(self.index)?;
                writer.write_u16::<NetworkEndian>(self.num_fragments)?;
            }
        }
        // only the last fragment has a payload size. others are 1024.
        if self.is_last() {
            assert!(payload_len <= 1024);
            writer.write_u16::<NetworkEndian>(payload_len)?;
        } else if payload_len != 1024 {
            // not possible in transports that checksum - truncated packets manifest as drops.
            log::error!(
                "Non-final fragment should always have payload size 1024. got {payload_len}."
            );
            return Err(PicklebackError::InvalidMessage);
        }
        Ok(())
    }

    pub fn parse_header(
        reader: &mut Cursor<&[u8]>,
        size_mode: MessageSizeMode,
        id: MessageId,
    ) -> Result<(Self, u16), PicklebackError> {
        let (fragment_id, num_fragments) = match size_mode {
            MessageSizeMode::Small => (reader.read_u8()? as u16, reader.read_u8()? as u16),
            MessageSizeMode::Large => (
                reader.read_u16::<NetworkEndian>()?,
                reader.read_u16::<NetworkEndian>()?,
            ),
        };
        let payload_size = if fragment_id == num_fragments - 1 {
            reader.read_u16::<NetworkEndian>()?
        } else {
            1024_u16
        };
        Ok((
            Fragment {
                index: fragment_id,
                num_fragments,
                parent_id: MessageId(id.0.wrapping_sub(fragment_id)),
            },
            payload_size,
        ))
    }
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub(crate) enum MessageSizeMode {
    Small,
    Large,
}

pub(crate) enum Fragmented {
    No,
    Yes(Fragment),
}

/// Messages are coalesced and written together into packets.
/// each message has a header.
/// they can be fragments of a larger message, which get reassembled.
#[derive(Clone)]
pub struct Message {
    id: MessageId,
    size_mode: MessageSizeMode,
    channel: u8,
    buffer: PooledBuffer,
    fragment: Option<Fragment>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message{{id:{:?}, buffer.len:{} fragment:{:?} channel:{}",
            self.id,
            self.buffer.len(),
            self.fragment,
            self.channel,
        )
    }
}

// Payloads > 255 bytes need more than a `u8` size prefix
impl From<usize> for MessageSizeMode {
    fn from(val: usize) -> Self {
        if val > 255 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        }
    }
}

impl Message {
    /// Creates a new message, allocates a buffer from the pool, and writes headers and payload to
    /// the buffer immediately.
    pub(crate) fn new_outbound(
        pool: &mut BufPool,
        id: MessageId,
        channel: u8,
        payload: &[u8],
        fragmented: Fragmented,
    ) -> Self {
        assert!(channel < 64, "max channel id is 64");
        assert!(payload.len() <= 1024, "max payload size is 1024");

        let size_mode = payload.len().into();
        let header_size = Self::header_size(&fragmented, size_mode);
        let fragment = match fragmented {
            Fragmented::No => None,
            Fragmented::Yes(f) => Some(f),
        };
        let mut buf = pool.get_buffer(header_size + payload.len());
        log::warn!(
            "new_outbound buffer = '{buf:?}' min_size: {} cap: {}",
            header_size + payload.len(),
            buf.capacity()
        );
        let mut writer = Cursor::new(&mut *buf);
        Self::write_headers(
            &mut writer,
            id,
            &fragment,
            size_mode,
            channel,
            payload.len(),
        )
        .unwrap();
        writer.write_all(payload).unwrap();
        Self {
            id,
            size_mode,
            channel,
            buffer: buf,
            fragment,
        }
    }

    pub(crate) fn header_size(fragmented: &Fragmented, size_mode: MessageSizeMode) -> usize {
        // prefix byte
        1 +
        if let Fragmented::Yes(frag) = fragmented {
            frag.header_size(size_mode)
        } else {
            match size_mode {
                MessageSizeMode::Large => 2,
                MessageSizeMode::Small => 1,
            }
        }
        // message id
        + 2
    }

    /// Takes the buffer, leaving a default empty one.
    /// used when message is finished with, to return buffer to pool.
    pub(crate) fn take_buffer(&mut self) -> PooledBuffer {
        std::mem::take(&mut self.buffer)
    }

    pub(crate) fn fragment(&self) -> Option<&Fragment> {
        self.fragment.as_ref()
    }

    pub fn id(&self) -> MessageId {
        self.id
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }
    pub fn as_slice(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    pub fn buffer(&self) -> &Vec<u8> {
        &self.buffer
    }

    pub fn size(&self) -> usize {
        let (is_fragment, is_last_fragment) = if let Some(frag) = self.fragment.as_ref() {
            (true, frag.is_last())
        } else {
            (false, false)
        };
        self.buffer.len()
            + 1
            + match (is_fragment, self.size_mode) {
                // small unfragmented
                (false, MessageSizeMode::Small) => 1,
                // large unfragmented
                (false, MessageSizeMode::Large) => 2,
                // small fragmented
                (true, MessageSizeMode::Small) if is_last_fragment => 5,
                (true, MessageSizeMode::Small)  => 3,
                // large fragmented
                (true, MessageSizeMode::Large) if is_last_fragment => 7,
                (true, MessageSizeMode::Large)  => 5,
            }
            // add message id u16
            // TODO (encode as delta to prev msg id?)
            +  2
    }

    pub(crate) fn write_headers(
        mut writer: impl std::io::Write,
        id: MessageId,
        fragment: &Option<Fragment>,
        size_mode: MessageSizeMode,
        channel: u8,
        payload_len: usize,
    ) -> Result<(), PicklebackError> {
        // three bits are reserved for flags:
        // * is fragment?
        // * size mode large?
        // * unused flag
        // remaining bits are channel id.
        let mut prefix_byte = 0_u8;
        if fragment.is_some() {
            prefix_byte = 0b0000_0001;
        }
        if size_mode == MessageSizeMode::Large {
            prefix_byte |= 0b0000_0010;
        }
        // // spare flag
        // if flag_true {
        //     prefix_byte |= 0b0000_0100;
        // }
        let channel_mask = channel << 3;
        prefix_byte |= channel_mask;

        writer.write_u8(prefix_byte)?;
        writer.write_u16::<NetworkEndian>(id.0)?;

        if let Some(fragment) = fragment.as_ref() {
            fragment.write_header(writer, payload_len as u16, size_mode)?;
        } else {
            match size_mode {
                MessageSizeMode::Small => writer.write_u8(payload_len as u8)?,
                MessageSizeMode::Large => writer.write_u16::<NetworkEndian>(payload_len as u16)?,
            }
        }
        Ok(())
    }

    pub fn parse(pool: &mut BufPool, reader: &mut Cursor<&[u8]>) -> Result<Self, PicklebackError> {
        let prefix_byte = reader.read_u8()?;
        let fragmented = prefix_byte & 1 != 0;
        let size_mode = if prefix_byte & (1 << 1) != 0 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        // let spare_flag = prefix_byte & (1 << 2) != 0
        let id = MessageId(reader.read_u16::<NetworkEndian>()?);
        let channel = prefix_byte >> 3;

        let (fragment, payload_size) = if !fragmented {
            let payload_size = match size_mode {
                MessageSizeMode::Small => reader.read_u8()? as u16,
                MessageSizeMode::Large => reader.read_u16::<NetworkEndian>()?,
            };
            (None, payload_size)
        } else {
            let (fragment, payload_size) = Fragment::parse_header(reader, size_mode, id)?;
            (Some(fragment), payload_size)
        };
        // copy payload from reader into buf
        let mut buf = pool.get_buffer(payload_size as usize);
        if reader.remaining() < payload_size as u64 {
            log::warn!("Payload appears truncated for message {id:?}");
            return Err(PicklebackError::InvalidMessage);
        }
        reader.take(payload_size as u64).read_to_end(&mut buf)?;

        Ok(Self {
            id,
            size_mode,
            channel,
            buffer: buf,
            fragment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serialization() {
        crate::test_utils::init_logger();
        let mut pool = BufPool::empty();

        let payload1 = b"HELLO";
        let payload2 = b"FRAGMENTED";
        let payload3 = b"WORLD";
        let msg1 = Message::new_outbound(&mut pool, MessageId(1), 1, payload1, Fragmented::No);
        // the last fragment can be a small msg that rides along with other unfragmented messages:
        let fragment = Fragment {
            index: 0,
            num_fragments: 1,
            parent_id: MessageId(1),
        };
        let msg2 = Message::new_outbound(
            &mut pool,
            MessageId(3),
            5,
            payload2,
            Fragmented::Yes(fragment),
        );
        let msg3 = Message::new_outbound(&mut pool, MessageId(2), 16, payload3, Fragmented::No);

        let mut buffer = Vec::with_capacity(1500);
        buffer.extend_from_slice(msg1.as_slice());
        buffer.extend_from_slice(msg2.as_slice());
        buffer.extend_from_slice(msg3.as_slice());

        let incoming = Vec::from(buffer.as_slice());
        let mut cur = Cursor::new(incoming.as_ref());

        let recv_msg1 = Message::parse(&mut pool, &mut cur).unwrap();
        let recv_msg2 = Message::parse(&mut pool, &mut cur).unwrap();
        let recv_msg3 = Message::parse(&mut pool, &mut cur).unwrap();

        assert_eq!(cur.position(), incoming.len() as u64);

        assert_eq!(*recv_msg1.buffer, payload1);
        assert_eq!(*recv_msg2.buffer, payload2);
        assert_eq!(*recv_msg3.buffer, payload3);

        assert_eq!(recv_msg3.id(), msg3.id());

        assert_eq!(recv_msg1.channel(), msg1.channel());
        assert_eq!(recv_msg2.channel(), msg2.channel());
        assert_eq!(recv_msg3.channel(), msg3.channel());

        assert!(recv_msg1.fragment.is_none());
        assert!(recv_msg2.fragment.is_some());
        assert_eq!(recv_msg2.fragment.as_ref().unwrap().index, 0);
        assert_eq!(recv_msg2.fragment.as_ref().unwrap().num_fragments, 1);
        assert!(recv_msg3.fragment.is_none());
    }

    #[test]
    fn fragment_message_serialization() {
        crate::test_utils::init_logger();
        let mut pool = BufPool::empty();

        // fragment messages (except the last) have a fixed size of 1024 bytes
        let payload = &[41; 1024];
        let fragment = Fragment {
            index: 0,
            num_fragments: 10,
            parent_id: MessageId(1),
        };
        let msg = Message::new_outbound(
            &mut pool,
            MessageId(0),
            0,
            payload,
            Fragmented::Yes(fragment),
        );

        let mut buffer = Vec::with_capacity(1500);
        buffer.extend_from_slice(msg.as_slice());

        let mut incoming = Cursor::new(buffer.as_ref());

        let recv_msg = Message::parse(&mut pool, &mut incoming).unwrap();

        assert_eq!(incoming.position(), buffer.len() as u64);

        assert_eq!(*recv_msg.buffer, payload);
        assert!(recv_msg.fragment.is_some());
        assert_eq!(recv_msg.fragment.as_ref().unwrap().index, 0);
        assert_eq!(recv_msg.fragment.as_ref().unwrap().num_fragments, 10);
    }
}
