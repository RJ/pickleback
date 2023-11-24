use std::io::{empty, Cursor, Read};

use crate::*;

#[derive(Debug)]
enum ReceivedMessageType {
    Single(Message),
    Fragmented(Vec<Message>),
}

/// Wrapper over either a single unfragmented message, or a vec of fragments.
/// Provides a reader which chains together payload split over fragmented messages.
#[derive(Debug)]
pub struct ReceivedMessage {
    message_type: ReceivedMessageType,
}

impl ReceivedMessage {
    /// Creates a ReceivedMessage backed by a single `Message`
    pub fn new_single(message: Message) -> Self {
        Self {
            message_type: ReceivedMessageType::Single(message),
        }
    }
    /// Creates a ReceivedMessage backed by multiple fragment `Message`s
    pub fn new_fragmented(fragments: Vec<Message>) -> Self {
        Self {
            message_type: ReceivedMessageType::Fragmented(fragments),
        }
    }
    /// Get MessageId.
    /// In the case of a fragmented message, this is always the ID of the first message.
    ///
    /// # Panics
    ///
    /// Unwraps messages in the vec, but they must exist - them existing was the trigger to build ReceivedMessage
    ///
    pub fn id(&self) -> MessageId {
        match &self.message_type {
            ReceivedMessageType::Single(message) => message.id(),
            ReceivedMessageType::Fragmented(v) => v[0].fragment().unwrap().parent_id,
        }
    }
    /// Channel this message was sent on.
    pub fn channel(&self) -> u8 {
        match &self.message_type {
            ReceivedMessageType::Single(message) => message.channel(),
            ReceivedMessageType::Fragmented(v) => v[0].channel(),
        }
    }
    /// Length of message payload.
    pub fn payload_len(&self) -> usize {
        match &self.message_type {
            ReceivedMessageType::Single(message) => message.size(),
            ReceivedMessageType::Fragmented(v) => 1024 * (v.len() - 1) + v[v.len() - 1].size(),
        }
    }
    /// Reader for the payload.
    ///
    /// In the case of fragmented messages, this will chain together readers over
    /// multiple underlying message buffers.
    pub fn payload(&self) -> Box<dyn Read + '_> {
        match &self.message_type {
            ReceivedMessageType::Single(message) => Box::new(Cursor::new(message.as_slice())),
            ReceivedMessageType::Fragmented(v) => {
                // chain readers together
                v.iter()
                    .map(|msg| Cursor::new(msg.as_slice()))
                    .fold(Box::new(empty()) as Box<dyn Read>, |acc, cur| {
                        Box::new(acc.chain(cur))
                    })
            }
        }
    }
    /// Read payload cursor into a new Vec<u8> and return it.
    ///
    /// This is for unit tests
    ///
    /// # Panics
    /// Panics if unable to read entire payload for some reason.
    pub fn payload_to_owned(&self) -> Vec<u8> {
        let mut ret = Vec::with_capacity(self.payload_len());

        self.payload()
            .read_to_end(&mut ret)
            .expect("Couldn't read payload");

        ret
    }
}

// impl std::fmt::Debug for ReceivedMessage {
//     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
//         write!(
//             f,
//             "ReceivedMessage{{id: {}, channel: {}}}",
//             self.id(),
//             self.channel(),
//         )
//     }
// }
