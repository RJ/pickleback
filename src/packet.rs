///
/// ## Packet Anatomy
///
/// ### PrefixByte
///
/// Is a `u8` at the start of each packet.
///
/// | bits       |              | description                                                                            |
/// | ---------- | ------------ | -------------------------------------------------------------------------------------- |
/// | `-------X` | `=  0`       | X = 0  = regular packet containing messages                                            |
/// | `---XXXX-` | `<< 1,2,3,4` | denotes size of ack mask. each of 4 bits meaning another byte of ack mask data follows |
/// | `--X-----` | `<<5`        | sequence difference bit                                                                |
/// | `XX------` | `<<6,7`      | currently unused                                                                       |
///
/// ### PacketHeader
///
/// | bytes              | type             | description                                                                                                     |
/// | ------------------ | ---------------- | --------------------------------------------------------------------------------------------------------------- |
/// | 1                  | `u8`             | `PrefixByte`                                                                                                    |
/// | 2,3                | `u16_le`         | sequence                                                                                                        |
/// | 4 or 4,5           | `u8` or `u16_le` | sequence_difference, depending on sequnce difference bit in PrefixByte. <br> `sequence` - `last_acked_sequence` |
/// | 5,6,7,8 or 6,7,8,9 | `u8` x 1-4       | ack bits mask                                                                                                   |
///
use crate::{PacketId, PacketeerError};
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use log::*;
use std::{
    io::{Cursor, Write},
    num::Wrapping,
};

#[derive(Clone, PartialEq, PartialOrd, Debug, Default)]
pub struct PacketHeader {
    id: PacketId,
    ack_id: PacketId,
    ack_bits: u32,
}

impl PacketHeader {
    pub fn new(id: PacketId, ack_id: PacketId, ack_bits: u32) -> Self {
        Self {
            id,
            ack_id,
            ack_bits,
        }
    }

    pub fn id(&self) -> PacketId {
        self.id
    }
    pub fn ack_id(&self) -> PacketId {
        self.ack_id
    }
    pub fn ack_bits(&self) -> u32 {
        self.ack_bits
    }

    // pub fn max_possible_size() -> usize {
    //     9
    // }

    pub fn size(&self) -> usize {
        let mut size: usize = 3;

        let mut sequence_difference = i32::from((Wrapping(self.id.0) - Wrapping(self.ack_id.0)).0);
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
    pub fn write(&self, writer: &mut impl Write) -> Result<(), PacketeerError> {
        // if writer.remaining_mut() < 9 {
        //     panic!("::write given too-small BytesMut");
        // }

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

        let mut sequence_difference = i32::from((Wrapping(self.id.0) - Wrapping(self.ack_id.0)).0);
        if sequence_difference < 0 {
            sequence_difference = (Wrapping(sequence_difference) + Wrapping(65536)).0;
        }

        if sequence_difference <= 255 {
            prefix_byte |= 1 << 5;
        }

        writer.write_u8(prefix_byte)?; // 1
        writer.write_u16::<NetworkEndian>(self.id.0)?; // +2 = 3

        if sequence_difference <= 255 {
            writer.write_u8(sequence_difference as u8)?; // +1 = 4
        } else {
            writer.write_u16::<NetworkEndian>(self.ack_id.0)?; // or +2 = 5
        }
        // +4:
        if (self.ack_bits & 0x0000_00FF) != 0x0000_00FF {
            writer.write_u8((self.ack_bits & 0x0000_00FF) as u8)?;
        }

        if (self.ack_bits & 0x0000_FF00) != 0x0000_FF00 {
            writer.write_u8(((self.ack_bits & 0x0000_FF00) >> 8) as u8)?;
        }

        if (self.ack_bits & 0x00FF_0000) != 0x00FF_0000 {
            writer.write_u8(((self.ack_bits & 0x00FF_0000) >> 16) as u8)?;
        }

        if (self.ack_bits & 0xFF00_0000) != 0xFF00_0000 {
            writer.write_u8(((self.ack_bits & 0xFF00_0000) >> 24) as u8)?;
        }

        Ok(())
    }

    pub fn parse(reader: &mut Cursor<&[u8]>) -> Result<Self, PacketeerError> {
        let prefix_byte = reader.read_u8()?;

        if prefix_byte & 1 != 0 {
            error!("prefix byte does not indicate regular packet");
            return Err(PacketeerError::InvalidPacket);
        }

        let mut ack_bits: u32 = 0xFFFF_FFFF;
        let id = PacketId(reader.read_u16::<NetworkEndian>()?);
        // ack is greatest seqno seen?
        let ack_id = if prefix_byte & (1 << 5) != 0 {
            let sequence_difference = reader.read_u8()?;
            PacketId((Wrapping(id.0) - Wrapping(u16::from(sequence_difference))).0)
        } else {
            PacketId(reader.read_u16::<NetworkEndian>()?)
        };

        // let mut expected_ack_bytes: usize = 0;
        // for i in 1..5 {
        //     if prefix_byte & (1 << i) != 0 {
        //         expected_ack_bytes += 1;
        //     }
        // }
        // if reader.remaining() < expected_ack_bytes {
        //     error!("Packet too small for packet header (4) expected_ack_bytes: {expected_ack_bytes} remaining: {}", reader.remaining());
        //     return Err(PacketeerError::InvalidPacket);
        // }

        if prefix_byte & (1 << 1) != 0 {
            ack_bits &= 0xFFFF_FF00;
            ack_bits |= u32::from(reader.read_u8()?);
        }

        if prefix_byte & (1 << 2) != 0 {
            ack_bits &= 0xFFFF_00FF;
            ack_bits |= u32::from(reader.read_u8()?) << 8;
        }

        if prefix_byte & (1 << 3) != 0 {
            ack_bits &= 0xFF00_FFFF;
            ack_bits |= u32::from(reader.read_u8()?) << 16;
        }

        if prefix_byte & (1 << 4) != 0 {
            ack_bits &= 0x00FF_FFFF;
            ack_bits |= u32::from(reader.read_u8()?) << 24;
        }

        Ok(Self {
            id,
            ack_id,
            ack_bits,
        })
    }
}
