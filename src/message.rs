use std::io::{Cursor, Read, Write};

///
/// ### Message header
///
/// Packet payloads consist of multiple Messages.
/// Messages are prefixed by a message header.
///
/// ### Small flag
///
/// * For non-frag msgs, payload size is a u8 (256b msgs)
/// * for frags, number of fragments, and also fragment id, uses u8 (256kB payloads)
///
/// ### Large flag
/// * For frag msgs, payload size uses 2 bytes, a u16 (can fill packet, up to ~1024B)
/// * for frags, number of fragments, and also fragment id, uses u16 (65,536 * 1kB = loads)
///
///
/// `MessagePrefixByte`
/// | bits       |          | description                            |
/// | ---------- | -------- | -------------------------------------- |
/// | `-------X` | `<< 0`   | X = 0 non-fragmented. X = 1 fragmented |
/// | `------X-` | `<< 1`   | X = 0 small flag, X = 1 large flag     |
/// | `XXXXXX--` | `<< 2-7` | channel number 2^6 = 64 channels       |
///
/// ## Non-fragmented Message
///
/// | bytes  | type       | description                                                                     |
/// | ------ | ---------- | ------------------------------------------------------------------------------- |
/// | 1      | `u8`       | `MessagePrefixByte`                                                             |
/// | 1 or 2 | `u8`/`u16` | Payload Length, 1 or 2 bytes, depending on `MessagePrefixByte` small/large flag |
/// | ...    | Payload    |                                                                                 |
///
/// ## Fragmented Message
///
/// need to take care with multiple frag groups in flight - ensure no overlap or we get old frags..
/// also can probably pack frag group, id, num frags more concisely.
///
/// | bytes  | type          | description                                              |
/// | ------ | ------------- | -------------------------------------------------------- |
/// | 1      | `u8`          | `MessagePrefixByte`                                      |
/// | 1      | `u8`          | frag group id (same for all fragments in msg.)           |
/// | 1 or 2 | `u8` or `u16` | fragment id, depending on small/large flag               |
/// | 1 or 2 | `u8` or `u16` | num fragments, depending on small/large flag             |
/// | 2      | `u16`         | Payload Length, only on last fragment_id. Rest are 1024. |
/// | ..     | Payload       |                                                          |
///
use crate::{
    buffer_pool::{BufHandle, BufPool},
    PacketeerError,
};
use byteorder::{NetworkEndian, WriteBytesExt};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::error;

pub type MessageId = u16;

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
    ) -> Result<(), PacketeerError> {
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
        } else {
            // TODO return error here? can we even get corrupted packets off our webrtc transport?
            // do we get checksums for free?
            assert_eq!(
                payload_len, 1024,
                "non-last frag packets should have payload size of 1024"
            );
        }
        Ok(())
    }

    pub fn parse_header(
        reader: &mut Bytes,
        size_mode: MessageSizeMode,
        id: MessageId,
    ) -> Result<(Self, u16), PacketeerError> {
        let (fragment_id, num_fragments) = match size_mode {
            MessageSizeMode::Small => {
                if reader.remaining() < 2 {
                    error!("parse message error 4");
                    return Err(PacketeerError::InvalidMessage);
                }
                (reader.get_u8() as u16, reader.get_u8() as u16)
            }
            MessageSizeMode::Large => {
                if reader.remaining() < 4 {
                    error!("parse message error 5");
                    return Err(PacketeerError::InvalidMessage);
                }
                (reader.get_u16(), reader.get_u16())
            }
        };
        let payload_size = if fragment_id == num_fragments - 1 {
            if reader.remaining() < 2 {
                error!("parse message error 6");
                return Err(PacketeerError::InvalidMessage);
            }
            reader.get_u16()
        } else {
            1024_u16
        };
        Ok((
            Fragment {
                index: fragment_id,
                num_fragments,
                parent_id: id.wrapping_sub(fragment_id),
            },
            payload_size,
        ))
    }
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum MessageSizeMode {
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
    buffer: BufHandle,
    fragment: Option<Fragment>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message{{id:{}, payload_len:{} fragment:{:?} channel:{}",
            self.id,
            self.buffer.len(),
            self.fragment,
            self.channel,
        )
    }
}
// TODO pool holds messages, and they get a builder style api to construct once issued?

