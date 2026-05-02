use std::io;

use memmap2::Mmap;

/// Random-access byte reader over an MP4 source.
///
/// The library never opens a file; callers pass an implementor. `cmafly-serve`
/// holds an `Mmap` per archive; tests use a thin `&[u8]` adapter. Returning
/// `Ok(0)` indicates end of source — callers must treat a short read as final.
pub trait ReadAt {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
}

fn read_at_bytes(src: &[u8], offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let off = offset as usize;
    if off >= src.len() {
        return Ok(0);
    }
    let n = buf.len().min(src.len() - off);
    buf[..n].copy_from_slice(&src[off..off + n]);
    Ok(n)
}

impl ReadAt for Mmap {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_at_bytes(self, offset, buf)
    }
}

/// Test-only `ReadAt` adapter over a byte slice. Lives in library code (not
/// `dev-dependencies`) so every module's `#[cfg(test)]` block shares one
/// implementation rather than duplicating the boilerplate.
#[cfg(test)]
pub(crate) struct SliceReader<'a>(pub &'a [u8]);

#[cfg(test)]
impl ReadAt for SliceReader<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        read_at_bytes(self.0, offset, buf)
    }
}
