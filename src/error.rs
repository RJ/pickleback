#[derive(Debug)]
pub enum PacketeerError {
    Io(std::io::Error),
    ExceededMaxPacketSize,
    SequenceBufferFull,
    // SequenceTooOld,
    PacketTooSmall,
    InvalidPacket,
    StalePacket,
    DuplicatePacket,
    InvalidMessage,
}

impl std::fmt::Display for PacketeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid first item to double")
    }
}

// This is important for other errors to wrap this one.
impl std::error::Error for PacketeerError {
    fn description(&self) -> &str {
        "invalid first item to double"
    }

    fn cause(&self) -> Option<&dyn std::error::Error> {
        None
    }
}

impl From<std::io::Error> for PacketeerError {
    fn from(err: std::io::Error) -> Self {
        PacketeerError::Io(err)
    }
}
