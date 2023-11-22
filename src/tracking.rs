#[derive(Debug, Clone)]
pub(crate) struct SentData {
    pub(crate) time: f64,
    pub(crate) acked: bool,
    pub(crate) size: usize,
}

impl SentData {
    pub fn new(time: f64, size: usize) -> Self {
        Self {
            time,
            size,
            acked: false,
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

#[derive(Default, Debug, Clone)]
pub struct PacketeerStats {
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_acked: u64,
    pub packets_stale: u64,
    pub packets_duplicate: u64,
    /// number of calls to send_message. Some of which will result in multiple fragmented
    /// messages being sent.
    pub message_sends: u64,
    /// actual number of messages sent in packets. Some of which will be fragments for larger messages.
    pub messages_sent: u64,
    pub messages_received: u64,
}
