//! `cmafly-index` — build a `.idx` file from an MP4 archive.
//!
//! The CLI mmaps the source MP4, hands it to [`IndexBuilder::build`],
//! re-opens the result through [`IndexView::open`] to emit a stderr
//! `note:` when every video sample is flagged sync (the only on-disk
//! fingerprint of an `stss`-absent input), and writes the bytes
//! atomically: write to a sibling `*.tmp`, fsync, then rename.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use memmap2::Mmap;

use cmafly::{IndexBuilder, IndexView, SampleEntry};

#[derive(Parser, Debug)]
#[command(
    name = "cmafly-index",
    about = "Build a .idx index file from an MP4 archive"
)]
struct Cli {
    /// Source MP4 path.
    #[arg(long)]
    input: PathBuf,
    /// Output .idx path. Written atomically via a sibling `*.tmp` + rename.
    #[arg(long)]
    output: PathBuf,
    /// Nominal segment duration in seconds. Real cuts snap forward to
    /// the next IDR (sync sample), so the actual duration is always
    /// `≥ segment_duration` and aligned on a video sync sample.
    #[arg(long)]
    segment_duration: f64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    run(
        &cli.input,
        &cli.output,
        cli.segment_duration,
        &mut std::io::stderr(),
    )
}

/// Build a `.idx` from `input` and write it to `output`. `stderr`
/// receives a `note:` line when the input likely lacked `stss`.
fn run<W: Write>(input: &Path, output: &Path, segment_duration: f64, stderr: &mut W) -> Result<()> {
    let file = File::open(input).with_context(|| format!("open input {}", input.display()))?;
    // SAFETY: read-only mmap of a regular file we just opened.
    // Concurrent mutation of source MP4s during indexing is
    // operationally forbidden — the indexer is offline / batch-only.
    let mmap =
        unsafe { Mmap::map(&file) }.with_context(|| format!("mmap input {}", input.display()))?;
    let len = mmap.len() as u64;

    let bytes = IndexBuilder::build(&mmap, len, segment_duration)
        .with_context(|| format!("build .idx from {}", input.display()))?;

    let aligned = AlignedIdx::from_bytes(&bytes);
    let view =
        IndexView::open(aligned.as_slice()).context("re-open built .idx for stss-note check")?;
    if all_video_samples_sync(view.video_samples()) {
        writeln!(
            stderr,
            "note: all video samples are sync (input likely lacked stss)"
        )
        .context("write stss note to stderr")?;
    }

    write_atomic(output, &bytes).with_context(|| format!("atomic write {}", output.display()))
}

/// 8-byte-aligned copy of a `.idx` byte image.
///
/// `IndexView::open` casts the sample-table payloads to `[SampleEntry]`
/// in-place and rejects misaligned input with `MalformedIndexSection`.
/// `Vec<u8>::as_ptr()` is only `align_of::<u8>() == 1` by Rust's
/// allocator contract; system malloc happens to return ≥ 8-aligned
/// chunks, but we do not rely on allocator-specific behaviour. Backing
/// the buffer with `Vec<u64>` makes the start address 8-byte aligned by
/// construction.
struct AlignedIdx {
    backing: Vec<u64>,
    len: usize,
}

impl AlignedIdx {
    fn from_bytes(bytes: &[u8]) -> Self {
        let words = bytes.len().div_ceil(8);
        let mut backing = vec![0u64; words];
        // SAFETY: `backing` owns `words * 8 >= bytes.len()` bytes of
        // contiguous, writable storage; `bytes` is a borrowed read-only
        // slice that does not alias `backing`. We only initialise the
        // first `bytes.len()` bytes; trailing bytes remain zero from
        // `vec![0u64; …]`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                backing.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        Self {
            backing,
            len: bytes.len(),
        }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: `backing` provides at least `len` initialised bytes
        // (see `from_bytes`), the pointer is non-null and properly
        // aligned for `u8`, and the returned slice's lifetime is tied
        // to `&self`.
        unsafe { std::slice::from_raw_parts(self.backing.as_ptr().cast::<u8>(), self.len) }
    }
}

/// Heuristic for the `stss`-absent stderr note: every video sample
/// flagged sync is the only on-disk fingerprint of an `stss`-absent
/// source. The note's wording — "input likely lacked stss" —
/// accommodates the rare intra-only case where a real `stss` lists
/// every sample, which the same heuristic cannot disambiguate.
fn all_video_samples_sync(samples: &[SampleEntry]) -> bool {
    !samples.is_empty() && samples.iter().all(SampleEntry::is_sync)
}

