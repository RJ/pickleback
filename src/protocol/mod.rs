mod ack_header;
mod message;
mod packets;
pub(crate) const PROTOCOL_VERSION: u64 = 1;

pub(crate) use ack_header::*;
pub(crate) use message::*;
pub(crate) use packets::*;

use crate::prelude::PicklebackError;

#[derive(Debug, PartialEq, Clone)]
#[repr(u8)]
pub enum DisconnectReason {
    Normal = 0,
    ProtocolMismatch = 1,
    HandshakeTimeout = 2,
    TimedOut = 3,
    ServerFull = 4,
}

impl std::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectReason::Normal => write!(f, "Disconnected cleanly."),
            DisconnectReason::HandshakeTimeout => write!(f, "Handshake took too long"),
            DisconnectReason::ProtocolMismatch => {
                write!(f, "Protocol version too old - check for an update?")
            }
            DisconnectReason::TimedOut => write!(f, "Timed out"),
            DisconnectReason::ServerFull => write!(f, "Server Full"),
        }
    }
}

impl TryFrom<u8> for DisconnectReason {
    type Error = PicklebackError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(DisconnectReason::Normal),
            1 => Ok(DisconnectReason::ProtocolMismatch),
            2 => Ok(DisconnectReason::HandshakeTimeout),
            3 => Ok(DisconnectReason::TimedOut),
            4 => Ok(DisconnectReason::ServerFull),
            _ => Err(PicklebackError::InvalidPacket),
        }
    }
}
