// This can be deleted once rust's #![feature(cursor_remaining)] is stable.
use std::io::Cursor;

pub(crate) trait CursorExtras {
    fn remaining(&self) -> u64;
    fn remaining_slice(&self) -> &[u8];
    fn is_empty(&self) -> bool;
}

impl<T> CursorExtras for Cursor<T>
where
    T: AsRef<[u8]>,
{
    /// Returns the remaining length.
    #[allow(clippy::manual_saturating_arithmetic)]
    fn remaining(&self) -> u64 {
        (self.get_ref().as_ref().len() as u64)
            .checked_sub(self.position())
            .unwrap_or(0)
    }

    /// Returns the remaining slice.
    fn remaining_slice(&self) -> &[u8] {
        let len = self.position().min(self.get_ref().as_ref().len() as u64);
        &self.get_ref().as_ref()[(len as usize)..]
    }

    /// Returns `true` if the remaining slice is empty.
    fn is_empty(&self) -> bool {
        self.position() >= self.get_ref().as_ref().len() as u64
    }
}
