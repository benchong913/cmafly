//! `.idx` round-trip integration test.
//!
//! Two modes:
//! 1. **Hand-crafted in-memory fixture** (always runs): synthesises a
//!    minimal MP4 with 1 video + 1 audio track, runs
//!    `IndexBuilder::build`, re-opens via `IndexView::open`, and asserts
//!    every accessor returns data consistent with the input. This proves
//!    the format spine end-to-end without depending on external files.
//! 2. **Real-fixture branch** (skipped with a stderr note when the file
//!    is missing): mmaps the MP4 path resolved from
//!    `HLS_TEST_FIXTURE_MP4` (default workspace-relative
//!    `tests/fixtures/sample.mp4`), runs the same pipeline, and
//!    cross-checks against ffprobe-derived expectations for the primary
//!    fixture (1920×1080 `avc1`, 48 kHz / 2 ch `mp4a`,
//!    container ≈ 30.527 s; `elst` / `ctts` / `pasp` non-empty).
//!
//! Both modes also feed the resulting `IndexView` back to
//! `write_init_segment` and `write_media_segment` to exercise the
//! always-on box-size assertion end-to-end on every emitted box.

use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use byteorder::{BigEndian, ByteOrder};

use cmafly::{IndexBuilder, IndexView, ReadAt, fmp4, playlist};

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

// -- synthetic-MP4 builders --------------------------------------------------

fn make_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

fn make_full_box(fourcc: &[u8; 4], version: u8, flags: [u8; 3], body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + body.len());
    payload.push(version);
    payload.extend_from_slice(&flags);
    payload.extend_from_slice(body);
    make_box(fourcc, &payload)
}

fn ftyp() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"mp42");
    body.extend_from_slice(&0u32.to_be_bytes());
    body.extend_from_slice(b"isom");
    body.extend_from_slice(b"mp42");
    make_box(b"ftyp", &body)
}

fn tkhd_v0(track_id: u32, width: u32, height: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(80);
    body.extend_from_slice(&[0u8; 8]); // ctime + mtime
    body.extend_from_slice(&track_id.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]); // reserved
    body.extend_from_slice(&[0u8; 4]); // duration
    body.extend_from_slice(&[0u8; 8]); // reserved
    body.extend_from_slice(&[0u8; 2]); // layer
    body.extend_from_slice(&[0u8; 2]); // alternate_group
    body.extend_from_slice(&[0u8; 2]); // volume
    body.extend_from_slice(&[0u8; 2]); // reserved
    body.extend_from_slice(&[0u8; 36]); // matrix
    body.extend_from_slice(&(width << 16).to_be_bytes());
    body.extend_from_slice(&(height << 16).to_be_bytes());
    make_full_box(b"tkhd", 0, [0, 0, 0], &body)
}

fn mdhd_v0(timescale: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&[0u8; 4]); // ctime
    body.extend_from_slice(&[0u8; 4]); // mtime
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]); // duration
    body.extend_from_slice(&[0u8; 4]); // language + pre_defined
    make_full_box(b"mdhd", 0, [0, 0, 0], &body)
}

fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    body.extend_from_slice(&[0u8; 4]); // pre_defined
    body.extend_from_slice(handler);
    body.extend_from_slice(&[0u8; 12]); // reserved
    body.push(0);
    make_full_box(b"hdlr", 0, [0, 0, 0], &body)
}

fn avc1_sample_entry() -> Vec<u8> {
    let mut body = Vec::with_capacity(78);
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&[0, 1]); // data_reference_index
    body.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
    body.extend_from_slice(&[0x07, 0x80]); // width = 1920
    body.extend_from_slice(&[0x04, 0x38]); // height = 1080
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&[0, 1]); // frame_count
    body.extend_from_slice(&[0u8; 32]); // compressorname
    body.extend_from_slice(&[0, 24]); // depth
    body.extend_from_slice(&[0xff, 0xff]); // pre_defined
    make_box(b"avc1", &body)
}

