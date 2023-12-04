use crate::{
    ack_header::AckHeader,
    buffer_pool::{BufHandle, BufPool},
    cursor::{BufferLimitedWriter, CursorExtras},
    message::Message,
    PacketId, PacketeerError,
};
use byteorder::*;
use std::{
    io::{Cursor, Write},
    net::SocketAddr,
};

#[derive(Clone, Eq, PartialEq)]
pub struct AddressedPacket {
    pub address: SocketAddr,
    pub packet: BufHandle,
}

#[derive(Debug)]
pub(crate) enum ProtocolPacket {
    // 1 - C2S
    ConnectionRequest(ConnectionRequestPacket),
    // 2 - S2C
    ConnectionChallenge(ConnectionChallengePacket),
    // 3 - C2S
    ConnectionChallengeResponse(ConnectionChallengeResponsePacket),
    // 4 - S2C
    ConnectionDenied(ConnectionDeniedPacket),
    // 5 - Any
    Messages(MessagesPacket),
    // 6 - Any (server can kick you, or you can gracefully exit)
    Disconnect(DisconnectPacket),
    // 7 - Any
    KeepAlive(KeepAlivePacket),
}

#[derive(Debug, Copy, Clone)]
#[repr(u8)]
pub(crate) enum PacketType {
    ConnectionRequest = 1,
    ConnectionChallenge = 2,
    ConnectionChallengeResponse = 3,
    ConnectionDenied = 4,
    Messages = 5,
    Disconnect = 6,
    KeepAlive = 7,
}

impl TryFrom<u8> for PacketType {
    type Error = PacketeerError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(PacketType::ConnectionRequest),
            2 => Ok(PacketType::ConnectionChallenge),
            3 => Ok(PacketType::ConnectionChallengeResponse),
            4 => Ok(PacketType::ConnectionDenied),
            5 => Ok(PacketType::Messages),
            6 => Ok(PacketType::Disconnect),
            7 => Ok(PacketType::KeepAlive),
            _ => Err(PacketeerError::InvalidPacket),
        }
    }
}

