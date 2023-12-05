use crate::prelude::PicklebackError;
use crate::PacketId;
use crate::ReceivedMeta;
use crate::SequenceBuffer;
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Cursor, Write};

pub(crate) const MAX_ACK_BYTES: u8 = 50; // MAX_ACK_BYTES*7 = num acks.
pub(crate) const MAX_UNACKED_PACKETS: u16 = 7 * MAX_ACK_BYTES as u16;

/// ack bitfield written like so:
/// the ack_id is sent as a normal u16, and then each bit is 1 if (ack_id - index) is acked.
/// index increases by one each bit we read
/// 7 bits are read per-byte, starting at least significant bit.
/// most significant bit is continuation bit. if 1, read another byte of acks.
#[derive(Copy, Clone)]
pub(crate) struct AckHeader {
    /// the most recent sequence id to ack
    /// ie, the packet we recently received with the greatest sequence number.
    ack_id: PacketId,
    /// how many packets prior to the ack_id do we ack?
    /// this indicates the length of the bitfield we write.
    num_acks: u16,
    /// the buffer into which the bitfield is written
    bit_buffer: [u8; MAX_ACK_BYTES as usize],
    /// number of bytes used to encode the ack field
    num_bytes_needed: u8,
    /// as an iterator, the byte offset and the bit offset of current position
    byte_offset: u8,
    bit_offset: u8,
}

impl std::fmt::Debug for AckHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AckHeader{{ ack_id:{:?}, num_acks:{}, ack_bits:",
            self.ack_id, self.num_acks
        )?;
        for i in 0..self.num_bytes_needed {
            let b = self.bit_buffer[i as usize];
            write!(f, " {b:#08b}")?;
        }
        write!(f, "}}")
    }
}

// Iterating over the header parses out the bits from the buffer as you go.
impl Iterator for AckHeader {
    type Item = (u16, bool);

    fn next(&mut self) -> Option<Self::Item> {
        if self.byte_offset == self.num_bytes_needed || self.byte_offset == MAX_ACK_BYTES {
            return None;
        }
        let b = self.bit_buffer[self.byte_offset as usize];
        let mask = 1_u8 << self.bit_offset;
        let is_acked = b & mask == mask;
        let seq_offset = 7 * self.byte_offset + self.bit_offset;
        let sequence = self.ack_id.0.wrapping_sub(seq_offset as u16);
        if self.bit_offset == 6 {
            if (b & 0b10000000) != 0b10000000 {
                // no continuation bit, ensure we terminate next time
                self.byte_offset = MAX_ACK_BYTES;
            } else {
                self.byte_offset += 1;
                self.bit_offset = 0;
            }
        } else {
            self.bit_offset += 1;
        }
        Some((sequence, is_acked))
    }
}

impl AckHeader {
    pub(crate) fn ack_id(&self) -> PacketId {
        self.ack_id
    }
    pub(crate) fn size(&self) -> usize {
        2 + // ack_id u16
        self.num_bytes_needed as usize
    }
    pub(crate) fn write(&self, writer: &mut impl Write) -> Result<usize, PicklebackError> {
        writer.write_u16::<NetworkEndian>(self.ack_id.0)?;
        writer.write_all(&self.bit_buffer[..self.num_bytes_needed as usize])?;
        Ok(self.num_bytes_needed as usize + 2)
    }
    /// num_acks is how many acks we must include prior to the largest sequence in the buffer
    /// ie how many bits in the bitfield
    pub(crate) fn from_ack_iter(
        num_acks: u16,
        ack_iter: impl Iterator<Item = (u16, bool)>,
    ) -> Result<Self, PicklebackError> {
        let mut peekable_iter = ack_iter.peekable();
        // peek the first id, which is always the most recent ack, and gets written as a u16
        // all prior acks get 1 bit, the offset of which is relative to the first ack id.
        let (ack_id, _) = peekable_iter.peek().expect("ack iter must be non-empty");
        let ack_id = PacketId(*ack_id);
        let num_bytes_needed = ((num_acks as f32 / 7_f32).ceil() as u8).max(1_u8);
        let mut bit_buffer = [0_u8; MAX_ACK_BYTES as usize];
        let mut writer = &mut bit_buffer[..];
        for _ in 0..num_bytes_needed {
            let mut mask: u8 = 1;
            let mut current_byte: u8 = 0;
            for _ in 0..7 {
                match peekable_iter.next() {
                    Some((_id, is_acked)) if is_acked => current_byte |= mask,
                    _ => {}
                }
                mask <<= 1;
            }
            // the 8th and most sig bit is the continuation marker. are there more to come?
            if peekable_iter.peek().is_some() {
                current_byte |= mask;
            }
            writer.write_u8(current_byte)?;
        }

        Ok(Self {
            ack_id,
            num_acks,
            num_bytes_needed,
            bit_buffer,
            byte_offset: 0,
            bit_offset: 0,
        })
    }

