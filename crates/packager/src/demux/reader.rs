//! ISO/IEC 14496-12 box-header scanner.
//!
//! Walks a contiguous byte range over a `ReadAt` source and yields one
//! [`BoxHeader`] per call. Tolerates both the 32-bit `size` and the 64-bit
//! `largesize` (size = 1) header forms, and the legacy "size = 0 → extends to
//! end of container" form. `uuid` (vendor-extension) boxes are silently
//! skipped — none of the atoms this demuxer cares about live inside one.

use byteorder::{BigEndian, ByteOrder};

use crate::ReadAt;
use crate::error::PackagerError;

/// One parsed box header. Payload bytes occupy the half-open range
/// `[payload_offset .. start + declared_size)` in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoxHeader {
    pub(crate) box_type: [u8; 4],
    pub(crate) start: u64,
    pub(crate) header_size: u8,
    pub(crate) declared_size: u64,
    pub(crate) payload_offset: u64,
}

impl BoxHeader {
    pub(crate) fn payload_len(&self) -> u64 {
        self.declared_size - self.header_size as u64
    }

    pub(crate) fn end(&self) -> u64 {
        self.start + self.declared_size
    }
}

/// Iterator over the boxes in a half-open `[start, end)` byte range.
///
/// Construct one with [`BoxIter::new`], then call [`BoxIter::next_header`]
/// repeatedly until it returns `Ok(None)`. Errors abort iteration; the
/// internal cursor is not advanced past a malformed header so the caller can
/// inspect [`BoxIter::cursor`] to locate the failure.
pub(crate) struct BoxIter<'r, R: ReadAt + ?Sized> {
    reader: &'r R,
    cursor: u64,
    end: u64,
}

impl<'r, R: ReadAt + ?Sized> BoxIter<'r, R> {
    pub(crate) fn new(reader: &'r R, start: u64, end: u64) -> Self {
        Self {
            reader,
            cursor: start,
            end,
        }
    }

    pub(crate) fn next_header(&mut self) -> Result<Option<BoxHeader>, PackagerError> {
        loop {
            if self.cursor >= self.end {
                return Ok(None);
            }
            let remaining = self.end - self.cursor;
            if remaining < 8 {
                return Err(PackagerError::MalformedAtom {
                    atom: "<box-header>",
                    reason: "fewer than 8 bytes remain for size and type",
                });
            }

            let mut head = [0u8; 8];
            read_exact(self.reader, self.cursor, &mut head)?;
            let size32 = BigEndian::read_u32(&head[..4]);
            let mut box_type = [0u8; 4];
            box_type.copy_from_slice(&head[4..8]);

            let (declared_size, header_size): (u64, u8) = if size32 == 1 {
                if remaining < 16 {
                    return Err(PackagerError::MalformedAtom {
                        atom: "<box-header>",
                        reason: "largesize indicated but fewer than 16 bytes remain",
                    });
                }
                let mut large = [0u8; 8];
                read_exact(self.reader, self.cursor + 8, &mut large)?;
                (BigEndian::read_u64(&large), 16)
            } else if size32 == 0 {
                // Box extends to the end of the enclosing container.
                (remaining, 8)
            } else {
                (size32 as u64, 8)
            };

            if declared_size < header_size as u64 {
                return Err(PackagerError::MalformedAtom {
                    atom: "<box-header>",
                    reason: "declared size is smaller than its own header",
                });
            }
            if declared_size > remaining {
                return Err(PackagerError::MalformedAtom {
                    atom: "<box-header>",
                    reason: "declared size exceeds enclosing container",
                });
            }

            let header = BoxHeader {
                box_type,
                start: self.cursor,
                header_size,
                declared_size,
                payload_offset: self.cursor + header_size as u64,
            };
            self.cursor = header.end();

            if &box_type == b"uuid" {
                continue;
            }
            return Ok(Some(header));
        }
    }
}