fn mp4a_sample_entry() -> Vec<u8> {
    let mut body = Vec::with_capacity(28);
    body.extend_from_slice(&[0u8; 6]); // reserved
    body.extend_from_slice(&[0, 1]); // data_reference_index
    body.extend_from_slice(&[0, 0]); // version
    body.extend_from_slice(&[0, 0]); // revision
    body.extend_from_slice(&[0u8; 4]); // vendor
    body.extend_from_slice(&[0, 2]); // channel_count = 2
    body.extend_from_slice(&[0, 16]); // samplesize
    body.extend_from_slice(&[0, 0]); // compression_id
    body.extend_from_slice(&[0, 0]); // packet_size
    body.extend_from_slice(&(48_000u32 << 16).to_be_bytes());
    make_box(b"mp4a", &body)
}

fn stsd(entry: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(entry);
    make_full_box(b"stsd", 0, [0, 0, 0], &body)
}

fn stts_constant(count: u32, delta: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    body.extend_from_slice(&count.to_be_bytes());
    body.extend_from_slice(&delta.to_be_bytes());
    make_full_box(b"stts", 0, [0, 0, 0], &body)
}

fn stsc_one_chunk(samples_per_chunk: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    body.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
    body.extend_from_slice(&samples_per_chunk.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
    make_full_box(b"stsc", 0, [0, 0, 0], &body)
}

fn stsz_constant(sample_size: u32, sample_count: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&sample_size.to_be_bytes());
    body.extend_from_slice(&sample_count.to_be_bytes());
    make_full_box(b"stsz", 0, [0, 0, 0], &body)
}

fn stco(offset: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    body.extend_from_slice(&offset.to_be_bytes());
    make_full_box(b"stco", 0, [0, 0, 0], &body)
}

fn stss(samples_one_indexed: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(samples_one_indexed.len() as u32).to_be_bytes());
    for s in samples_one_indexed {
        body.extend_from_slice(&s.to_be_bytes());
    }
    make_full_box(b"stss", 0, [0, 0, 0], &body)
}

#[allow(clippy::too_many_arguments)]
fn build_stbl(
    sample_entry: &[u8],
    sample_count: u32,
    sample_size: u32,
    dts_delta: u32,
    chunk_offset: u32,
    stss_entries: Option<&[u32]>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&stsd(sample_entry));
    payload.extend_from_slice(&stts_constant(sample_count, dts_delta));
    payload.extend_from_slice(&stsc_one_chunk(sample_count));
    payload.extend_from_slice(&stsz_constant(sample_size, sample_count));
    payload.extend_from_slice(&stco(chunk_offset));
    if let Some(samples) = stss_entries {
        payload.extend_from_slice(&stss(samples));
    }
    make_box(b"stbl", &payload)
}

#[allow(clippy::too_many_arguments)]
fn build_trak(
    track_id: u32,
    width: u32,
    height: u32,
    handler: &[u8; 4],
    timescale: u32,
    sample_entry: &[u8],
    sample_count: u32,
    sample_size: u32,
    dts_delta: u32,
    chunk_offset: u32,
    stss_entries: Option<&[u32]>,
) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&tkhd_v0(track_id, width, height));
    let stbl = build_stbl(
        sample_entry,
        sample_count,
        sample_size,
        dts_delta,
        chunk_offset,
        stss_entries,
    );
    let minf = make_box(b"minf", &stbl);
    let mut mdia_payload = Vec::new();
    mdia_payload.extend_from_slice(&mdhd_v0(timescale));
    mdia_payload.extend_from_slice(&hdlr(handler));
    mdia_payload.extend_from_slice(&minf);
    payload.extend_from_slice(&make_box(b"mdia", &mdia_payload));
    make_box(b"trak", &payload)
}

