//! Smoke tests for the public surface: `PackagerError` re-exports and
//! the `ReadAt` trait. The hand-rolled `&[u8]` adapter exercised here is
//! the same shape downstream demux tests rely on, so any future drift in
//! the trait signature breaks here first.

use std::io;

use cmafly::{PackagerError, ReadAt};

struct SliceReader<'a>(&'a [u8]);

impl ReadAt for SliceReader<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let off = offset as usize;
        if off >= self.0.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.0.len() - off);
        buf[..n].copy_from_slice(&self.0[off..off + n]);
        Ok(n)
    }
}

#[test]
fn read_at_slice_reads_partial_tail() {
    let data = b"hello, world";
    let r = SliceReader(data);
    let mut buf = [0u8; 5];

    assert_eq!(r.read_at(0, &mut buf).unwrap(), 5);
    assert_eq!(&buf, b"hello");

    assert_eq!(r.read_at(7, &mut buf).unwrap(), 5);
    assert_eq!(&buf, b"world");

    let mut tail = [0u8; 16];
    assert_eq!(r.read_at(7, &mut tail).unwrap(), 5);
    assert_eq!(&tail[..5], b"world");
}

#[test]
fn read_at_past_end_returns_zero() {
    let r = SliceReader(&[0u8; 4]);
    let mut buf = [0u8; 8];
    assert_eq!(r.read_at(4, &mut buf).unwrap(), 0);
    assert_eq!(r.read_at(99, &mut buf).unwrap(), 0);
}

#[test]
fn packager_error_variants_construct_and_display() {
    // One representative per pipeline stage so the contract surface
    // stays wired up; full variant coverage is not the goal here.
    let demux = PackagerError::MissingAtom("stsd");
    let layout = PackagerError::UnsupportedTrackLayout { video: 0, audio: 2 };
    let parse = PackagerError::IndexMagicMismatch;
    let assemble = PackagerError::SegmentIndexOutOfRange { idx: 3, count: 3 };

    for e in [&demux, &layout, &parse, &assemble] {
        assert!(!format!("{e}").is_empty());
    }
}

#[test]
fn io_error_converts_into_packager_error() {
    let io_err = io::Error::new(io::ErrorKind::UnexpectedEof, "short read");
    let err = PackagerError::from(io_err);
    assert!(matches!(err, PackagerError::Io(_)));
}
