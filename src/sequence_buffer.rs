use crate::PicklebackError;
/// SequenceBuffer as described here: https://gafferongames.com/post/packet_fragmentation_and_reassembly/
///
/// Other similar implementations:
/// * https://github.com/naia-lib/naia/blob/main/shared/src/connection/sequence_buffer.rs
/// * https://github.com/jaynus/reliable.io/blob/master/rust/src/sequence_buffer.rs
pub struct SequenceBuffer<T>
where
    T: Default + std::clone::Clone + Send + Sync,
{
    entries: Vec<T>,
    entry_sequences: Vec<u32>,
    sequence: u16,
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
            entries,
            entry_sequences,
        }
    }

    #[allow(unused)]
    fn type_name(&self) -> &str {
        std::any::type_name::<T>()
    }

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

    pub fn insert(&mut self, sequence: u16, data: T) -> Result<&mut T, PicklebackError> {
        if Self::sequence_less_than(sequence, self.sequence.wrapping_sub(self.len() as u16)) {
            return Err(PicklebackError::SequenceTooOld);
        }
        if Self::sequence_greater_than(sequence, self.sequence) {
            let first_candidate = self.sequence.wrapping_add(1);
            if first_candidate != sequence {
                self.remove_range(first_candidate, sequence);
            }
            self.sequence = sequence;
        }
        let index = self.index(sequence);
        self.entries[index] = data;
        self.entry_sequences[index] = u32::from(sequence);
        Ok(&mut self.entries[index])
    }

    /// remove items between start and end, inclusive
    pub fn remove_range(&mut self, start: u16, end: u16) {
        for i in start..=end {
            self.remove(i);
        }
    }

    /// removes and returns
    pub fn remove(&mut self, sequence: u16) -> Option<T> {
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

    /// current greatest inserted sequence
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
        Self::sequence_greater_than(sequence, self.sequence().wrapping_sub(self.len() as u16))
    }

    /// in ordered channels we would reject unfragmented packets that arrived late
    #[inline]
    pub fn check_newer_than_current(&self, sequence: u16) -> bool {
        Self::sequence_greater_than(sequence, self.sequence())
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