/// Read exactly `buf.len()` bytes from `offset`, looping over short reads and
/// translating `read_at` returning 0 into [`std::io::ErrorKind::UnexpectedEof`].
pub(crate) fn read_exact<R: ReadAt + ?Sized>(
    reader: &R,
    offset: u64,
    buf: &mut [u8],
) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = reader.read_at(offset + filled as u64, &mut buf[filled..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ReadAt returned 0 before requested bytes were available",
            ));
        }
        filled += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_at::SliceReader;

    fn box_header(size: u32, fourcc: &[u8; 4]) -> [u8; 8] {
        let mut head = [0u8; 8];
        head[..4].copy_from_slice(&size.to_be_bytes());
        head[4..].copy_from_slice(fourcc);
        head
    }

    #[test]
    fn yields_ftyp_then_mdat() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&box_header(8, b"ftyp"));
        bytes.extend_from_slice(&box_header(64, b"mdat"));
        bytes.extend_from_slice(&[0u8; 56]);

        let len = bytes.len() as u64;
        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, len);

        let h1 = iter.next_header().unwrap().expect("ftyp present");
        assert_eq!(&h1.box_type, b"ftyp");
        assert_eq!(h1.declared_size, 8);
        assert_eq!(h1.header_size, 8);
        assert_eq!(h1.payload_offset, 8);
        assert_eq!(h1.payload_len(), 0);

        let h2 = iter.next_header().unwrap().expect("mdat present");
        assert_eq!(&h2.box_type, b"mdat");
        assert_eq!(h2.declared_size, 64);
        assert_eq!(h2.payload_offset, 16);
        assert_eq!(h2.end(), 72);

        assert!(iter.next_header().unwrap().is_none());
    }

    #[test]
    fn handles_largesize_header() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&box_header(1, b"mdat"));
        bytes.extend_from_slice(&32u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 16]);

        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let h = iter.next_header().unwrap().expect("mdat present");
        assert_eq!(&h.box_type, b"mdat");
        assert_eq!(h.declared_size, 32);
        assert_eq!(h.header_size, 16);
        assert_eq!(h.payload_offset, 16);
        assert!(iter.next_header().unwrap().is_none());
    }

    #[test]
    fn size_zero_extends_to_container_end() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&box_header(0, b"mdat"));
        bytes.extend_from_slice(&[0u8; 24]);

        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let h = iter.next_header().unwrap().expect("mdat present");
        assert_eq!(h.declared_size, bytes.len() as u64);
        assert_eq!(h.payload_offset, 8);
        assert!(iter.next_header().unwrap().is_none());
    }

    #[test]
    fn skips_uuid_silently() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&box_header(24, b"uuid"));
        bytes.extend_from_slice(&[0u8; 16]); // uuid extended-type
        bytes.extend_from_slice(&box_header(8, b"ftyp"));

        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let h = iter.next_header().unwrap().expect("only ftyp surfaces");
        assert_eq!(&h.box_type, b"ftyp");
        assert!(iter.next_header().unwrap().is_none());
    }

    #[test]
    fn rejects_size_below_header() {
        let bytes = box_header(4, b"ftyp");
        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let err = iter.next_header().unwrap_err();
        assert!(matches!(err, PackagerError::MalformedAtom { .. }));
    }

    #[test]
    fn rejects_size_past_container() {
        let bytes = box_header(64, b"ftyp");
        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let err = iter.next_header().unwrap_err();
        assert!(matches!(err, PackagerError::MalformedAtom { .. }));
    }

    #[test]
    fn rejects_truncated_largesize() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&box_header(1, b"mdat"));
        bytes.extend_from_slice(&[0u8; 4]); // only half of largesize present
        let reader = SliceReader(&bytes);
        let mut iter = BoxIter::new(&reader, 0, bytes.len() as u64);
        let err = iter.next_header().unwrap_err();
        assert!(matches!(err, PackagerError::MalformedAtom { .. }));
    }
}
