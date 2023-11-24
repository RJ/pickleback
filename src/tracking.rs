use crate::PacketId;

#[derive(Debug, Clone)]
pub(crate) struct SentData {
    pub(crate) time: f64,
    pub(crate) acked: bool,
    pub(crate) size: usize,
    pub(crate) acked_up_to: PacketId,
}

impl SentData {
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
        self.size
    }
}

impl Default for SentData {
    fn default() -> Self {
        Self {
            time: 0.0,
            size: 0,
            acked: false,
            acked_up_to: PacketId(0),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct RecvData {
    pub(crate) time: f64,
    pub(crate) size: usize,
}
impl RecvData {
    pub fn new(time: f64, size: usize) -> Self {
        Self { time, size }
    }
}

impl Default for RecvData {
    fn default() -> Self {
        Self { time: 0.0, size: 0 }
    }
}

/// Stats object
#[derive(Default, Debug, Clone)]
pub struct PacketeerStats {
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
