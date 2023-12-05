#![warn(missing_docs, clippy::missing_errors_doc, clippy::missing_panics_doc)]
#![doc = include_str!("../README.md")]
#![deny(rustdoc::broken_intra_doc_links)]

use byteorder::{NetworkEndian, WriteBytesExt};
use cursor::BufferLimitedWriter;
use log::*;
use std::{
    collections::{HashMap, VecDeque},
    io::{Cursor, Write},
};
mod buffer_pool;
mod channel;
mod client;
mod config;
mod cursor;
mod dispatcher;
mod error;
mod jitter_pipe;
mod message_reassembler;
mod pickleback;
mod protocol;
mod received_message;
mod sequence_buffer;
mod server;
mod test_utils;

pub use buffer_pool::BufHandle;
pub(crate) use buffer_pool::*;
pub(crate) use channel::*;
pub use client::PicklebackClient;
pub(crate) use config::*;
pub(crate) use dispatcher::*;
pub(crate) use error::*;
pub(crate) use jitter_pipe::*;
pub(crate) use message_reassembler::*;
pub use server::PicklebackServer;

pub use pickleback::Pickleback;
// use protocol::*;
use received_message::*;
use sequence_buffer::*;

/// Easy importing of all the important bits
pub mod prelude {
    pub use super::buffer_pool::PoolConfig;
    pub use super::client::ClientState;
    pub use super::config::PicklebackConfig;
    pub use super::error::{Backpressure, PicklebackError};
    pub use super::received_message::ReceivedMessage;
    pub use super::BufHandle;
    pub use super::MessageId;
    pub use super::Pickleback;
    pub use super::PicklebackStats;
    pub use super::{PicklebackClient, PicklebackServer};
}

/// Things needed for integration tests
pub mod testing {
    pub use super::jitter_pipe::JitterPipeConfig;
    pub use super::test_utils::*;
    pub use assert_float_eq::*;
}

use protocol::*;

use crate::cursor::CursorExtras;

/// Identifies a packet - contains sequence number
#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy, PartialOrd, Ord, Default)]
pub(crate) struct PacketId(pub(crate) u16);

/// A Message, once sent, is identified by a MessageId. Use to check for acks later.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Default)]
pub struct MessageId(pub(crate) u16);

/// every N packets, we recaluclate packet loss. N is:
const PACKET_LOSS_RECALCULATE_INTERVAL: u16 = 100;

/// We use 5 bits from the message prefix byte to encode the channel id:
pub const MAX_CHANNELS: usize = 32;

/// With a fragment size of 1kB, this gives 1MB.
/// Arbitrary, but anything more than this should probably be transferred by other means.
pub const MAX_FRAGMENTS: usize = 1024;

/// Stats object
#[derive(Default, Debug, Clone)]
pub struct PicklebackStats {
    /// Number of packets sent
    pub packets_sent: u64,
    /// Number of packets received
    pub packets_received: u64,
    /// Number of packets acked
    pub packets_acked: u64,
    /// Number of stale packets received (and discarded)
    pub packets_stale: u64,
    /// Number of duplicate packets received (and discarded)
    pub packets_duplicate: u64,
    /// Number of calls to send_message.
    /// (Some of which will result in multiple fragmented messages being sent)
    pub message_sends: u64,
    /// Actual number of messages sent in packets.
    /// (Some of which will be fragments for larger messages)
    pub messages_sent: u64,
    /// Number of messages received.
    pub messages_received: u64,
}

/// Tracking outbound packets
#[derive(Debug, Clone)]
pub(crate) struct SentMeta {
    pub(crate) time: f64,
    pub(crate) acked: bool,
    pub(crate) size: usize,
    pub(crate) acked_up_to: PacketId,
}

impl SentMeta {
    pub fn new(time: f64, size: usize, acked_up_to: PacketId) -> Self {
        Self {
            time,
            size,
            acked: false,
            acked_up_to,
        }
    }
    #[allow(unused)]
    pub fn acked(&self) -> bool {
        self.acked
    }
    #[allow(unused)]
    pub fn size(&self) -> usize {
        // TODO: bandwidth budget stuff
        self.size
    }
}

impl Default for SentMeta {
    fn default() -> Self {
        Self {
            time: 0.0,
            size: 0,
            acked: false,
            acked_up_to: PacketId(0),
        }
    }
}

/// Tracking inbound packets
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ReceivedMeta {
    pub(crate) time: f64,
    pub(crate) size: usize,
}
impl ReceivedMeta {
    pub fn new(time: f64, size: usize) -> Self {
        Self { time, size }
    }
}

impl Default for ReceivedMeta {
    fn default() -> Self {
        Self { time: 0.0, size: 0 }
    }
}
