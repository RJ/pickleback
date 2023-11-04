#[derive(Debug)]
#[cfg_attr(feature = "cargo-clippy", allow(stutter))]
pub enum ReliableError {
    Io(std::io::Error),
    ExceededMaxPacketSize,
    SequenceBufferFull,
    PacketTooSmall,
    InvalidPacket,
    StalePacket,
    InvalidFragment,
}

impl std::fmt::Display for ReliableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid first item to double")
    }
}

// This is important for other errors to wrap this one.
impl std::error::Error for ReliableError {
    fn description(&self) -> &str {
        "invalid first item to double"
    }

    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
}

impl From<std::io::Error> for ReliableError {
    fn from(err: std::io::Error) -> Self {
        ReliableError::Io(err)
    }
}