/// Build a 6-video / 6-audio synthetic MP4. The mdat is laid down last
/// (trailing-moov) so chunk offsets reach into a section the builder can
/// stream-read.
///
/// Timing summary:
/// - Video timescale 30, dts_delta 10 → 6 samples = 60 ticks = 2 s.
/// - Sync samples at 1-indexed positions 1 and 4 → 0-indexed [0, 3].
/// - Audio timescale 48 000, dts_delta 16 000 → 6 samples = 96 000 ticks
///   ≈ 2 s.
/// - Nominal cut 0.5 s = 15 video ticks → snaps forward to the second
///   sync at video-DTS 30 → segments [0..3) and [3..6).
/// - Audio split at video DTS 30 = 30 / 30 × 48 000 = 48 000 audio ticks.
///   After audio sample 3 cumulative DTS = 48 000 → audio segments
///   [0..3) and [3..6).
fn synthetic_mp4() -> SyntheticMp4 {
    let video_count = 6u32;
    let video_size = 8u32; // each video sample 8 B
    let audio_count = 6u32;
    let audio_size = 4u32;
    let video_payload_bytes = video_count * video_size;
    let audio_payload_bytes = audio_count * audio_size;
    let mdat_payload_len = video_payload_bytes + audio_payload_bytes;

    // Compute moov size first so we know where mdat lands.
    // We build moov twice: once with placeholder offsets to size it,
    // once with the resolved offsets. This is fine because the box sizes
    // do not depend on the offset values.
    let placeholder_video_offset = 0u32;
    let placeholder_audio_offset = 0u32;
    let moov_placeholder = build_moov(
        placeholder_video_offset,
        placeholder_audio_offset,
        video_count,
        video_size,
        audio_count,
        audio_size,
    );
    let ftyp_bytes = ftyp();
    let mdat_header_offset = (ftyp_bytes.len() + moov_placeholder.len()) as u32;
    let mdat_payload_offset = mdat_header_offset + 8;
    let video_chunk_offset = mdat_payload_offset;
    let audio_chunk_offset = mdat_payload_offset + video_payload_bytes;

    let moov = build_moov(
        video_chunk_offset,
        audio_chunk_offset,
        video_count,
        video_size,
        audio_count,
        audio_size,
    );
    assert_eq!(moov.len(), moov_placeholder.len(), "moov layout stable");

    // mdat payload: 6 video samples then 6 audio samples.
    let mut mdat_payload = Vec::with_capacity(mdat_payload_len as usize);
    for i in 0..video_count {
        let mut tag = [b'V', b'D', b'_', b'_', b'_', b'_', b'_', b'0' + i as u8];
        tag[2] = b'0' + (i / 10) as u8;
        tag[3] = b'0' + (i % 10) as u8;
        mdat_payload.extend_from_slice(&tag);
    }
    for i in 0..audio_count {
        let tag = [b'A', b'D', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];
        mdat_payload.extend_from_slice(&tag);
    }
    let mdat_bytes = make_box(b"mdat", &mdat_payload);

    let mut bytes = Vec::with_capacity(ftyp_bytes.len() + moov.len() + mdat_bytes.len());
    bytes.extend_from_slice(&ftyp_bytes);
    bytes.extend_from_slice(&moov);
    bytes.extend_from_slice(&mdat_bytes);

    SyntheticMp4 {
        bytes,
        video_count,
        video_size,
        audio_count,
        audio_size,
        video_chunk_offset,
        audio_chunk_offset,
    }
}

fn build_moov(
    video_chunk_offset: u32,
    audio_chunk_offset: u32,
    video_count: u32,
    video_size: u32,
    audio_count: u32,
    audio_size: u32,
) -> Vec<u8> {
    let video_trak = build_trak(
        1,
        1920,
        1080,
        b"vide",
        30,
        &avc1_sample_entry(),
        video_count,
        video_size,
        10,
        video_chunk_offset,
        Some(&[1, 4]),
    );
    let audio_trak = build_trak(
        2,
        0,
        0,
        b"soun",
        48_000,
        &mp4a_sample_entry(),
        audio_count,
        audio_size,
        16_000,
        audio_chunk_offset,
        None,
    );
    let mut moov_payload = Vec::new();
    moov_payload.extend_from_slice(&video_trak);
    moov_payload.extend_from_slice(&audio_trak);
    make_box(b"moov", &moov_payload)
}

