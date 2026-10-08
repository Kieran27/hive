//! Bounded byte ring with monotonic sequence numbers, so a client can attach
//! late (or re-attach) and receive exactly the bytes it missed.

use std::collections::VecDeque;

pub const RING_CAPACITY: usize = 1024 * 1024;

pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
    /// Seq of the first byte still held.
    start: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            start: 0,
        }
    }

    /// Seq one past the newest byte.
    pub fn end(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn wrapped(&self) -> bool {
        self.start > 0
    }

    /// Append; returns the seq of the first appended byte.
    pub fn push(&mut self, data: &[u8]) -> u64 {
        let seq = self.end();
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            self.start = seq + data.len() as u64 - self.cap as u64;
            return seq;
        }
        let overflow = (self.buf.len() + data.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
            self.start += overflow as u64;
        }
        self.buf.extend(data);
        seq
    }

    /// Bytes from `from` onward. If `from` already fell off (or is in the
    /// future) the whole ring is returned. Returns `(base_seq, bytes)`.
    pub fn snapshot_from(&self, from: u64) -> (u64, Vec<u8>) {
        if from < self.start || from > self.end() {
            return (self.start, self.buf.iter().copied().collect());
        }
        let skip = (from - self.start) as usize;
        (from, self.buf.iter().skip(skip).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_and_full() {
        let mut r = Ring::new(8);
        assert_eq!(r.push(b"abc"), 0);
        assert_eq!(r.push(b"def"), 3);
        assert_eq!(r.snapshot_from(3), (3, b"def".to_vec()));
        assert_eq!(r.snapshot_from(6), (6, vec![]));
        assert_eq!(r.snapshot_from(0), (0, b"abcdef".to_vec()));
    }

    #[test]
    fn overflow_drops_oldest() {
        let mut r = Ring::new(8);
        r.push(b"abcdef");
        r.push(b"ghij");
        assert_eq!(r.start(), 2);
        assert_eq!(r.end(), 10);
        assert!(r.wrapped());
        // Requested seq fell off: whole ring from its start.
        assert_eq!(r.snapshot_from(1), (2, b"cdefghij".to_vec()));
        assert_eq!(r.snapshot_from(8), (8, b"ij".to_vec()));
        // Future seq (client from an older daemon): whole ring.
        assert_eq!(r.snapshot_from(99).0, 2);
    }

    #[test]
    fn huge_write() {
        let mut r = Ring::new(4);
        r.push(b"xy");
        assert_eq!(r.push(b"0123456789"), 2);
        assert_eq!(r.snapshot_from(0), (8, b"6789".to_vec()));
        assert_eq!(r.end(), 12);
    }
}
