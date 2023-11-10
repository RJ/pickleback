use crate::ReliableError;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::*;
use std::num::Wrapping;

pub trait HeaderParser {
    type T;

    fn max_possible_size() -> usize;

    fn size(&self) -> usize;
    fn write(&self, writer: &mut BytesMut) -> Result<(), ReliableError>;
    fn parse(reader: &mut Bytes) -> Result<Self::T, ReliableError>;
}

#[derive(Clone, PartialEq, PartialOrd, Debug, Default)]
pub struct PacketHeader {
    sequence: u16,
    ack: u16,
    ack_bits: u32,
}

impl PacketHeader {
    pub fn new(sequence: u16, ack: u16, ack_bits: u32) -> Self {
        Self {
            sequence,
            ack,
            ack_bits,
        }
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }
    pub fn ack(&self) -> u16 {
        self.ack
    }
    pub fn ack_bits(&self) -> u32 {
        self.ack_bits
    }
}

impl HeaderParser for PacketHeader {
    type T = Self;
    fn max_possible_size() -> usize {
        9
    }
    fn size(&self) -> usize {
        let mut size: usize = 3;

        let mut sequence_difference = i32::from((Wrapping(self.sequence) - Wrapping(self.ack)).0);
        if sequence_difference < 0 {
            sequence_difference = (Wrapping(sequence_difference) + Wrapping(65536)).0;
        }

        if sequence_difference <= 255 {
            size += 1;
        } else {
            size += 2;
        }

        if (self.ack_bits & 0x0000_00FF) != 0x0000_00FF {
            size += 1;
        }
        if (self.ack_bits & 0x0000_FF00) != 0x0000_FF00 {
            size += 1;
        }

        if (self.ack_bits & 0x00FF_0000) != 0x00FF_0000 {
            size += 1;
        }
        if (self.ack_bits & 0xFF00_0000) != 0xFF00_0000 {
            size += 1;
        }

        size
    }

    // max of 9 bytes?
    fn write(&self, writer: &mut BytesMut) -> Result<(), ReliableError> {
        if writer.remaining_mut() < 9 {
            panic!("::write given too-small BytesMut");
        }

        let mut prefix_byte = 0;

        if (self.ack_bits & 0x0000_00FF) != 0x0000_00FF {
            prefix_byte |= 1 << 1;
        }

        if (self.ack_bits & 0x0000_FF00) != 0x0000_FF00 {
            prefix_byte |= 1 << 2;
        }

        if (self.ack_bits & 0x00FF_0000) != 0x00FF_0000 {
            prefix_byte |= 1 << 3;
        }

        if (self.ack_bits & 0xFF00_0000) != 0xFF00_0000 {
            prefix_byte |= 1 << 4;
        }

        let mut sequence_difference = i32::from((Wrapping(self.sequence) - Wrapping(self.ack)).0);
        if sequence_difference < 0 {
            sequence_difference = (Wrapping(sequence_difference) + Wrapping(65536)).0;
        }

        if sequence_difference <= 255 {
            prefix_byte |= 1 << 5;
        }

        writer.put_u8(prefix_byte); // 1
        writer.put_u16_le(self.sequence); // +2 = 3

        if sequence_difference <= 255 {
            writer.put_u8(sequence_difference as u8); // +1 = 4
        } else {
            writer.put_u16_le(self.ack); // or +2 = 5
        }
        // +4:
        if (self.ack_bits & 0x0000_00FF) != 0x0000_00FF {
            writer.put_u8((self.ack_bits & 0x0000_00FF) as u8);
        }

        if (self.ack_bits & 0x0000_FF00) != 0x0000_FF00 {
            writer.put_u8(((self.ack_bits & 0x0000_FF00) >> 8) as u8);
        }

        if (self.ack_bits & 0x00FF_0000) != 0x00FF_0000 {
            writer.put_u8(((self.ack_bits & 0x00FF_0000) >> 16) as u8);
        }

        if (self.ack_bits & 0xFF00_0000) != 0xFF00_0000 {
            writer.put_u8(((self.ack_bits & 0xFF00_0000) >> 24) as u8);
        }

        Ok(())
    }

    fn parse(mut reader: &mut Bytes) -> Result<Self, ReliableError> {
        if reader.remaining() < 3 {
            error!("Packet too small for packet header (1)");
            return Err(ReliableError::PacketTooSmall);
        }
        let prefix_byte = reader.get_u8();

        if prefix_byte & 1 != 0 {
            error!("prefix byte does not indicate regular packet");
            return Err(ReliableError::InvalidPacket);
        }

        let ack: u16;
        let mut ack_bits: u32 = 0xFFFF_FFFF;
        let sequence = reader.get_u16_le();
        // ack is greatest seqno seen?
        if prefix_byte & (1 << 5) != 0 {
            if reader.remaining() < 4 {
                error!("Packet too small for packet header (2)");
                return Err(ReliableError::InvalidPacket);
            }
            let sequence_difference = reader.get_u8();
            ack = (Wrapping(sequence) - Wrapping(u16::from(sequence_difference))).0;
        } else {
            if reader.remaining() < 5 {
                error!("Packet too small for packet header (3)");
                return Err(ReliableError::InvalidPacket);
            }
            ack = reader.get_u16_le();
        }

        let mut expected_bytes: usize = 0;
        for i in 1..5 {
            if prefix_byte & (1 << i) != 0 {
                expected_bytes += 1;
            }
        }
        if reader.remaining() < expected_bytes {
            error!("Packet too small for packet header (4)");
            return Err(ReliableError::InvalidPacket);
        }

        if prefix_byte & (1 << 1) != 0 {
            ack_bits &= 0xFFFF_FF00;
            ack_bits |= u32::from(reader.get_u8());
        }

        if prefix_byte & (1 << 2) != 0 {
            ack_bits &= 0xFFFF_00FF;
            ack_bits |= u32::from(reader.get_u8()) << 8;
        }

        if prefix_byte & (1 << 3) != 0 {
            ack_bits &= 0xFF00_FFFF;
            ack_bits |= u32::from(reader.get_u8()) << 16;
        }

        if prefix_byte & (1 << 4) != 0 {
            ack_bits &= 0x00FF_FFFF;
            ack_bits |= u32::from(reader.get_u8()) << 24;
        }

        Ok(Self {
            sequence,
            ack,
            ack_bits,
        })
    }
}