/// Write `bytes` to `dst` atomically: open `dst.tmp`, write + fsync,
/// then rename over `dst`. Crash before the rename leaves the original
/// `dst` (if any) untouched.
fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_sibling(dst);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .with_context(|| format!("open tmp {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write tmp {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync tmp {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()))?;
    sync_parent_dir(dst)
}

/// Make the rename durable: a `rename(2)` only updates the parent
/// directory's metadata, which the OS may buffer. Without an `fsync` on
/// the parent, a crash immediately after the call returns can lose the
/// rename even though `tmp` was already fsynced. POSIX-only; the
/// equivalent on non-Unix platforms is left to the OS.
#[cfg(unix)]
fn sync_parent_dir(dst: &Path) -> Result<()> {
    let parent = dst
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .with_context(|| format!("open parent dir {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("fsync parent dir {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_dir(_dst: &Path) -> Result<()> {
    Ok(())
}

/// Path of a sibling temp file: same directory as `dst`, suffix `.tmp`.
/// Sibling siting matters: `rename` is only atomic within a single
/// filesystem.
fn tmp_sibling(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "cmafly-index-test-{label}-{}-{nanos}",
            std::process::id(),
        ));
        std::fs::create_dir(&dir).expect("create tempdir");
        dir
    }

    fn sample(flags: u32) -> SampleEntry {
        SampleEntry {
            offset: 0,
            size: 1,
            dts_delta: 1,
            cts_offset: 0,
            flags,
        }
    }

    #[test]
    fn tmp_sibling_appends_dot_tmp_to_dst() {
        let tmp = tmp_sibling(Path::new("/tmp/foo.idx"));
        assert_eq!(tmp, PathBuf::from("/tmp/foo.idx.tmp"));
    }

    #[test]
    fn tmp_sibling_preserves_directory() {
        let tmp = tmp_sibling(Path::new("/var/data/sub/out.idx"));
        assert_eq!(tmp, PathBuf::from("/var/data/sub/out.idx.tmp"));
    }

    #[test]
    fn note_heuristic_true_when_every_sample_sync() {
        let v = [sample(1), sample(1)];
        assert!(all_video_samples_sync(&v));
    }

    #[test]
    fn note_heuristic_false_when_any_non_sync() {
        let v = [sample(1), sample(0), sample(1)];
        assert!(!all_video_samples_sync(&v));
    }

    #[test]
    fn note_heuristic_false_for_empty() {
        assert!(!all_video_samples_sync(&[]));
    }

    #[test]
    fn aligned_idx_preserves_bytes_and_alignment() {
        // Cover the misaligned-source path: take a `Vec<u8>` allocated
        // off-by-one (so even system malloc cannot save us), copy
        // through `AlignedIdx`, and confirm the result is 8-aligned and
        // byte-identical for the original payload length.
        let raw: Vec<u8> = (0u8..23).collect();
        let aligned = AlignedIdx::from_bytes(&raw);
        let slice = aligned.as_slice();
        assert_eq!(slice, raw.as_slice());
        assert_eq!(slice.len(), raw.len());
        assert!(
            (slice.as_ptr() as usize).is_multiple_of(8),
            "AlignedIdx::as_slice must return an 8-byte-aligned pointer",
        );
    }

    #[test]
    fn write_atomic_creates_dst_with_exact_bytes_no_tmp_remains() {
        let dir = unique_dir("write-atomic");
        let dst = dir.join("out.idx");
        let payload = b"HCMI\x00\x01\x02\x03";

        write_atomic(&dst, payload).expect("write_atomic");

        assert!(dst.exists(), "dst must exist");
        assert_eq!(std::fs::read(&dst).expect("read"), payload);
        assert!(
            !tmp_sibling(&dst).exists(),
            "tmp sibling must not remain after successful rename",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_atomic_overwrites_existing_dst() {
        let dir = unique_dir("overwrite");
        let dst = dir.join("out.idx");
        std::fs::write(&dst, b"OLD").expect("seed dst");

        write_atomic(&dst, b"NEW").expect("write_atomic");

        assert_eq!(std::fs::read(&dst).expect("read"), b"NEW");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_atomic_fails_when_parent_dir_missing() {
        let dir = unique_dir("missing-parent");
        let missing = dir.join("nope").join("out.idx");
        let err = write_atomic(&missing, b"x").expect_err("missing parent must error");
        // The error chain should mention the tmp path so the operator can
        // trace the failure.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("out.idx.tmp"),
            "error chain should mention tmp path; got: {msg}",
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_against_real_fixture_produces_valid_idx() {
        let fixture = resolve_fixture_path();
        if !fixture.exists() {
            eprintln!(
                "fixture {} missing — skipping run_against_real_fixture. \
                 Set HLS_TEST_FIXTURE_MP4 or place the fixture at the default path.",
                fixture.display(),
            );
            return;
        }

        let dir = unique_dir("real-fixture");
        let out = dir.join("fixture.idx");
        let mut stderr_buf: Vec<u8> = Vec::new();
        run(&fixture, &out, 6.0, &mut stderr_buf).expect("run");

        assert!(out.exists(), "output .idx must exist");
        let file = File::open(&out).expect("open out");
        // SAFETY: read-only mmap of a regular file we just wrote.
        let mmap = unsafe { Mmap::map(&file) }.expect("mmap out");
        let view = IndexView::open(&mmap).expect("open view");
        assert!(view.segment_count() > 0);
        assert!(view.max_segment_size() > 0);
        // sample.mp4 carries an `stss`, so the heuristic must not fire and
        // stderr must remain empty.
        assert!(
            stderr_buf.is_empty(),
            "real fixture has stss; expected no stderr note, got: {}",
            String::from_utf8_lossy(&stderr_buf),
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn resolve_fixture_path() -> PathBuf {
        if let Ok(env) = std::env::var("HLS_TEST_FIXTURE_MP4") {
            return PathBuf::from(env);
        }
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&manifest);
        workspace_root
            .join("tests")
            .join("fixtures")
            .join("sample.mp4")
    }
}
