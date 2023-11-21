use crate::PacketeerError;
use std::num::Wrapping;

pub struct SequenceBuffer<T>
where
    T: Default + std::clone::Clone + Send + Sync,
{
    entries: Vec<T>,
    entry_sequences: Vec<u32>,
    sequence: u16,
    // size: usize,
}

impl<T> SequenceBuffer<T>
where
    T: Default + std::clone::Clone + Send + Sync,
{
    pub fn with_capacity(size: usize) -> Self {
        let mut entries = Vec::with_capacity(size);
        let mut entry_sequences = Vec::with_capacity(size);

        entries.resize(size, T::default());
        entry_sequences.resize(size, 0xFFFF_FFFF);

        Self {
            sequence: 0,
            // size,
            entries,
            entry_sequences,
        }
    }

    fn type_name(&self) -> &str {
        std::any::type_name::<T>()
    }

    // #[allow(unused)]
    // pub fn reset(&mut self) {
    //     self.sequence = 0;
    //     self.entries.clear();
    //     for e in &mut self.entry_sequences {
    //         *e = 0;
    //     }
    // }

    pub fn exists(&self, sequence: u16) -> bool {
        let index = self.index(sequence);
        self.entry_sequences[index] == u32::from(sequence)
    }

    pub fn get(&self, sequence: u16) -> Option<&T> {
        let index = self.index(sequence);
        if self.entry_sequences[index] != u32::from(sequence) {
            return None;
        }

        Some(&self.entries[index])
    }
    pub fn get_mut(&mut self, sequence: u16) -> Option<&mut T> {
        let index = self.index(sequence);

        if self.entry_sequences[index] != u32::from(sequence) {
            return None;
        }

        Some(&mut self.entries[index])
    }

    pub fn insert(&mut self, data: T, sequence: u16) -> Result<&mut T, PacketeerError> {
        if Self::sequence_less_than(
            sequence,
            (Wrapping(self.sequence) - Wrapping(self.len() as u16)).0,
        ) {
            // too old to insert. rename error to "SequnceTooOld"?
            log::warn!(
                "{} Sequence too old to insert: {sequence}",
                self.type_name()
            );
            return Err(PacketeerError::SequenceBufferFull);
        }
        // log::info!("{} Inserting {sequence}..", self.type_name());
        // are we inserting with a gap in the range? ie new sequence we are inserting at
        // is more than 1 greater than the current max sequence?
        if Self::sequence_greater_than(sequence, self.sequence) {
            let first_candidate = (Wrapping(self.sequence) + Wrapping(1)).0;
            if first_candidate != sequence {
                self.remove_range(first_candidate..sequence);
            }

            self.sequence = sequence;
        }

        let index = self.index(sequence);

        self.entries[index] = data;
        self.entry_sequences[index] = u32::from(sequence);

        Ok(&mut self.entries[index])
    }

    // TODO: THIS IS INCLUSIVE END
    pub fn remove_range(&mut self, range: std::ops::Range<u16>) {
        for i in range.clone() {
            // log::warn!("{} * Remove {i} ", self.type_name());
            self.remove(i);
        }
        // log::warn!("{} * Remove {} ", self.type_name(), range.end);
        self.remove(range.end);
    }

    pub fn remove(&mut self, sequence: u16) -> Option<T> {
        // TODO: validity check
        let index = self.index(sequence);
        let ret = if self.entry_sequences[index] != u32::from(sequence) {
            self.entries[index] = T::default();
            None
        } else {
            let el = self.entries.get_mut(index).unwrap();
            Some(std::mem::take(el))
        };
        self.entry_sequences[index] = 0xFFFF_FFFF;
        ret
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        self.len() > 0
    }

    #[allow(unused)]
    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    pub fn ack_bits(&self) -> (u16, u32) {
        let ack = self.sequence;
        let mut ack_bits: u32 = 0;
        let mut mask: u32 = 1;
        for i in 0..33 {
            let sequence = (Wrapping(ack) - Wrapping(i as u16)).0;
            if self.exists(sequence) {
                ack_bits |= mask;
            }

            mask <<= 1;
        }
        (self.sequence, ack_bits)
    }

    #[inline]
    fn index(&self, sequence: u16) -> usize {
        (sequence % self.entries.len() as u16) as usize
    }

    #[inline]
    pub fn sequence_greater_than(s1: u16, s2: u16) -> bool {
        ((s1 > s2) && (s1 - s2 <= 32768)) || ((s1 < s2) && (s2 - s1 > 32768))
    }
    #[inline]
    pub fn sequence_less_than(s1: u16, s2: u16) -> bool {
        Self::sequence_greater_than(s2, s1)
    }

    #[inline]
    pub fn check_sequence(&self, sequence: u16) -> bool {
        // Self::sequence_greater_than(sequence, self.sequence)
        Self::sequence_greater_than(
            sequence,
            (Wrapping(self.sequence()) - Wrapping(self.len() as u16)).0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::*;

    #[test]
    fn gappy_inserts() {
        init_logger();
        let mut buf = SequenceBuffer::<u16>::with_capacity(100);
        for i in 0..5_u16 {
            buf.insert(i, i).unwrap();
        }
        buf.insert(6, 6).unwrap(); // removes 5,6, inserts 6
        buf.insert(5, 5).unwrap();
        buf.insert(7, 7).unwrap(); // removes 6,7, inserts 7

        for i in 0..8_u16 {
            assert_eq!(Some(&i), buf.get(i));
        }
    }
}
