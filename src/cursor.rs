use std::io::Cursor;
use std::io::{self, Write};

pub(crate) type BufferLimitedWriter<'a> = WriteLimiter<Cursor<&'a mut Vec<u8>>>;

pub(crate) struct WriteLimiter<W: Write> {
    writer: W,
    limit: usize,
    written: usize,
}

impl<W: Write> WriteLimiter<W> {
    pub fn new(writer: W, limit: usize) -> Self {
        WriteLimiter {
            writer,
            limit,
            written: 0,
        }
    }

    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.written)
    }
}

impl<W: Write> Write for WriteLimiter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.limit {
            return Ok(0);
        }

        let allowed_to_write = self.limit - self.written;
        let to_write = std::cmp::min(allowed_to_write, buf.len());

        match self.writer.write(&buf[..to_write]) {
            Ok(bytes_written) => {
                self.written += bytes_written;
                Ok(bytes_written)
            }
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// This can be deleted once rust's #![feature(cursor_remaining)] is stable.
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
