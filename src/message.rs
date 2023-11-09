use crate::ReliableError;
// use bevy::log::*;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::{debug, error, info, trace, warn};

pub type MessageId = u32;

#[derive(Debug)]
struct Fragment {
    id: u16,
    num_fragments: u16,
    // parent_id only used sender-side
    parent_id: Option<MessageId>,
}

impl Fragment {
    fn is_last(&self) -> bool {
        self.id == self.num_fragments - 1
    }
}

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum MessageSizeMode {
    Small,
    Large,
}

/// Messages are coalesced and written together into packets.
/// each message has a header.
/// they can be fragments of a larger message, which get reassembled.
#[derive(Debug)]
pub struct Message {
    id: MessageId,
    size_mode: MessageSizeMode,
    channel: u8,
    payload: Bytes,
    fragment: Option<Fragment>,
}

impl Message {
    pub fn new_unfragmented(id: MessageId, channel: u8, payload: Bytes) -> Self {
        assert!(channel < 64, "max channel id is 64");
        assert!(payload.len() <= 1024, "max payload size is 1024");
        let size_mode = if payload.len() > 255 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        Self {
            id,
            size_mode,
            channel,
            payload,
            fragment: None,
        }
    }

    pub fn new_fragment(
        id: MessageId,
        channel: u8,
        payload: Bytes,
        fragment_id: u16,
        num_fragments: u16,
        parent_id: MessageId,
    ) -> Self {
        assert!(channel < 64, "max channel id is 64");
        assert!(payload.len() <= 1024, "max payload size is 1024");
        let size_mode = if payload.len() > 255 {
            MessageSizeMode::Large
        } else {
            MessageSizeMode::Small
        };
        Self {
            id,
            size_mode,
            channel,
            payload,
            fragment: Some(Fragment {
                id: fragment_id,
                num_fragments,
                parent_id: Some(parent_id),
            }),
        }
    }

    pub fn parent_id(&self) -> Option<MessageId> {
        if let Some(fragment) = self.fragment.as_ref() {
            fragment.parent_id
        } else {
            None
        }
    }

    pub fn id(&self) -> MessageId {
        self.id
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
                    2 + if self.fragment.as_ref().unwrap().is_last() {
                        2
                    } else {
                        0
                    }
                }
                // large fragmented
                (true, MessageSizeMode::Large) => {
                    4 + if self.fragment.as_ref().unwrap().is_last() {
                        2
                    } else {
                        0
                    }
                }
            }
    }

    // pub fn new_fragments(channel: u8, payload: Bytes) -> Self {
    //     assert!(channel < 64, "max channel id is 64");
    //     assert!(payload.len() <= 1024, "max unfragmented payload size is 1024");
    //     let size_mode = if payload.len() > 255 {
    //         MessageSizeMode::Large
    //     } else {
    //         MessageSizeMode::Small
    //     };
    //     Self {
    //         size_mode,
    //         channel,
    //         payload,
    //         fragment: None,
    //     }
    // }

    // TODO check reminaing and error if writes will panic
    pub fn write(&self, writer: &mut BytesMut) -> Result<(), ReliableError> {
        let mut prefix_byte = 0_u8;
        if self.fragment.is_some() {
            prefix_byte = 1;
        }
        if self.size_mode == MessageSizeMode::Large {
            prefix_byte |= 1 << 1;
        }
        let channel_mask = self.channel << 2;
        prefix_byte |= channel_mask;

        writer.put_u8(prefix_byte);

        if let Some(Fragment {
            id: fragment_id,
            num_fragments,
            parent_id: _,
        }) = self.fragment
        {
            match self.size_mode {
                MessageSizeMode::Small => {
                    writer.put_u8(fragment_id as u8);
                    writer.put_u8(num_fragments as u8);
                }
                MessageSizeMode::Large => {
                    writer.put_u16(fragment_id);
                    writer.put_u16(num_fragments);
                }
            }
            // only the last fragment has a payload size. others are 1024.
            if fragment_id == num_fragments - 1 {
                assert!(self.payload.len() <= 1024);
                writer.put_u16(self.payload.len() as u16);
            } else {
                assert_eq!(self.payload.len(), 1024);
            }
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
        let channel = prefix_byte << 2;
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
                    (reader.get_u16() as u16, reader.get_u16())
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
            (
                Some(Fragment {
                    id: fragment_id,
                    num_fragments,
                    parent_id: None, // only used sender-side
                }),
                payload_size,
            )
        };

        if reader.remaining() < payload_size as usize {
            return Err(ReliableError::InvalidMessage);
        }
        let payload = reader.split_to(payload_size as usize);
        Ok(Self {
            id: 0, // message ids aren't sent over the network.
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
    use log::{debug, error, info, trace, warn};
    // use env_logger
    use std::sync::Once;

    static LOGGER_INIT: Once = Once::new();

    fn enable_logging() {
        LOGGER_INIT.call_once(|| {
            use env_logger::Builder;
            use log::LevelFilter;

            Builder::new().filter(None, LevelFilter::Trace).init();
        });
    }

    #[test]
    fn message_serialization() {
        enable_logging();

        let payload1 = Bytes::from_static("HELLO".as_bytes());
        let payload2 = Bytes::from_static("WORLD".as_bytes());
        let msg1 = Message::new_unfragmented(1, 0, payload1);
        let msg2 = Message::new_unfragmented(2, 0, payload2);

        let mut buffer = BytesMut::with_capacity(1500);
        msg1.write(&mut buffer).unwrap();
        msg2.write(&mut buffer).unwrap();

        let mut incoming = Bytes::copy_from_slice(&buffer[..]);

        let recv_msg1 = Message::parse(&mut incoming).unwrap();
        let recv_msg2 = Message::parse(&mut incoming).unwrap();

        assert!(incoming.is_empty());
        assert_eq!(recv_msg1.payload, msg1.payload);
        assert_eq!(recv_msg2.payload, msg2.payload);
    }
}
