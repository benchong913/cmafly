//! fMP4 box framing helpers.
//!
//! Enforces the always-on box-size invariant: every emitted box's declared
//! `size` field equals the bytes actually written between its header start
//! and the writer cursor at `finish`-time. The check is `assert!`, not
//! `debug_assert!`, because a mismatch produces silently-malformed output
//! that no downstream parser could recover.
//!
//! Usage pattern:
//!
//! ```ignore
//! let mut moov = BoxWriter::open(out, *b"moov")?;
//! {
//!     let mut mvhd = moov.child(*b"mvhd")?;
//!     mvhd.writer().write_all(&payload)?;
//!     mvhd.finish()?;
//! }
//! moov.finish()?;
//! ```
//!
//! For boxes whose payload size is known up front (e.g., `mdat` from the
//! sum of sample sizes, or pre-rendered byte blobs), use
//! `finish_expecting(expected)` so the byte count is cross-checked against
//! the caller's arithmetic.

use std::io::{self, Seek, SeekFrom, Write};

use byteorder::{BigEndian, WriteBytesExt};

/// Write the 4-byte FullBox header `(version << 24) | flags` to `out`.
/// `flags` must fit in 24 bits — this is checked under `debug_assert!`.
pub(super) fn write_full_box_header<W: Write>(
    out: &mut W,
    version: u8,
    flags: u32,
) -> io::Result<()> {
    debug_assert!(flags <= 0x00FF_FFFF, "flags must fit in 24 bits");
    let packed = (u32::from(version) << 24) | (flags & 0x00FF_FFFF);
    out.write_u32::<BigEndian>(packed)
}

/// A scoped fMP4 box. `open` reserves the 4-byte size placeholder and writes
/// the fourcc; `finish` patches the size and verifies the box-size invariant.
///
/// `BoxWriter` borrows the underlying writer mutably for the duration of the
/// scope; nested boxes are constructed via [`BoxWriter::child`], which
/// re-borrows for the inner scope.
///
/// `#[must_use]` flags the open-and-discard pattern at compile time; the
/// caller must hold the value until [`BoxWriter::finish`] (or
/// [`BoxWriter::finish_expecting`]) runs, otherwise the size header is left
/// unpatched. There is no `Drop` panic on purpose — early `?` returns are a
/// legitimate exit path and a `Drop` panic would mask the underlying I/O
/// error.
#[must_use = "BoxWriter must be closed via `finish` or `finish_expecting` to patch the size header"]
pub(crate) struct BoxWriter<'w, W: Write + Seek> {
    inner: &'w mut W,
    start: u64,
    fourcc: [u8; 4],
}

impl<'w, W: Write + Seek> BoxWriter<'w, W> {
    /// Open a new box of `fourcc` at the writer's current position.
    pub(crate) fn open(inner: &'w mut W, fourcc: [u8; 4]) -> io::Result<Self> {
        let start = inner.stream_position()?;
        inner.write_u32::<BigEndian>(0)?;
        inner.write_all(&fourcc)?;
        Ok(Self {
            inner,
            start,
            fourcc,
        })
    }

    /// Direct access to the underlying writer for raw payload bytes.
    pub(crate) fn writer(&mut self) -> &mut W {
        self.inner
    }

    /// Open a child box at the current cursor, sharing the parent's writer.
    pub(crate) fn child<'c>(&'c mut self, fourcc: [u8; 4]) -> io::Result<BoxWriter<'c, W>> {
        BoxWriter::open(self.inner, fourcc)
    }

    /// Patch the size header with the actual byte count and assert the
    /// box-size invariant. Returns the total bytes occupied by the box.
    pub(crate) fn finish(self) -> io::Result<u64> {
        self.finish_with_expected(None)
    }

    /// Patch the size header and assert the actual byte count equals
    /// `expected_size`. Use this for boxes whose size is computed in
    /// advance (e.g., `mdat`, verbatim blob children); a mismatch signals
    /// an arithmetic bug in the caller and is unrecoverable.
    pub(crate) fn finish_expecting(self, expected_size: u32) -> io::Result<u64> {
        self.finish_with_expected(Some(expected_size))
    }

    fn finish_with_expected(self, expected_size: Option<u32>) -> io::Result<u64> {
        let end = self.inner.stream_position()?;
        let written = end.checked_sub(self.start).ok_or_else(|| {
            io::Error::other(format!(
                "fMP4 box `{}`: writer position regressed below box start",
                fourcc_str(self.fourcc),
            ))
        })?;

        let declared = u32::try_from(written).map_err(|_| {
            io::Error::other(format!(
                "fMP4 box `{}` size {} exceeds u32::MAX (use largesize via a separate writer)",
                fourcc_str(self.fourcc),
                written,
            ))
        })?;

        self.inner.seek(SeekFrom::Start(self.start))?;
        self.inner.write_u32::<BigEndian>(declared)?;
        self.inner.seek(SeekFrom::Start(end))?;

        // Always-on box-size invariant. With a correctly-implemented `Seek`,
        // the first assertion holds tautologically — it exists to catch a
        // buggy writer rather than a buggy compose-time path. The
        // `expected_size` arm catches arithmetic bugs in callers that
        // pre-compute payload sizes (e.g., `mdat` from sample-size sums).
        assert_eq!(
            u64::from(declared),
            written,
            "fMP4 box `{}` size invariant: declared {} bytes but wrote {}",
            fourcc_str(self.fourcc),
            declared,
            written,
        );
        if let Some(expected) = expected_size {
            assert_eq!(
                u64::from(expected),
                written,
                "fMP4 box `{}` size invariant: expected {} bytes but wrote {}",
                fourcc_str(self.fourcc),
                expected,
                written,
            );
        }

        Ok(written)
    }
}