struct SyntheticMp4 {
    bytes: Vec<u8>,
    video_count: u32,
    video_size: u32,
    audio_count: u32,
    audio_size: u32,
    video_chunk_offset: u32,
    audio_chunk_offset: u32,
}

// -- the actual round-trip tests --------------------------------------------

#[test]
fn round_trip_hand_crafted_fixture() {
    let mp4 = synthetic_mp4();
    let reader = SliceReader(&mp4.bytes);
    let idx_bytes = IndexBuilder::build(&reader, mp4.bytes.len() as u64, 0.5).expect("build");

    // The unsafe slice cast in `IndexView::open` requires the buffer's
    // start address to be 8-byte-aligned. `Vec<u8>::as_ptr()` is only
    // 1-aligned by the Rust spec; copy into a `Vec<u64>` to guarantee
    // ≥ 8-byte alignment on every target.
    let aligned = align_buf(&idx_bytes);
    let view = IndexView::open(&aligned).expect("open");

    assert_eq!(view.source_mp4_len(), mp4.bytes.len() as u64);
    let expected_blake3 = *blake3::hash(&mp4.bytes).as_bytes();
    assert_eq!(view.source_mp4_blake3(), &expected_blake3);

    let video = view.video_track();
    assert_eq!(video.timescale, 30);
    assert_eq!(video.fourcc, *b"avc1");
    assert_eq!(video.width, 1920);
    assert_eq!(video.height, 1080);
    assert_eq!(
        BigEndian::read_u32(&video.sample_entry[..4]) as usize,
        video.sample_entry.len()
    );
    assert_eq!(&video.sample_entry[4..8], b"avc1");
    assert!(video.elst.is_empty());

    let audio = view.audio_track();
    assert_eq!(audio.timescale, 48_000);
    assert_eq!(audio.fourcc, *b"mp4a");
    assert_eq!(audio.sample_rate, 48_000);
    assert_eq!(audio.channel_count, 2);
    assert_eq!(&audio.sample_entry[4..8], b"mp4a");
    assert!(audio.elst.is_empty());

    let video_samples = view.video_samples();
    assert_eq!(video_samples.len(), mp4.video_count as usize);
    for (i, s) in video_samples.iter().enumerate() {
        assert_eq!(
            s.offset,
            u64::from(mp4.video_chunk_offset) + (i as u64) * u64::from(mp4.video_size),
            "video sample {i} offset",
        );
        assert_eq!(s.size, mp4.video_size);
        assert_eq!(s.dts_delta, 10);
        assert_eq!(s.cts_offset, 0);
        let expected_sync = i == 0 || i == 3;
        assert_eq!(s.is_sync(), expected_sync, "sample {i} sync");
    }

    let audio_samples = view.audio_samples();
    assert_eq!(audio_samples.len(), mp4.audio_count as usize);
    for (i, s) in audio_samples.iter().enumerate() {
        assert_eq!(
            s.offset,
            u64::from(mp4.audio_chunk_offset) + (i as u64) * u64::from(mp4.audio_size),
            "audio sample {i} offset",
        );
        assert_eq!(s.size, mp4.audio_size);
        assert_eq!(s.dts_delta, 16_000);
        assert_eq!(s.cts_offset, 0);
        assert!(s.is_sync(), "audio frames are independently decodable");
    }

    let segments = view.segments();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].video_sample_start, 0);
    assert_eq!(segments[0].video_sample_count, 3);
    assert_eq!(segments[0].audio_sample_start, 0);
    assert_eq!(segments[0].audio_sample_count, 3);
    assert_eq!(segments[0].video_base_dts, 0);
    assert_eq!(segments[0].audio_base_dts, 0);
    assert_eq!(segments[1].video_sample_start, 3);
    assert_eq!(segments[1].video_sample_count, 3);
    assert_eq!(segments[1].audio_sample_start, 3);
    assert_eq!(segments[1].audio_sample_count, 3);
    assert_eq!(segments[1].video_base_dts, 30);
    assert_eq!(segments[1].audio_base_dts, 48_000);
    assert_eq!(view.segment_count(), 2);

    // playlist_bytes round-trip: builder embeds the playlist via the
    // optional KIND_PLAYLIST_BYTES section. Re-rendering through the
    // public writer must agree with the stored bytes once the trailing
    // `\n` alignment pad is trimmed (RFC 8216 §4.1 blank lines).
    let stored_playlist = view
        .playlist_bytes()
        .expect("builder embeds the playlist section");
    let mut canonical_playlist = Vec::new();
    playlist::write_media_playlist(&view, &mut canonical_playlist).expect("playlist");
    assert_eq!(
        trim_trailing_lf(stored_playlist),
        canonical_playlist.as_slice(),
        "stored playlist must round-trip to canonical text",
    );
    assert!(stored_playlist.len().is_multiple_of(8));
    assert!(canonical_playlist.starts_with(b"#EXTM3U\n"));
    assert!(canonical_playlist.ends_with(b"#EXT-X-ENDLIST\n"));

    // init_segment_bytes: re-render via the public writer over the view
    // we just opened; bytes must match exactly. Trailing pad would surface
    // as a length difference here.
    let mut init_round_trip = Vec::new();
    fmp4::write_init_segment(&view, &mut Cursor::new(&mut init_round_trip)).expect("init");
    assert_eq!(view.init_segment_bytes(), init_round_trip.as_slice());

    // max_segment_size: every segment, written via `write_media_segment`,
    // must produce ≤ max_segment_size bytes. Walking both segments
    // exercises the always-on box-size assertion in every emitted box,
    // and pins `max_segment_size` to the largest observed write so
    // shrinking it later would fail this round-trip.
    let max = view.max_segment_size();
    assert!(max > 0, "max_segment_size must be populated");
    let mut observed_max = 0u32;
    for idx in 0..view.segment_count() {
        let mut buf = Vec::new();
        fmp4::write_media_segment(&view, idx, &reader, &mut Cursor::new(&mut buf)).expect("media");
        let written = u32::try_from(buf.len()).expect("synthetic segment fits u32");
        observed_max = observed_max.max(written);
        assert!(
            written <= max,
            "segment {idx} byte count {written} exceeds max_segment_size {max}",
        );
    }
    assert_eq!(
        max, observed_max,
        "max_segment_size must equal the largest emitted segment",
    );
}

