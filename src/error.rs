/// Packeteer specific errors
#[derive(Debug)]
pub enum PacketeerError {
    /// A wrapped io:Error
    Io(std::io::Error),
    /// Tried to insert into a sequence buffer, but sequence is too old for buffer
    SequenceTooOld,
    /// Parsing packet format error
    InvalidPacket,
    /// Packet arrived too late to be useful
    StalePacket,
    /// Already received this packet
    DuplicatePacket,
    /// Parsing message format error
    InvalidMessage,
}

impl std::fmt::Display for PacketeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PacketeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PacketeerError {
    fn from(err: std::io::Error) -> Self {
        PacketeerError::Io(err)
    }
}
