use crate::{
    ack_header::AckHeader, buffer_pool::BufPool, cursor::CursorExtras, message::Message, PacketId,
    PacketeerError,
};
use byteorder::*;
use std::io::{BufRead, Cursor, Write};

#[derive(Debug)]
pub(crate) enum ProtocolPacket {
    // 1
    ConnectionRequest(ConnectionRequestPacket),
    // 2
    ConnectionChallenge(ConnectionChallengePacket),
    // 3
    ConnectionChallengeResponse(ConnectionChallengeResponsePacket),
    // 4
    ConnectionDenied(ConnectionDeniedPacket),
    // 5
    Messages(MessagesPacket),
    // 6
    Disconnect(DisconnectPacket),
}

impl Into<u8> for &ProtocolPacket {
    fn into(self) -> u8 {
        match self {
            ProtocolPacket::ConnectionRequest(_) => 1,
            ProtocolPacket::ConnectionChallenge(_) => 2,
            ProtocolPacket::ConnectionChallengeResponse(_) => 3,
            ProtocolPacket::ConnectionDenied(_) => 4,
            ProtocolPacket::Messages(_) => 5,
            ProtocolPacket::Disconnect(_) => 6,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProtocolPacketHeader {
    pub(crate) prefix_byte: u8,
    pub(crate) sequence: u16,
}

#[derive(Debug)]
pub(crate) struct ConnectionRequestPacket {
    pub(crate) client_salt: u64,
}

#[derive(Debug)]
pub(crate) struct ConnectionChallengePacket {
    pub(crate) client_salt: u64,
    pub(crate) server_salt: u64,
}

#[derive(Debug)]
pub(crate) struct ConnectionChallengeResponsePacket {
    pub(crate) client_salt_xor_server_salt: u64,
}

#[derive(Debug)]
pub(crate) struct ConnectionDeniedPacket {
    pub(crate) reason: u8,
}

#[derive(Debug)]
pub(crate) struct MessagesPacket {
    pub(crate) client_salt_xor_server_salt: u64,
    pub(crate) ack_header: AckHeader,
    pub(crate) messages: Vec<Message>,
}

#[derive(Debug)]
pub(crate) struct DisconnectPacket {
    pub(crate) client_salt_xor_server_salt: u64,
}

fn write_zero_bytes<W: Write>(writer: &mut W, num_bytes: usize) -> std::io::Result<()> {
    let buffer = vec![0u8; num_bytes];
    writer.write_all(&buffer)
}

pub(crate) fn write_packet(
    sequence_id: PacketId,
    packet: &ProtocolPacket,
    mut writer: &mut impl Write,
) -> Result<(), PacketeerError> {
    let prefix_byte: u8 = packet.into();
    // packet header
    writer.write_u8(prefix_byte)?;
    writer.write_u16::<NetworkEndian>(sequence_id.0)?;
    // packet-specific writing
    match packet {
        ProtocolPacket::ConnectionRequest(ConnectionRequestPacket { client_salt }) => {
            writer.write_u64::<NetworkEndian>(*client_salt)?;
            write_zero_bytes(writer, 1000 - 8)?;
        }
        ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
            client_salt,
            server_salt,
        }) => {
            writer.write_u64::<NetworkEndian>(*client_salt)?;
            writer.write_u64::<NetworkEndian>(*server_salt)?;
            write_zero_bytes(writer, 1000 - 8 - 8)?;
        }
        ProtocolPacket::ConnectionChallengeResponse(ConnectionChallengeResponsePacket {
            client_salt_xor_server_salt,
        }) => {
            writer.write_u64::<NetworkEndian>(*client_salt_xor_server_salt)?;
            write_zero_bytes(writer, 1000 - 8)?;
        }
        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { reason }) => {
            writer.write_u8(*reason)?;
        }
        ProtocolPacket::Messages(MessagesPacket {
            client_salt_xor_server_salt,
            ack_header,
            messages,
        }) => {
            writer.write_u64::<NetworkEndian>(*client_salt_xor_server_salt)?;
            ack_header.write(&mut writer)?;
            for msg in messages {
                writer.write_all(msg.as_slice())?;
            }
        }
        ProtocolPacket::Disconnect(DisconnectPacket {
            client_salt_xor_server_salt,
        }) => {
            writer.write_u64::<NetworkEndian>(*client_salt_xor_server_salt)?;
        }
    }
    Ok(())
}

pub(crate) fn read_packet(
    reader: &mut Cursor<&[u8]>,
    pool: &BufPool,
) -> Result<ProtocolPacket, PacketeerError> {
    let prefix_byte = reader.read_u8()?;
    // lowest 4 bits give packet type
    let packet_type: u8 = prefix_byte & 0b0000_1111;
    match packet_type {
        // ConnectionRequestPacket
        1 => {
            let c = ConnectionRequestPacket {
                client_salt: reader.read_u64::<NetworkEndian>()?,
            };
            if reader.remaining() != (1000 - 8) {
                log::warn!("Invalid remaining len for ConnectionRequestPacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionRequest(c))
        }
        // ConnectionChallengePacket
        2 => {
            let c = ConnectionChallengePacket {
                client_salt: reader.read_u64::<NetworkEndian>()?,
                server_salt: reader.read_u64::<NetworkEndian>()?,
            };
            if reader.remaining() != (1000 - 8 - 8) {
                log::warn!("Invalid remaining len for ConnectionChallengePacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionChallenge(c))
        }
        // ConnectionChallengeResponsePacket
        3 => {
            let c = ConnectionChallengeResponsePacket {
                client_salt_xor_server_salt: reader.read_u64::<NetworkEndian>()?,
            };
            if reader.remaining() != (1000 - 8) {
                log::warn!("Invalid remaining len for ConnectionChallengeResponsePacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionChallengeResponse(c))
        }
        // ConnectionDeniedPacket
        4 => {
            let c = ConnectionDeniedPacket {
                reason: reader.read_u8()?,
            };
            Ok(ProtocolPacket::ConnectionDenied(c))
        }
        // Messages
        5 => {
            let client_salt_xor_server_salt = reader.read_u64::<NetworkEndian>()?;
            let ack_header = AckHeader::parse(reader)?;
            let mut messages = Vec::new();
            while reader.remaining() > 0 {
                // as long as there are bytes left to read, we should only find whole messages
                messages.push(Message::parse(pool, reader)?);
            }
            let c = MessagesPacket {
                client_salt_xor_server_salt,
                ack_header,
                messages,
            };
            // payload remains unread by cursor, caller will do that
            Ok(ProtocolPacket::Messages(c))
        }
        // Disconnect
        6 => {
            let c = DisconnectPacket {
                client_salt_xor_server_salt: reader.read_u64::<NetworkEndian>()?,
            };
            Ok(ProtocolPacket::Disconnect(c))
        }
        other => {
            log::error!("Invalid packet type: {other}");
            Err(PacketeerError::InvalidPacket)
        }
    }
}