#[test]
fn round_trip_real_fixture() {
    let path = resolve_fixture_path();
    if !path.exists() {
        eprintln!(
            "fixture {} missing — skipping round_trip_real_fixture. \
             Run `ln -s ../../file_example_MP4_1920_18MG.mp4 tests/fixtures/sample.mp4` \
             from the workspace root, or set HLS_TEST_FIXTURE_MP4 to override.",
            path.display(),
        );
        return;
    }

    let file = std::fs::File::open(&path).expect("open fixture");
    // SAFETY: `path` is a regular file we just opened; mmap is read-only.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.expect("mmap fixture");
    let len = mmap.len() as u64;
    let idx_bytes = IndexBuilder::build(&mmap, len, 6.0).expect("build");
    let aligned = align_buf(&idx_bytes);
    let view = IndexView::open(&aligned).expect("open");

    assert_eq!(view.source_mp4_len(), len);
    let expected_blake3 = *blake3::hash(&mmap).as_bytes();
    assert_eq!(view.source_mp4_blake3(), &expected_blake3);

    // ffprobe-derived expectations for the primary 30 s H.264 sample
    // fixture. `mdhd.timescale` is sourced from the file, not the
    // codec's nominal 90 kHz: this fixture happens to use a frame-rate
    // timescale.
    let video = view.video_track();
    assert_eq!(video.fourcc, *b"avc1");
    assert_eq!(video.timescale, 30);
    assert_eq!(video.width, 1920);
    assert_eq!(video.height, 1080);
    assert!(
        !video.elst.is_empty(),
        "sample.mp4 has an edit list (B-frame composition compensation)",
    );
    assert!(
        contains_subsequence(video.sample_entry, b"pasp"),
        "video sample-entry should carry the `pasp` sibling",
    );
    assert!(
        view.video_samples().iter().any(|s| s.cts_offset != 0),
        "sample.mp4 has B-frames → expect at least one nonzero `ctts` offset",
    );

    let audio = view.audio_track();
    assert_eq!(audio.fourcc, *b"mp4a");
    assert_eq!(audio.timescale, 48_000);
    assert_eq!(audio.sample_rate, 48_000);
    assert_eq!(audio.channel_count, 2);
    assert!(
        !audio.elst.is_empty(),
        "sample.mp4 audio carries an `elst` (AAC priming)",
    );

    assert!(view.segment_count() > 0);
    assert!(view.max_segment_size() > 0);

    // Box-size invariant exercised end-to-end: write every segment and
    // confirm none exceeds max_segment_size; the always-on `assert!` in
    // `BoxWriter::finish` fires if the writer's byte count disagrees with
    // any declared size.
    let max = view.max_segment_size();
    let mut observed_max = 0u32;
    for idx in 0..view.segment_count() {
        let mut buf: Vec<u8> = Vec::with_capacity(max as usize);
        fmp4::write_media_segment(&view, idx, &mmap, &mut Cursor::new(&mut buf))
            .expect("write_media_segment");
        let written = u32::try_from(buf.len()).expect("real-fixture segment fits u32");
        observed_max = observed_max.max(written);
        assert!(
            written <= max,
            "segment {idx} ({written} B) exceeds max_segment_size ({max} B)",
        );
    }
    assert_eq!(
        max, observed_max,
        "max_segment_size must equal the largest emitted segment",
    );

    let mut init_round_trip = Vec::new();
    fmp4::write_init_segment(&view, &mut Cursor::new(&mut init_round_trip)).expect("init");
    assert_eq!(view.init_segment_bytes(), init_round_trip.as_slice());

    let stored_playlist = view
        .playlist_bytes()
        .expect("builder embeds the playlist section");
    let mut canonical_playlist = Vec::new();
    playlist::write_media_playlist(&view, &mut canonical_playlist).expect("playlist");
    assert_eq!(
        trim_trailing_lf(stored_playlist),
        canonical_playlist.as_slice(),
        "real-fixture playlist must round-trip to canonical text",
    );
    assert!(stored_playlist.len().is_multiple_of(8));
    assert!(canonical_playlist.starts_with(b"#EXTM3U\n"));
    assert!(canonical_playlist.ends_with(b"#EXT-X-ENDLIST\n"));
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

fn align_buf(bytes: &[u8]) -> AlignedBytes {
    AlignedBytes::from_bytes(bytes)
}

struct AlignedBytes {
    backing: Vec<u64>,
    len: usize,
}

impl AlignedBytes {
    fn from_bytes(bytes: &[u8]) -> Self {
        let words = bytes.len().div_ceil(8);
        let mut backing = vec![0u64; words];
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
}

impl std::ops::Deref for AlignedBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.backing.as_ptr().cast::<u8>(), self.len) }
    }
}

fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn trim_trailing_lf(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    // Restore the single trailing `\n` that terminates the canonical
    // `#EXT-X-ENDLIST\n` line — only the alignment-pad newlines beyond
    // it should be trimmed.
    if end < bytes.len() {
        end += 1;
    }
    &bytes[..end]
}