impl From<&ProtocolPacket> for PacketType {
    fn from(val: &ProtocolPacket) -> Self {
        match val {
            ProtocolPacket::ConnectionRequest(_) => PacketType::ConnectionRequest,
            ProtocolPacket::ConnectionChallenge(_) => PacketType::ConnectionChallenge,
            ProtocolPacket::ConnectionChallengeResponse(_) => {
                PacketType::ConnectionChallengeResponse
            }
            ProtocolPacket::ConnectionDenied(_) => PacketType::ConnectionDenied,
            ProtocolPacket::Messages(_) => PacketType::Messages,
            ProtocolPacket::Disconnect(_) => PacketType::Disconnect,
            ProtocolPacket::KeepAlive(_) => PacketType::KeepAlive,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ProtocolPacketHeader {
    pub(crate) packet_type: PacketType,
    pub(crate) id: PacketId,
    pub(crate) ack_header: Option<AckHeader>,
    pub(crate) xor_salt: Option<u64>,
}

impl ProtocolPacketHeader {
    pub(crate) fn new(
        id: PacketId,
        ack_iter: impl Iterator<Item = (u16, bool)>,
        num_acks: u16,
        packet_type: PacketType,
        xor_salt: Option<u64>,
    ) -> Result<Self, PacketeerError> {
        if num_acks == 0 {
            return Self::new_no_acks(id, packet_type, xor_salt);
        }
        let ack_header = AckHeader::from_ack_iter(num_acks, ack_iter)?;
        Ok(Self {
            packet_type,
            id,
            ack_header: Some(ack_header),
            xor_salt,
        })
    }
    pub(crate) fn new_no_acks(
        id: PacketId,
        packet_type: PacketType,
        xor_salt: Option<u64>,
    ) -> Result<Self, PacketeerError> {
        Ok(Self {
            packet_type,
            id,
            ack_header: None,
            xor_salt,
        })
    }

    pub(crate) fn id(&self) -> PacketId {
        self.id
    }
    pub(crate) fn ack_id(&self) -> Option<PacketId> {
        self.ack_header.map(|header| header.ack_id())
    }
    pub(crate) fn acks(&self) -> Option<impl Iterator<Item = (u16, bool)>> {
        self.ack_header.map(|header| header.into_iter())
    }

    #[allow(unused)]
    pub(crate) fn ack_header(&self) -> Option<&AckHeader> {
        self.ack_header.as_ref()
    }

    pub(crate) fn size(&self) -> usize {
        1 + // prefix byte
        2 + // packet sequence id
        8 + // xor_salt
        self.ack_header.map_or(0, |header| header.size())
    }

    pub(crate) fn write(&self, mut writer: &mut impl Write) -> Result<(), PacketeerError> {
        let mut prefix_byte = self.packet_type as u8;
        if self.ack_header.is_some() {
            // highest bit denotes presence of ack header
            prefix_byte |= 0b1000_0000;
        }
        writer.write_u8(prefix_byte)?;
        writer.write_u16::<NetworkEndian>(self.id.0)?;
        // packet types that have the xor_salt:
        // 3 = ConnectionChallengeResponse
        // 5 = Messages
        // 6 = Disconnect
        // 7 = KeepAlive
        // just writing zeros if absent anyway, for convenience.
        // only low-volume messages don't need the xor salt, so it's not really wasting bandwidth.
        writer.write_u64::<NetworkEndian>(self.xor_salt.unwrap_or(0))?;

        if let Some(ack_header) = self.ack_header {
            ack_header.write(&mut writer)?;
        }
        Ok(())
    }
    pub(crate) fn parse(reader: &mut Cursor<&[u8]>) -> Result<Self, PacketeerError> {
        let prefix_byte = reader.read_u8()?;
        let ack_header_present = prefix_byte & 0b1000_0000 != 0;
        let Ok(packet_type) = PacketType::try_from(prefix_byte & 0b0111_1111) else {
            log::error!("prefix byte packet type invalid");
            return Err(PacketeerError::InvalidPacket);
        };
        let id = PacketId(reader.read_u16::<NetworkEndian>()?);
        let xor_salt = reader.read_u64::<NetworkEndian>()?;
        let xor_salt = if xor_salt > 0 { Some(xor_salt) } else { None };
        let ack_header = if ack_header_present {
            Some(AckHeader::parse(reader)?)
        } else {
            None
        };
        Ok(Self {
            packet_type,
            id,
            ack_header,
            xor_salt,
        })
    }
}

// C2S
#[derive(Debug)]
pub(crate) struct ConnectionRequestPacket {
    pub(crate) header: ProtocolPacketHeader,
    pub(crate) client_salt: u64,
    pub(crate) protocol_version: u64,
    // TODO protocol version, so server can reject unsupported versions.
}

// S2C
#[derive(Debug)]
pub(crate) struct ConnectionChallengePacket {
    pub(crate) header: ProtocolPacketHeader,
    pub(crate) client_salt: u64,
    pub(crate) server_salt: u64,
}

// C2S
#[derive(Debug)]
pub(crate) struct ConnectionChallengeResponsePacket {
    pub(crate) header: ProtocolPacketHeader,
}

// S2C
#[derive(Debug)]
pub(crate) struct ConnectionDeniedPacket {
    pub(crate) header: ProtocolPacketHeader,
    pub(crate) reason: u8,
}

// Bidirectional
pub(crate) struct MessagesPacket {
    pub(crate) header: ProtocolPacketHeader,
    pub(crate) messages: Vec<Message>, // Box<dyn Iterator<Item = Result<Message, PacketeerError>> + 'a>, //Vec<Message>,
}

impl std::fmt::Debug for MessagesPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MessagesPacket[header: {:?}]]", self.header)
    }
}

// Bi?
#[derive(Debug)]
pub(crate) struct DisconnectPacket {
    pub(crate) header: ProtocolPacketHeader,
}

// Bi?
#[derive(Debug)]
pub(crate) struct KeepAlivePacket {
    pub(crate) header: ProtocolPacketHeader,
    pub(crate) client_index: u32,
}

fn write_zero_bytes<W: Write>(writer: &mut W, num_bytes: usize) -> std::io::Result<()> {
    let buffer = vec![0u8; num_bytes];
    writer.write_all(&buffer)
}

