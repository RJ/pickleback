use crate::{ack_header::AckHeader, PacketId, PacketeerError};
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use log::*;
use std::io::{Cursor, Write};

#[derive(Debug)]
pub struct PacketHeader {
    id: PacketId,
    ack_header: Option<AckHeader>,
}

impl PacketHeader {
    pub(crate) fn new_no_acks(id: PacketId) -> Result<Self, PacketeerError> {
        Ok(Self {
            id,
            ack_header: None,
        })
    }
    pub(crate) fn new(
        id: PacketId,
        ack_iter: impl Iterator<Item = (u16, bool)>,
        num_acks_required: u16,
    ) -> Result<Self, PacketeerError> {
        assert!(num_acks_required > 0, "num acks required must be > 0");
        let ack_header = Some(AckHeader::from_ack_iter(num_acks_required, ack_iter)?);
        Ok(Self { id, ack_header })
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
        self.ack_header.map_or(0, |header| header.size())
    }

    pub(crate) fn write(&self, mut writer: &mut impl Write) -> Result<(), PacketeerError> {
        let mut prefix_byte = 0;

        // second bit 1 ==> ack header present.
        if self.ack_header.is_some() {
            prefix_byte |= 0b0000_0010;
        }
        // could use a bit to say if the ack_id in the ack header is > 255 from the packet id,
        // and write it as a u8/u16 delta to maybe save a byte?
        writer.write_u8(prefix_byte)?; // 1
        writer.write_u16::<NetworkEndian>(self.id.0)?;
        if let Some(ack_header) = self.ack_header {
            ack_header.write(&mut writer)?;
        }
        Ok(())
    }

    pub(crate) fn parse(reader: &mut Cursor<&[u8]>) -> Result<Self, PacketeerError> {
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
