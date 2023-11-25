///
/// ## Packet Anatomy
///
///  TODO maybe use bitflags or bitmask-enum crate for prefixbytes
use crate::{ack_header::AckHeader, PacketId, PacketeerError};
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use log::*;
use std::{
    io::{Cursor, Write},
    iter,
    num::Wrapping,
};

#[derive(Debug)]
pub struct PacketHeader {
    id: PacketId,
    ack_header: Option<AckHeader>,
    // ack_id: PacketId,
    // ack_bits: u32,
}

// pub(crate) struct AckWindow {
//     pub(crate) ack_id: PacketId,
//     pub(crate)
// }

impl PacketHeader {
    pub fn new_no_acks(id: PacketId) -> Result<Self, PacketeerError> {
        Ok(Self {
            id,
            ack_header: None,
        })
    }
    pub fn new(
        id: PacketId,
        ack_iter: impl Iterator<Item = (u16, bool)>,
        num_acks_required: u16,
    ) -> Result<Self, PacketeerError> {
        assert!(num_acks_required > 0, "num acks required must be > 0");
        let ack_header = Some(AckHeader::from_ack_iter(num_acks_required, ack_iter)?);
        Ok(Self { id, ack_header })
    }
    /*
    fn make_ack_bits(ack_iter: impl Iterator<Item = (u16, bool)>) -> (PacketId, u32) {
        let mut peekable_iter = ack_iter.peekable();
        // peek the first id, which is always the most recent ack.
        let (ack_id, _) = peekable_iter.peek().expect("ack_bits must be non-empty");
        let ack_id = *ack_id;
        let mut mask: u32 = 1;
        let mut ack_bits: u32 = 0;
        for (bit_count, (_sequence, is_acked)) in peekable_iter.enumerate() {
            if bit_count == 32 {
                panic!("ack bit count exceeded");
            }
            if is_acked {
                ack_bits |= mask;
                info!("Acking {_sequence}");
            }
            mask <<= 1;
        }
        info!("ACK_ID = {ack_id} ACK_BITS -> {ack_bits:#032b}");
        (PacketId(ack_id), ack_bits)
    }
    */

    pub fn id(&self) -> PacketId {
        self.id
    }
    pub fn ack_id(&self) -> Option<PacketId> {
        self.ack_header.map(|header| header.ack_id())
    }
    pub fn acks(&self) -> Option<impl Iterator<Item = (u16, bool)>> {
        self.ack_header.map(|header| header.into_iter())
    }

    pub fn ack_header(&self) -> Option<&AckHeader> {
        self.ack_header.as_ref()
    }
    // pub fn ack_bits(&self) -> u32 {
    //     self.ack_bits
    // }

    // pub fn max_possible_size() -> usize {
    //     9
    // }

    pub fn size(&self) -> usize {
        1 + // prefix bytes
        2 + // packet sequence id
        self.ack_header.map_or(0, |header| header.size())
    }

    // max of 9 bytes?
    pub fn write(&self, mut writer: &mut impl Write) -> Result<(), PacketeerError> {
        let mut prefix_byte = 0;

        // second bit 1 ==> ack header present.
        if self.ack_header.is_some() {
            prefix_byte |= 0b0000_0010;
        }
        // use a bit to say if the ack_id in the ack header is > 255 from the packet id,
        // and write it as a u8/u16 delta to maybe save a byte?
        writer.write_u8(prefix_byte)?; // 1
        writer.write_u16::<NetworkEndian>(self.id.0)?; // +2 = 3
        if let Some(ack_header) = self.ack_header {
            ack_header.write(&mut writer)?;
        }
        Ok(())
    }

    pub fn parse(reader: &mut Cursor<&[u8]>) -> Result<Self, PacketeerError> {
        let prefix_byte = reader.read_u8()?;
        if prefix_byte & 1 != 0 {
            error!("prefix byte does not indicate regular packet");
            return Err(PacketeerError::InvalidPacket);
        }
        let ack_header_present = prefix_byte & 0b0000_0010 != 0;
        let id = PacketId(reader.read_u16::<NetworkEndian>()?);
        let ack_header = if ack_header_present {
            Some(AckHeader::parse(reader)?)
        } else {
            None
        };
        Ok(Self { id, ack_header })
    }
}