pub(crate) fn write_packet(
    pool: &BufPool,
    packet: ProtocolPacket,
) -> Result<BufHandle, PacketeerError> {
    let max_packet_size = 1180; // TODO config here
    let mut buffer = pool.get_buffer(max_packet_size);
    let mut writer = BufferLimitedWriter::new(Cursor::new(&mut buffer), max_packet_size);

    match packet {
        ProtocolPacket::KeepAlive(KeepAlivePacket {
            header,
            client_index,
        }) => {
            header.write(&mut writer)?;
            writer.write_u32::<NetworkEndian>(client_index)?;
        }
        ProtocolPacket::ConnectionRequest(ConnectionRequestPacket {
            header,
            client_salt,
            protocol_version,
        }) => {
            header.write(&mut writer)?;
            writer.write_u64::<NetworkEndian>(client_salt)?;
            writer.write_u64::<NetworkEndian>(protocol_version)?;
            write_zero_bytes(&mut writer, 500)?;
        }
        ProtocolPacket::ConnectionChallenge(ConnectionChallengePacket {
            header,
            client_salt,
            server_salt,
        }) => {
            header.write(&mut writer)?;
            writer.write_u64::<NetworkEndian>(client_salt)?;
            writer.write_u64::<NetworkEndian>(server_salt)?;
            write_zero_bytes(&mut writer, 500)?;
        }
        ProtocolPacket::ConnectionChallengeResponse(ConnectionChallengeResponsePacket {
            header,
        }) => {
            assert!(header.xor_salt.is_some());
            header.write(&mut writer)?;
            write_zero_bytes(&mut writer, 500)?;
        }
        ProtocolPacket::ConnectionDenied(ConnectionDeniedPacket { header, reason }) => {
            header.write(&mut writer)?;
            writer.write_u8(reason)?;
        }
        ProtocolPacket::Messages(MessagesPacket { .. }) => {
            panic!("written elsewhere");
            // assert!(header.xor_salt.is_some());
            // header.write(&mut writer)?;
            // while let Some(msg) = messages.next() {
            //     writer.write_all(msg?.as_slice())?;
            // }
        }
        ProtocolPacket::Disconnect(DisconnectPacket { header }) => {
            assert!(header.xor_salt.is_some());
            header.write(&mut writer)?;
        }
    }
    Ok(buffer)
}

pub(crate) fn read_packet(
    reader: &mut Cursor<&[u8]>,
    pool: &BufPool,
) -> Result<ProtocolPacket, PacketeerError> {
    let header = ProtocolPacketHeader::parse(reader)?;
    match header.packet_type {
        PacketType::KeepAlive => {
            let c = KeepAlivePacket {
                header,
                client_index: reader.read_u32::<NetworkEndian>()?,
            };
            Ok(ProtocolPacket::KeepAlive(c))
        }
        PacketType::ConnectionRequest => {
            let c = ConnectionRequestPacket {
                header,
                client_salt: reader.read_u64::<NetworkEndian>()?,
                protocol_version: reader.read_u64::<NetworkEndian>()?,
            };
            if reader.remaining() != 500 {
                log::warn!("Invalid remaining len for ConnectionRequestPacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionRequest(c))
        }
        PacketType::ConnectionChallenge => {
            let c = ConnectionChallengePacket {
                header,
                client_salt: reader.read_u64::<NetworkEndian>()?,
                server_salt: reader.read_u64::<NetworkEndian>()?,
            };
            if reader.remaining() != 500 {
                log::warn!("Invalid remaining len for ConnectionChallengePacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionChallenge(c))
        }
        PacketType::ConnectionChallengeResponse => {
            let c = ConnectionChallengeResponsePacket { header };
            if reader.remaining() != 500 {
                log::warn!("Invalid remaining len for ConnectionChallengeResponsePacket");
                return Err(PacketeerError::InvalidPacket);
            }
            Ok(ProtocolPacket::ConnectionChallengeResponse(c))
        }
        PacketType::ConnectionDenied => {
            let c = ConnectionDeniedPacket {
                header,
                reason: reader.read_u8()?,
            };
            Ok(ProtocolPacket::ConnectionDenied(c))
        }
        PacketType::Messages => {
            let mut messages = Vec::new();
            while reader.remaining() > 0 {
                // as long as there are bytes left to read, we should only find whole messages
                messages.push(Message::parse(pool, reader)?);
            }
            let c = MessagesPacket { header, messages };
            Ok(ProtocolPacket::Messages(c))
        }
        PacketType::Disconnect => {
            let c = DisconnectPacket { header };
            Ok(ProtocolPacket::Disconnect(c))
        }
    }
}

// struct MessageIterator<'a> {
//     reader: &'a mut Cursor<&'a [u8]>,
//     pool: &'a BufPool,
// }

// impl<'a> Iterator for MessageIterator<'a> {
//     type Item = Result<Message, PacketeerError>;

//     fn next(&mut self) -> Option<Self::Item> {
//         if self.reader.remaining() > 0 {
//             Some(Message::parse(self.pool, self.reader))
//         } else {
//             None
//         }
//     }
// }