    pub(crate) fn parse(reader: &mut Cursor<&[u8]>) -> Result<Self, PicklebackError> {
        let ack_id = PacketId(reader.read_u16::<NetworkEndian>()?);
        let mut bit_buffer = [0_u8; MAX_ACK_BYTES as usize];
        let mut writer = &mut bit_buffer[..];
        let mut num_bytes_needed = 0_u8;
        // just reading the correct number of bytes and storing in buffer;
        // don't care about decoding them here – the iter does that.
        for _ in 0..MAX_ACK_BYTES {
            let b = reader.read_u8()?;
            writer.write_u8(b)?;
            num_bytes_needed += 1;
            // most sig bit set? continuation to next byte
            if (b & 0b10000000) != 0b10000000 {
                break;
            }
        }
        Ok(Self {
            ack_id,
            num_acks: num_bytes_needed as u16 * 7,
            num_bytes_needed,
            bit_buffer,
            byte_offset: 0,
            bit_offset: 0,
        })
    }
}

/// An iterator of received sequence ids from RecvData SequenceBuffer
pub(crate) struct AckIter<'a> {
    seq_buffer: &'a SequenceBuffer<ReceivedMeta>,
    i: u16,
    max: u16,
}
impl<'a> Iterator for AckIter<'a> {
    type Item = (u16, bool);
    fn next(&mut self) -> Option<Self::Item> {
        if self.i >= self.max {
            None
        } else {
            let sequence = self.seq_buffer.sequence().wrapping_sub(self.i);
            let exists = self.seq_buffer.exists(sequence);
            self.i += 1;
            Some((sequence, exists))
        }
    }
}
impl<'a> AckIter<'a> {
    /// Creates the acks iterator rounded up to the nearest multiple of 7, to fill the available
    /// bitfield in the ack header.
    pub(crate) fn with_minimum_length(
        seq_buffer: &'a SequenceBuffer<ReceivedMeta>,
        length: u16,
    ) -> AckIter<'a> {
        let max = (length as f32 / 7.).ceil() as u16 * 7;
        AckIter {
            seq_buffer,
            i: 0,
            max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;
    #[test]
    fn ack_header() {
        init_logger();

        fn mk_acks(len: u16) -> Vec<(u16, bool)> {
            let start = rand::random::<u16>();
            let mut v = vec![(start, true)];
            for i in 1..len {
                let id = start.wrapping_sub(i);
                let is_acked = rand::random::<bool>();
                v.push((id, is_acked));
            }
            v
        }

        for i in 0..1000 {
            let len = i % MAX_UNACKED_PACKETS;
            // always send multiple of 7 acks, due to encoding format
            let len = ((len as f32 / 7.0).ceil() * 7.) as u16;
            let len = len.max(7);
            let acks = mk_acks(len);
            let header = AckHeader::from_ack_iter(acks.len() as u16, acks.iter().cloned()).unwrap();
            let decoded = header.into_iter().collect::<Vec<_>>();
            assert_eq!(decoded, acks, "ack mismatch, len was {len}");
        }
    }
}
