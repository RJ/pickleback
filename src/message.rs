use crate::ReliableError;
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
    pub fn write_header(
        &self,
        writer: &mut BytesMut,
        payload_len: u16,
        size_mode: MessageSizeMode,
    ) -> Result<(), ReliableError> {
        match size_mode {
            MessageSizeMode::Small => {
                writer.put_u8(self.index as u8);
                writer.put_u8(self.num_fragments as u8);
            }
            MessageSizeMode::Large => {
                writer.put_u16(self.index);
                writer.put_u16(self.num_fragments);
            }
        }
        // only the last fragment has a payload size. others are 1024.
        if self.is_last() {
            assert!(payload_len <= 1024);
            writer.put_u16(payload_len);
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
    ) -> Result<(Self, u16), ReliableError> {
        let (fragment_id, num_fragments) = match size_mode {
            MessageSizeMode::Small => {
                if reader.remaining() < 2 {
                    error!("parse message error 4");
                    return Err(ReliableError::InvalidMessage);
                }
                (reader.get_u8() as u16, reader.get_u8() as u16)
            }
            MessageSizeMode::Large => {
                if reader.remaining() < 4 {
                    error!("parse message error 5");
                    return Err(ReliableError::InvalidMessage);
                }
                (reader.get_u16(), reader.get_u16())
            }
        };
        let payload_size = if fragment_id == num_fragments - 1 {
            if reader.remaining() < 2 {
                error!("parse message error 6");
                return Err(ReliableError::InvalidMessage);
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
    payload: Bytes,
    fragment: Option<Fragment>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message{{id:{}, payload_len:{} fragment:{:?} channel:{}",
            self.id,
            self.payload.len(),
            self.fragment,
            self.channel,
        )
    }
}

impl Message {
    pub(crate) fn new(id: MessageId, channel: u8, payload: Bytes, fragmented: Fragmented) -> Self {
        assert!(channel < 64, "max channel id is 64");
        assert!(payload.len() <= 1024, "max payload size is 1024");
        let size_mode = if payload.len() > 255 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        let fragment = match fragmented {
            Fragmented::No => None,
            Fragmented::Yes(f) => Some(f),
        };
        Self {
            id,
            size_mode,
            channel,
            payload,
            fragment,
        }
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
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }

    pub fn size(&self) -> usize {
        self.payload.len()
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
    pub fn write(&self, writer: &mut BytesMut) -> Result<(), ReliableError> {
        let mut prefix_byte = 0_u8;
        if self.fragment.is_some() {
            prefix_byte = 1;
        }
        if self.size_mode == MessageSizeMode::Large {
            prefix_byte |= 1 << 1;
        }
        // // spare flag
        // if flag_true {
        //     prefix_byte |= 1 << 2;
        // }
        let channel_mask = self.channel << 3;
        prefix_byte |= channel_mask;

        writer.put_u8(prefix_byte);
        writer.put_u16(self.id);

        if let Some(fragment) = self.fragment.as_ref() {
            fragment.write_header(writer, self.payload.len() as u16, self.size_mode)?;
        } else {
            match self.size_mode {
                MessageSizeMode::Small => writer.put_u8(self.payload.len() as u8),
                MessageSizeMode::Large => writer.put_u16(self.payload.len() as u16),
            }
        }
        writer.extend_from_slice(self.payload.as_ref());
        Ok(())
    }

    pub fn parse(reader: &mut Bytes) -> Result<Self, ReliableError> {
        if reader.remaining() < 1 {
            error!("parse message error 1");
            return Err(ReliableError::InvalidMessage);
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
                        return Err(ReliableError::InvalidMessage);
                    }
                    reader.get_u8() as u16
                }
                MessageSizeMode::Large => {
                    if reader.remaining() < 2 {
                        error!("parse message error 3");
                        return Err(ReliableError::InvalidMessage);
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
            return Err(ReliableError::InvalidMessage);
        }
        let payload = reader.split_to(payload_size as usize);
        Ok(Self {
            id,
            size_mode,
            channel,
            payload,
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

        let payload1 = Bytes::from_static("HELLO".as_bytes());
        let payload2 = Bytes::from_static("FRAGMENTED".as_bytes());
        let payload3 = Bytes::from_static("WORLD".as_bytes());
        let msg1 = Message::new(1, 1, payload1, Fragmented::No);
        // the last fragment can be a small msg that rides along with other unfragmented messages:
        let fragment = Fragment {
            index: 0,
            num_fragments: 1,
            parent_id: 1,
        };
        let msg2 = Message::new(3, 5, payload2, Fragmented::Yes(fragment));
        let msg3 = Message::new(2, 16, payload3, Fragmented::No);

        let mut buffer = BytesMut::with_capacity(1500);
        msg1.write(&mut buffer).unwrap();
        msg2.write(&mut buffer).unwrap();
        msg3.write(&mut buffer).unwrap();

        let mut incoming = Bytes::copy_from_slice(&buffer[..]);

        let recv_msg1 = Message::parse(&mut incoming).unwrap();
        let recv_msg2 = Message::parse(&mut incoming).unwrap();
        let recv_msg3 = Message::parse(&mut incoming).unwrap();

        assert!(incoming.is_empty());
        assert_eq!(recv_msg1.payload, msg1.payload);
        assert_eq!(recv_msg2.payload, msg2.payload);
        assert_eq!(recv_msg3.payload, msg3.payload);

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
        // fragment messages (except the last) have a fixed size of 1024 bytes
        let payload = Bytes::copy_from_slice(&[41; 1024]);
        let fragment = Fragment {
            index: 0,
            num_fragments: 10,
            parent_id: 1,
        };
        let msg = Message::new(0, 0, payload, Fragmented::Yes(fragment));

        let mut buffer = BytesMut::with_capacity(1500);
        msg.write(&mut buffer).unwrap();

        let mut incoming = Bytes::copy_from_slice(&buffer[..]);

        let recv_msg = Message::parse(&mut incoming).unwrap();

        assert!(incoming.is_empty());

        assert_eq!(recv_msg.payload, msg.payload);
        assert!(recv_msg.fragment.is_some());
        assert_eq!(recv_msg.fragment.as_ref().unwrap().index, 0);
        assert_eq!(recv_msg.fragment.as_ref().unwrap().num_fragments, 10);
    }
}