fn fourcc_str(fourcc: [u8; 4]) -> String {
    String::from_utf8(fourcc.to_vec()).unwrap_or_else(|_| format!("{fourcc:02x?}"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use byteorder::{BigEndian, ByteOrder};

    use super::*;

    #[test]
    fn open_finish_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        let mut cur = Cursor::new(&mut buf);
        let mut bw = BoxWriter::open(&mut cur, *b"test").expect("open");
        bw.writer().write_all(&[0xDE, 0xAD, 0xBE, 0xEF]).unwrap();
        let total = bw.finish().expect("finish");
        assert_eq!(total, 12);
        assert_eq!(BigEndian::read_u32(&buf[..4]), 12);
        assert_eq!(&buf[4..8], b"test");
        assert_eq!(&buf[8..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn empty_box_is_eight_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        let mut cur = Cursor::new(&mut buf);
        let bw = BoxWriter::open(&mut cur, *b"free").expect("open");
        let total = bw.finish().expect("finish");
        assert_eq!(total, 8);
        assert_eq!(buf, [0, 0, 0, 8, b'f', b'r', b'e', b'e']);
    }

    #[test]
    fn nested_boxes_size_each_independently() {
        let mut buf: Vec<u8> = Vec::new();
        let mut cur = Cursor::new(&mut buf);
        let mut outer = BoxWriter::open(&mut cur, *b"moov").expect("open outer");
        {
            let mut inner = outer.child(*b"mvhd").expect("open inner");
            inner.writer().write_all(&[0u8; 4]).unwrap();
            let inner_size = inner.finish().expect("finish inner");
            assert_eq!(inner_size, 12);
        }
        {
            let mut inner = outer.child(*b"trak").expect("open inner");
            inner.writer().write_all(&[0u8; 16]).unwrap();
            let inner_size = inner.finish().expect("finish inner");
            assert_eq!(inner_size, 24);
        }
        let outer_size = outer.finish().expect("finish outer");
        assert_eq!(outer_size, 8 + 12 + 24);

        // Outer box header carries the total size; first inner header lives
        // immediately after (offset 8); second inner immediately after that.
        assert_eq!(BigEndian::read_u32(&buf[0..4]), outer_size as u32);
        assert_eq!(&buf[4..8], b"moov");
        assert_eq!(BigEndian::read_u32(&buf[8..12]), 12);
        assert_eq!(&buf[12..16], b"mvhd");
        assert_eq!(BigEndian::read_u32(&buf[20..24]), 24);
        assert_eq!(&buf[24..28], b"trak");
    }

    #[test]
    fn finish_expecting_matching_size_succeeds() {
        let mut buf: Vec<u8> = Vec::new();
        let mut cur = Cursor::new(&mut buf);
        let mut bw = BoxWriter::open(&mut cur, *b"mdat").expect("open");
        bw.writer().write_all(&[0u8; 16]).unwrap();
        // 8 (header) + 16 (payload) = 24
        let total = bw.finish_expecting(24).expect("finish");
        assert_eq!(total, 24);
    }

    #[test]
    fn finish_expecting_wrong_size_panics() {
        // Verifies the "caller wrote too few bytes" path: the caller declares
        // an expected size of 100 but writes only 4. The always-on assertion
        // fires before any malformed bytes leave the writer.
        let result = std::panic::catch_unwind(|| {
            let mut buf: Vec<u8> = Vec::new();
            let mut cur = Cursor::new(&mut buf);
            let mut bw = BoxWriter::open(&mut cur, *b"mdat").expect("open");
            bw.writer().write_all(&[0u8; 4]).unwrap();
            // Actual: 8 + 4 = 12; expected: 100. Assertion must fire.
            let _ = bw.finish_expecting(100);
        });
        let payload = result.expect_err("expected the size invariant to panic");
        let msg = panic_message(&payload);
        assert!(
            msg.contains("mdat"),
            "panic message should name fourcc; got: {msg}",
        );
        assert!(
            msg.contains("100") && msg.contains("12"),
            "panic message should name both sizes; got: {msg}",
        );
    }

    fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            String::from("<non-string panic payload>")
        }
    }
}
