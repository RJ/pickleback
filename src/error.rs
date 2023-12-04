/// Pickleback specific errors
#[derive(Debug)]
pub enum PicklebackError {
    /// A wrapped io:Error
    Io(std::io::Error),
    /// Tried to insert into a sequence buffer, but sequence is too old for buffer
    SequenceTooOld,
    /// Parsing packet format error
    InvalidPacket,
    /// Packet arrived too late to be useful / out of sequence
    StalePacket,
    /// Already received this packet
    DuplicatePacket,
    /// Parsing message format error
    InvalidMessage,
    /// Can't send due to backpressure (num of unacks, bandwidth, etc)
    Backpressure(Backpressure),
    /// Payload exceeds max_message_size from config
    PayloadTooBig,
    /// Channel doesn't exist
    NoSuchChannel,
}

/// Reasons for not being able to send due to backpressure
#[derive(Debug)]
pub enum Backpressure {
    /// There are too many unacked packets outstanding, so we can't send any more
    /// until we receive some acks.
    TooManyPending,
    // TODO: bandwidth related reasons, once implemented.
}

impl std::fmt::Display for PicklebackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PicklebackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PicklebackError {
    fn from(err: std::io::Error) -> Self {
        PicklebackError::Io(err)
    }
}