impl Message {
    pub(crate) fn new_outbound(
        pool: &BufPool,
        id: MessageId,
        channel: u8,
        payload: &[u8],
        fragmented: Fragmented,
    ) -> Self {
        assert!(channel < 64, "max channel id is 64");
        assert!(payload.len() <= 1024, "max payload size is 1024");
        let size_mode = if payload.len() > 255 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        let header_size = Self::header_size(&fragmented, size_mode);
        let fragment = match fragmented {
            Fragmented::No => None,
            Fragmented::Yes(f) => Some(f),
        };
        let mut buf = pool.get_buffer(header_size + payload.len());
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

    // writes the buffer to the writer
    pub(crate) fn write(&self, mut writer: impl std::io::Write) -> Result<(), PacketeerError> {
        writer.write_all(self.buffer())?;
        Ok(())
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

    pub(crate) fn fragment(&self) -> Option<&Fragment> {
        self.fragment.as_ref()
    }

    pub fn id(&self) -> MessageId {
        self.id
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }
    pub fn buffer(&self) -> &[u8] {
        &self.buffer.as_slice()
    }

    pub fn size(&self) -> usize {
        self.buffer.len()
            + 1
            + match (self.fragment.is_some(), self.size_mode) {
                // small unfragmented
                (false, MessageSizeMode::Small) => 1,
                // large unfragmented
                (false, MessageSizeMode::Large) => 2,
                // small fragmented
                (true, MessageSizeMode::Small) => {
                    3 + if self.fragment.as_ref().unwrap().is_last() {
                        2
                    } else {
                        0
                    }
                }
                // large fragmented
                (true, MessageSizeMode::Large) => {
                    5 + if self.fragment.as_ref().unwrap().is_last() {
                        2
                    } else {
                        0
                    }
                }
            }
            // add message id: TODO u16 for now
            +  2
    }

    // TODO check reminaing and error if writes will panic
    pub(crate) fn write_headers(
        mut writer: impl std::io::Write,
        id: MessageId,
        fragment: &Option<Fragment>,
        size_mode: MessageSizeMode,
        channel: u8,
        payload_len: usize,
    ) -> Result<(), PacketeerError> {
        let mut prefix_byte = 0_u8;
        if fragment.is_some() {
            prefix_byte = 1;
        }
        if size_mode == MessageSizeMode::Large {
            prefix_byte |= 1 << 1;
        }
        // // spare flag
        // if flag_true {
        //     prefix_byte |= 1 << 2;
        // }
        let channel_mask = channel << 3;
        prefix_byte |= channel_mask;

        writer.write_u8(prefix_byte)?;
        writer.write_u16::<NetworkEndian>(id)?;

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

    pub fn parse(pool: &BufPool, reader: &mut Bytes) -> Result<Self, PacketeerError> {
        if reader.remaining() < 1 {
            error!("parse message error 1");
            return Err(PacketeerError::InvalidMessage);
        }
        let prefix_byte = reader.get_u8();
        let fragmented = prefix_byte & 1 != 0;
        let size_mode = if prefix_byte & (1 << 1) != 0 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        // let spare_flag = prefix_byte & (1 << 2) != 0
        let id = reader.get_u16();
        let channel = prefix_byte >> 3;
        // TODO refac into MessageHeader which has an opt fragment and does the parsing?
        let (fragment, payload_size) = if !fragmented {
            let payload_size = match size_mode {
                MessageSizeMode::Small => {
                    if reader.remaining() < 1 {
                        error!("parse message error 2");
                        return Err(PacketeerError::InvalidMessage);
                    }
                    reader.get_u8() as u16
                }
                MessageSizeMode::Large => {
                    if reader.remaining() < 2 {
                        error!("parse message error 3");
                        return Err(PacketeerError::InvalidMessage);
                    }
                    reader.get_u16()
                }
            };
            (None, payload_size)
        } else {
            let (fragment, payload_size) = Fragment::parse_header(reader, size_mode, id)?;
            (Some(fragment), payload_size)
        };
        if reader.remaining() < payload_size as usize {
            return Err(PacketeerError::InvalidMessage);
        }
        // copy payload from reader into buf
        let mut buf = pool.get_buffer(payload_size as usize);
        reader
            .take(payload_size as usize)
            .reader()
            .read_to_end(&mut *buf);

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
    // explicit import to override bevy
    // use log::{debug, error, info, trace, warn};

    #[test]
    fn message_serialization() {
        crate::test_utils::init_logger();
        let pool = BufPool::new();

        let payload1 = b"HELLO";
        let payload2 = b"FRAGMENTED";
        let payload3 = b"WORLD";
        let msg1 = Message::new_outbound(&pool, 1, 1, payload1, Fragmented::No);
        // the last fragment can be a small msg that rides along with other unfragmented messages:
        let fragment = Fragment {
            index: 0,
            num_fragments: 1,
            parent_id: 1,
        };
        let msg2 = Message::new_outbound(&pool, 3, 5, payload2, Fragmented::Yes(fragment));
        let msg3 = Message::new_outbound(&pool, 2, 16, payload3, Fragmented::No);

        let mut buffer = Vec::with_capacity(1500);

        msg1.buffer().reader().read_to_end(buffer.as_mut()).unwrap();
        msg2.buffer().reader().read_to_end(buffer.as_mut()).unwrap();
        msg3.buffer().reader().read_to_end(buffer.as_mut()).unwrap();

        let mut incoming = Bytes::copy_from_slice(buffer.as_slice());

        let recv_msg1 = Message::parse(&pool, &mut incoming).unwrap();
        let recv_msg2 = Message::parse(&pool, &mut incoming).unwrap();
        let recv_msg3 = Message::parse(&pool, &mut incoming).unwrap();

        assert!(incoming.is_empty());
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
        let pool = BufPool::new();

        // fragment messages (except the last) have a fixed size of 1024 bytes
        let payload = &[41; 1024];
        let fragment = Fragment {
            index: 0,
            num_fragments: 10,
            parent_id: 1,
        };
        let msg = Message::new_outbound(&pool, 0, 0, payload, Fragmented::Yes(fragment));

        let mut buffer = Vec::with_capacity(1500);

        msg.buffer().reader().read_to_end(buffer.as_mut()).unwrap();

        let mut incoming = Bytes::copy_from_slice(&buffer[..]);

        let recv_msg = Message::parse(&pool, &mut incoming).unwrap();

        assert!(incoming.is_empty());

        assert_eq!(*recv_msg.buffer, payload);
        assert!(recv_msg.fragment.is_some());
        assert_eq!(recv_msg.fragment.as_ref().unwrap().index, 0);
        assert_eq!(recv_msg.fragment.as_ref().unwrap().num_fragments, 10);
    }
}
