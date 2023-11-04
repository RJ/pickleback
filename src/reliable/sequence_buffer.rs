use crate::ReliableError;
use bevy::prelude::*;
use std::num::Wrapping;

#[derive(Resource)]
pub struct SequenceBuffer<T>
where
    T: Default + std::clone::Clone + Send + Sync,
{
    entries: Vec<T>,
    entry_sequences: Vec<u32>,
    sequence: u16,
    size: usize,
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
            size,
            entries,
            entry_sequences,
        }
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

    #[cfg_attr(feature = "cargo-clippy", allow(cast_possible_truncation))]
    pub fn insert(&mut self, data: T, sequence: u16) -> Result<&mut T, ReliableError> {
        if Self::sequence_less_than(
            sequence,
            (Wrapping(self.sequence) - Wrapping(self.len() as u16)).0,
        ) {
            return Err(ReliableError::SequenceBufferFull);
        }
        if Self::sequence_greater_than((Wrapping(sequence) + Wrapping(1)).0, self.sequence) {
            self.remove_range(self.sequence..sequence);

            self.sequence = (Wrapping(sequence) + Wrapping(1)).0;
        }

        let index = self.index(sequence);

        self.entries[index] = data;
        self.entry_sequences[index] = u32::from(sequence);

        self.sequence = (Wrapping(sequence) + Wrapping(1)).0;

        Ok(&mut self.entries[index])
    }

    // TODO: THIS IS INCLUSIVE END
    pub fn remove_range(&mut self, range: std::ops::Range<u16>) {
        for i in range.clone() {
            self.remove(i);
        }
        self.remove(range.end);
    }

    pub fn remove(&mut self, sequence: u16) {
        // TODO: validity check
        let index = self.index(sequence);
        self.entries[index] = T::default();
        self.entry_sequences[index] = 0xFFFF_FFFF;
    }

    pub fn reset(&mut self) {
        self.sequence = 0;
        for e in &mut self.entry_sequences {
            *e = 0;
        }
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() > 0
    }

    pub fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    #[allow(unused)]
    #[cfg_attr(
        feature = "cargo-clippy",
        allow(cast_possible_truncation, cast_sign_loss)
    )]
    pub fn ack_bits(&self) -> (u16, u32) {
        let ack = (Wrapping(self.sequence as u16) - Wrapping(1)).0;
        let mut ack_bits: u32 = 0;
        let mut mask: u32 = 1;

        for i in 0..33 {
            let sequence = (Wrapping(ack) - Wrapping(i as u16)).0 as u16;

            if let Some(s) = self.get(sequence) {
                ack_bits |= mask;
            }

            mask <<= 1;
        }
        (ack, ack_bits)
    }

    #[inline]
    #[cfg_attr(feature = "cargo-clippy", allow(cast_possible_truncation))]
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
    #[cfg_attr(feature = "cargo-clippy", allow(cast_possible_truncation))]
    pub fn check_sequence(&self, sequence: u16) -> bool {
        Self::sequence_greater_than(
            sequence,
            (Wrapping(self.sequence()) - Wrapping(self.len() as u16)).0,
        )
    }
}
