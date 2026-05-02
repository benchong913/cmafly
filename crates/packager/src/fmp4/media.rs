//! CMAF media-segment writer.
//!
//! Emits one segment as `styp` + `moof` + `mdat`, streaming sample bytes
//! directly from the source MP4 via [`crate::ReadAt`] — no in-memory copy
//! of the `mdat` payload. The `trun.data_offset` fields are seek-patched
//! after the `mdat` header is written so video and audio data offsets
//! point into the correct strides relative to `moof` start.
//!
//! Track ordering, default-sample-flags, and `first-sample-flags-present`
//! semantics match the init writer ([`super::init`]) and the `mvex/trex`
//! defaults it lays down.

use std::io::{self, Seek, SeekFrom, Write};

use byteorder::{BigEndian, WriteBytesExt};

use crate::ReadAt;
use crate::error::PackagerError;
use crate::index::format::{SampleEntry, SegmentEntry};
use crate::index::view::IndexView;

use super::boxes::{BoxWriter, write_full_box_header};

const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;

// `tfhd` flag: default-base-is-moof. No defaults are inherited from `tfhd`
// itself; per-sample duration / size come from `trun`, default flags come
// from `trex` in the init segment.
const TFHD_FLAG_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

// `trun` flags. The video set always carries data-offset, first-sample-flags,
// per-sample duration, and per-sample size. The audio set carries data-offset
// + per-sample duration + per-sample size only — no per-sample flags, no
// first-sample override.
const TRUN_FLAG_DATA_OFFSET: u32 = 0x0000_0001;
const TRUN_FLAG_FIRST_SAMPLE_FLAGS: u32 = 0x0000_0004;
const TRUN_FLAG_SAMPLE_DURATION: u32 = 0x0000_0100;
const TRUN_FLAG_SAMPLE_SIZE: u32 = 0x0000_0200;
const TRUN_FLAG_SAMPLE_CTS_OFFSET: u32 = 0x0000_0800;

const VIDEO_TRUN_BASE_FLAGS: u32 = TRUN_FLAG_DATA_OFFSET
    | TRUN_FLAG_FIRST_SAMPLE_FLAGS
    | TRUN_FLAG_SAMPLE_DURATION
    | TRUN_FLAG_SAMPLE_SIZE;
const AUDIO_TRUN_FLAGS: u32 =
    TRUN_FLAG_DATA_OFFSET | TRUN_FLAG_SAMPLE_DURATION | TRUN_FLAG_SAMPLE_SIZE;

// First-sample-flags marking the leading IDR as a sync sample
// (sample_depends_on = 2 / independent, sample_is_non_sync_sample = 0).
const VIDEO_FIRST_SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;

// Streaming buffer for `ReadAt` → `Write` sample copies. 64 KiB is large
// enough to amortise syscalls / branch overhead for typical fragments
// (video samples ≲ 200 KiB at 4 Mbps / 30 fps × 6 s) without inflating
// the writer's stack usage. The buffer lives once per segment write.
const COPY_BUF_SIZE: usize = 64 * 1024;

/// Write one media segment (`styp` + `moof` + `mdat`) for `segment_idx`
/// into `out`, streaming sample payload from `sample_data`.
///
/// Called per request by `cmafly-serve`. Returns
/// [`PackagerError::SegmentIndexOutOfRange`] when `segment_idx` is past the
/// recorded segment table; all other errors are I/O failures wrapped via
/// `?`.
pub fn write_media_segment<W: Write + Seek, R: ReadAt + ?Sized>(
    index: &IndexView<'_>,
    segment_idx: u32,
    sample_data: &R,
    out: &mut W,
) -> Result<(), PackagerError> {
    let segments = index.segments();
    let segment_count = index.segment_count();
    let segment: &SegmentEntry =
        segments
            .get(segment_idx as usize)
            .ok_or(PackagerError::SegmentIndexOutOfRange {
                idx: segment_idx,
                count: segment_count,
            })?;

    let video_samples =
        slice_segment_samples(index.video_samples(), segment, /* video: */ true)?;
    let audio_samples =
        slice_segment_samples(index.audio_samples(), segment, /* video: */ false)?;

    if video_samples.is_empty() || audio_samples.is_empty() {
        return Err(PackagerError::SampleTableInconsistent(
            "media segment with zero video or audio samples",
        ));
    }

    // Track-level `ctts` presence: when the source track has no `ctts`,
    // every sample's `cts_offset` is 0. Scan the whole video sample table so
    // every emitted segment's `trun` matches the source track, instead of
    // drifting per-segment based on the local sample window.
    let video_has_ctts = index.video_samples().iter().any(|s| s.cts_offset != 0);

    let video_payload_bytes: u64 = sum_sizes(video_samples)?;
    let audio_payload_bytes: u64 = sum_sizes(audio_samples)?;

    write_styp(out)?;

    let moof_start = out.stream_position()?;
    let video_data_offset_pos: u64;
    let audio_data_offset_pos: u64;
    {
        let mut moof = BoxWriter::open(out, *b"moof")?;
        write_mfhd(&mut moof, segment_idx)?;
        video_data_offset_pos =
            write_traf(&mut moof, VIDEO_TRACK_ID, segment.video_base_dts, |traf| {
                write_video_trun(traf, video_samples, video_has_ctts)
            })?;
        audio_data_offset_pos =
            write_traf(&mut moof, AUDIO_TRACK_ID, segment.audio_base_dts, |traf| {
                write_audio_trun(traf, audio_samples)
            })?;
        moof.finish()?;
    }

    let mdat_header_start = out.stream_position()?;
    let mdat_payload_start = mdat_header_start + 8;
    let payload_bytes = video_payload_bytes
        .checked_add(audio_payload_bytes)
        .ok_or_else(|| io::Error::other("media segment payload byte count overflows u64"))?;
    let mdat_total_size_u64 = payload_bytes
        .checked_add(8)
        .ok_or_else(|| io::Error::other("media segment mdat size overflows u64"))?;
    let mdat_total_size =
        u32::try_from(mdat_total_size_u64).map_err(|_| segment_too_large(payload_bytes))?;

    {
        let mut mdat = BoxWriter::open(out, *b"mdat")?;
        let mut buf = vec![0u8; COPY_BUF_SIZE];
        for sample in video_samples {
            stream_sample(sample_data, mdat.writer(), sample, &mut buf)?;
        }
        for sample in audio_samples {
            stream_sample(sample_data, mdat.writer(), sample, &mut buf)?;
        }
        mdat.finish_expecting(mdat_total_size)?;
    }

    let video_data_offset =
        compute_data_offset(mdat_payload_start, moof_start, /* trailing: */ 0)?;
    let audio_data_offset =
        compute_data_offset(mdat_payload_start, moof_start, video_payload_bytes)?;

    let after = out.stream_position()?;
    out.seek(SeekFrom::Start(video_data_offset_pos))?;
    out.write_i32::<BigEndian>(video_data_offset)?;
    out.seek(SeekFrom::Start(audio_data_offset_pos))?;
    out.write_i32::<BigEndian>(audio_data_offset)?;
    out.seek(SeekFrom::Start(after))?;

    Ok(())
}

fn slice_segment_samples<'a>(
    table: &'a [SampleEntry],
    segment: &SegmentEntry,
    video: bool,
) -> Result<&'a [SampleEntry], PackagerError> {
    let (start, count) = if video {
        (segment.video_sample_start, segment.video_sample_count)
    } else {
        (segment.audio_sample_start, segment.audio_sample_count)
    };
    let start_us = start as usize;
    let end_us =
        start_us
            .checked_add(count as usize)
            .ok_or(PackagerError::SampleTableInconsistent(
                "segment sample range overflows usize",
            ))?;
    if end_us > table.len() {
        return Err(PackagerError::SampleTableInconsistent(
            "segment sample range exceeds sample table length",
        ));
    }
    Ok(&table[start_us..end_us])
}

fn sum_sizes(samples: &[SampleEntry]) -> Result<u64, PackagerError> {
    let mut total: u64 = 0;
    for s in samples {
        total =
            total
                .checked_add(u64::from(s.size))
                .ok_or(PackagerError::SampleTableInconsistent(
                    "segment payload byte count overflows u64",
                ))?;
    }
    Ok(total)
}

fn segment_too_large(payload_bytes: u64) -> io::Error {
    io::Error::other(format!(
        "media segment mdat size {} exceeds u32::MAX (max_segment_size invariant violated)",
        payload_bytes + 8,
    ))
}

fn compute_data_offset(
    mdat_payload_start: u64,
    moof_start: u64,
    trailing: u64,
) -> Result<i32, PackagerError> {
    let absolute = mdat_payload_start
        .checked_sub(moof_start)
        .and_then(|v| v.checked_add(trailing))
        .ok_or_else(|| io::Error::other("trun.data_offset arithmetic overflow"))?;
    i32::try_from(absolute).map_err(|_| {
        PackagerError::Io(io::Error::other(
            "trun.data_offset exceeds i32::MAX (segment too large)",
        ))
    })
}

fn write_styp<W: Write + Seek>(out: &mut W) -> io::Result<()> {
    let mut bx = BoxWriter::open(out, *b"styp")?;
    let w = bx.writer();
    w.write_all(b"msdh")?;
    w.write_u32::<BigEndian>(0)?;
    w.write_all(b"msdh")?;
    w.write_all(b"cmfc")?;
    bx.finish()?;
    Ok(())
}

fn write_mfhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>, segment_idx: u32) -> io::Result<()> {
    let mut bx = parent.child(*b"mfhd")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 0)?;
    let sequence_number = segment_idx.saturating_add(1);
    w.write_u32::<BigEndian>(sequence_number)?;
    bx.finish()?;
    Ok(())
}

/// Emit one `traf` block — `tfhd` + `tfdt` + a `trun` written by the caller —
/// and forward the `trun.data_offset` placeholder position back so the
/// caller can seek-patch it after `mdat` is laid down.
fn write_traf<W: Write + Seek>(
    moof: &mut BoxWriter<'_, W>,
    track_id: u32,
    base_dts: u64,
    write_trun: impl FnOnce(&mut BoxWriter<'_, W>) -> io::Result<u64>,
) -> io::Result<u64> {
    let mut traf = moof.child(*b"traf")?;
    write_tfhd(&mut traf, track_id)?;
    write_tfdt(&mut traf, base_dts)?;
    let data_offset_pos = write_trun(&mut traf)?;
    traf.finish()?;
    Ok(data_offset_pos)
}

fn write_tfhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>, track_id: u32) -> io::Result<()> {
    let mut bx = parent.child(*b"tfhd")?;
    let w = bx.writer();
    write_full_box_header(w, 0, TFHD_FLAG_DEFAULT_BASE_IS_MOOF)?;
    w.write_u32::<BigEndian>(track_id)?;
    bx.finish()?;
    Ok(())
}

fn write_tfdt<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    base_media_decode_time: u64,
) -> io::Result<()> {
    let mut bx = parent.child(*b"tfdt")?;
    let w = bx.writer();
    write_full_box_header(w, 1, 0)?;
    w.write_u64::<BigEndian>(base_media_decode_time)?;
    bx.finish()?;
    Ok(())
}

/// Returns the absolute byte position of the (4-byte) `data_offset`
/// placeholder inside `trun`, for the caller to seek-patch after `mdat`
/// is laid down.
fn write_video_trun<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    samples: &[SampleEntry],
    has_ctts: bool,
) -> io::Result<u64> {
    let mut bx = parent.child(*b"trun")?;
    let w = bx.writer();
    let version: u8 = if has_ctts { 1 } else { 0 };
    let flags = VIDEO_TRUN_BASE_FLAGS
        | if has_ctts {
            TRUN_FLAG_SAMPLE_CTS_OFFSET
        } else {
            0
        };
    write_full_box_header(w, version, flags)?;
    let sample_count = u32::try_from(samples.len()).map_err(|_| {
        io::Error::other(format!(
            "video segment sample_count {} exceeds u32::MAX",
            samples.len(),
        ))
    })?;
    w.write_u32::<BigEndian>(sample_count)?;
    let data_offset_pos = w.stream_position()?;
    w.write_u32::<BigEndian>(0)?; // data_offset placeholder
    w.write_u32::<BigEndian>(VIDEO_FIRST_SAMPLE_FLAGS_SYNC)?;
    for s in samples {
        w.write_u32::<BigEndian>(s.dts_delta)?;
        w.write_u32::<BigEndian>(s.size)?;
        if has_ctts {
            w.write_i32::<BigEndian>(s.cts_offset)?;
        }
    }
    bx.finish()?;
    Ok(data_offset_pos)
}

fn write_audio_trun<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    samples: &[SampleEntry],
) -> io::Result<u64> {
    let mut bx = parent.child(*b"trun")?;
    let w = bx.writer();
    write_full_box_header(w, 0, AUDIO_TRUN_FLAGS)?;
    let sample_count = u32::try_from(samples.len()).map_err(|_| {
        io::Error::other(format!(
            "audio segment sample_count {} exceeds u32::MAX",
            samples.len(),
        ))
    })?;
    w.write_u32::<BigEndian>(sample_count)?;
    let data_offset_pos = w.stream_position()?;
    w.write_u32::<BigEndian>(0)?; // data_offset placeholder
    for s in samples {
        w.write_u32::<BigEndian>(s.dts_delta)?;
        w.write_u32::<BigEndian>(s.size)?;
    }
    bx.finish()?;
    Ok(data_offset_pos)
}

fn stream_sample<R: ReadAt + ?Sized, W: Write>(
    src: &R,
    out: &mut W,
    sample: &SampleEntry,
    buf: &mut [u8],
) -> io::Result<()> {
    let mut remaining = sample.size as usize;
    let mut pos = sample.offset;
    while remaining > 0 {
        let chunk_len = remaining.min(buf.len());
        let n = src.read_at(pos, &mut buf[..chunk_len])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "ReadAt returned EOF mid-sample at offset {} (still need {} bytes)",
                    pos, remaining,
                ),
            ));
        }
        out.write_all(&buf[..n])?;
        pos += n as u64;
        remaining -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use byteorder::{BigEndian, ByteOrder};

    use super::*;
    use crate::demux::reader::BoxIter;
    use crate::index::view::{AudioTrackMeta, IndexView, VideoTrackMeta};
    use crate::read_at::SliceReader;

    fn vsample(offset: u64, size: u32, dts_delta: u32, cts_offset: i32, sync: bool) -> SampleEntry {
        SampleEntry {
            offset,
            size,
            dts_delta,
            cts_offset,
            flags: if sync { 1 } else { 0 },
        }
    }

    fn asample(offset: u64, size: u32, dts_delta: u32) -> SampleEntry {
        SampleEntry {
            offset,
            size,
            dts_delta,
            cts_offset: 0,
            flags: 1,
        }
    }

    /// Two-segment fixture: 4 video samples (sync at 0 and 2), 4 audio samples,
    /// segment 0 = first half, segment 1 = second half.
    struct Fixture {
        source: Vec<u8>,
        video_samples: Vec<SampleEntry>,
        audio_samples: Vec<SampleEntry>,
        segments: Vec<SegmentEntry>,
    }

    impl Fixture {
        fn build(video_has_ctts: bool) -> Self {
            // Source layout: [v0 v1 v2 v3 a0 a1 a2 a3] each 4 bytes.
            let mut source = Vec::new();
            for tag in [b"VID0", b"VID1", b"VID2", b"VID3"] {
                source.extend_from_slice(tag);
            }
            for tag in [b"AUD0", b"AUD1", b"AUD2", b"AUD3"] {
                source.extend_from_slice(tag);
            }
            let video = vec![
                vsample(0, 4, 100, if video_has_ctts { 50 } else { 0 }, true),
                vsample(4, 4, 100, if video_has_ctts { -10 } else { 0 }, false),
                vsample(8, 4, 100, if video_has_ctts { 20 } else { 0 }, true),
                // Last sample's `cts_offset` is 0 in either fixture; a non-zero
                // earlier sample is enough to flip the track-level `has_ctts`.
                vsample(12, 4, 100, 0, false),
            ];
            let audio = vec![
                asample(16, 4, 50),
                asample(20, 4, 50),
                asample(24, 4, 50),
                asample(28, 4, 50),
            ];
            let segments = vec![
                SegmentEntry {
                    video_sample_start: 0,
                    video_sample_count: 2,
                    audio_sample_start: 0,
                    audio_sample_count: 2,
                    video_base_dts: 0,
                    audio_base_dts: 0,
                },
                SegmentEntry {
                    video_sample_start: 2,
                    video_sample_count: 2,
                    audio_sample_start: 2,
                    audio_sample_count: 2,
                    video_base_dts: 200,
                    audio_base_dts: 100,
                },
            ];
            Self {
                source,
                video_samples: video,
                audio_samples: audio,
                segments,
            }
        }
    }

    fn fake_avc1() -> Vec<u8> {
        // Minimal avc1 byte-blob; the writer copies it verbatim and never
        // parses the payload, so the contents only need a valid box header.
        let mut bx = vec![0u8; 16];
        BigEndian::write_u32(&mut bx[..4], 16);
        bx[4..8].copy_from_slice(b"avc1");
        bx
    }

    fn fake_mp4a() -> Vec<u8> {
        let mut bx = vec![0u8; 16];
        BigEndian::write_u32(&mut bx[..4], 16);
        bx[4..8].copy_from_slice(b"mp4a");
        bx
    }

    fn build_view<'a>(
        fixture: &'a Fixture,
        avc1: &'a [u8],
        mp4a: &'a [u8],
        blake3: &'a [u8; 32],
    ) -> IndexView<'a> {
        let video_meta = VideoTrackMeta {
            timescale: 90_000,
            fourcc: *b"avc1",
            width: 1920,
            height: 1080,
            sample_entry: avc1,
            elst: &[],
        };
        let audio_meta = AudioTrackMeta {
            timescale: 48_000,
            fourcc: *b"mp4a",
            sample_rate: 48_000,
            channel_count: 2,
            sample_entry: mp4a,
            elst: &[],
        };
        IndexView::from_parts(
            64,
            fixture.source.len() as u64,
            blake3,
            video_meta,
            audio_meta,
            &fixture.video_samples,
            &fixture.audio_samples,
            &fixture.segments,
            &[],
            None,
        )
    }

    #[test]
    fn top_level_is_styp_then_moof_then_mdat() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let reader = SliceReader(&out);
        let mut iter = BoxIter::new(&reader, 0, out.len() as u64);
        let styp = iter.next_header().unwrap().unwrap();
        let moof = iter.next_header().unwrap().unwrap();
        let mdat = iter.next_header().unwrap().unwrap();
        assert_eq!(&styp.box_type, b"styp");
        assert_eq!(&moof.box_type, b"moof");
        assert_eq!(&mdat.box_type, b"mdat");
        assert!(iter.next_header().unwrap().is_none());
    }

    #[test]
    fn styp_carries_msdh_major_and_compatible_brands() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        // styp: size + 'styp' + 'msdh' + minor + ['msdh','cmfc'] = 24 B
        assert_eq!(BigEndian::read_u32(&out[0..4]), 24);
        assert_eq!(&out[4..8], b"styp");
        assert_eq!(&out[8..12], b"msdh");
        assert_eq!(BigEndian::read_u32(&out[12..16]), 0);
        assert_eq!(&out[16..20], b"msdh");
        assert_eq!(&out[20..24], b"cmfc");
    }

    #[test]
    fn moof_contains_mfhd_and_two_traf() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 1, &src, &mut Cursor::new(&mut out)).expect("write");

        let reader = SliceReader(&out);
        let mut top = BoxIter::new(&reader, 0, out.len() as u64);
        let _styp = top.next_header().unwrap().unwrap();
        let moof = top.next_header().unwrap().unwrap();
        let mut child = BoxIter::new(&reader, moof.payload_offset, moof.end());
        let names: Vec<[u8; 4]> = std::iter::from_fn(|| child.next_header().unwrap())
            .map(|h| h.box_type)
            .collect();
        assert_eq!(names, vec![*b"mfhd", *b"traf", *b"traf"]);
    }

    #[test]
    fn mfhd_sequence_number_is_segment_idx_plus_one() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        for (idx, expected) in [(0u32, 1u32), (1, 2)] {
            let mut out: Vec<u8> = Vec::new();
            write_media_segment(&view, idx, &src, &mut Cursor::new(&mut out)).expect("write");
            let mfhd_box = locate_descendant(&out, &[*b"moof", *b"mfhd"]);
            // mfhd payload: 4 (ver+flags) + 4 (sequence_number)
            let p = mfhd_box.payload_offset as usize;
            assert_eq!(BigEndian::read_u32(&out[p + 4..p + 8]), expected);
        }
    }

    #[test]
    fn tfdt_carries_per_track_base_media_decode_time() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 1, &src, &mut Cursor::new(&mut out)).expect("write");

        // Locate both `traf`s, then their `tfdt`s.
        let trafs = collect_trafs(&out);
        assert_eq!(trafs.len(), 2);
        let video_tfdt = locate_child(&out, &trafs[0], *b"tfdt");
        let p = video_tfdt.payload_offset as usize;
        assert_eq!(BigEndian::read_u64(&out[p + 4..p + 12]), 200);

        let audio_tfdt = locate_child(&out, &trafs[1], *b"tfdt");
        let p = audio_tfdt.payload_offset as usize;
        assert_eq!(BigEndian::read_u64(&out[p + 4..p + 12]), 100);
    }

    #[test]
    fn mdat_payload_concatenates_video_then_audio_samples() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let mdat = locate_top(&out, *b"mdat");
        let p = mdat.payload_offset as usize;
        // Segment 0 = video[0..2] + audio[0..2] = "VID0VID1AUD0AUD1"
        assert_eq!(&out[p..p + 16], b"VID0VID1AUD0AUD1");
    }

    #[test]
    fn trun_data_offsets_point_into_mdat_payload() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let moof = locate_top(&out, *b"moof");
        let mdat = locate_top(&out, *b"mdat");
        let mdat_payload_abs = mdat.payload_offset;

        let trafs = collect_trafs(&out);
        let video_trun = locate_child(&out, &trafs[0], *b"trun");
        let audio_trun = locate_child(&out, &trafs[1], *b"trun");

        let video_data_offset =
            read_trun_data_offset(&out, &video_trun, /* has_first_flags: */ true);
        let audio_data_offset =
            read_trun_data_offset(&out, &audio_trun, /* has_first_flags: */ false);

        let video_video_payload_bytes = 8u64; // 2 video samples × 4 bytes
        assert_eq!(
            video_data_offset as i64,
            (mdat_payload_abs - moof.start) as i64,
        );
        assert_eq!(
            audio_data_offset as i64,
            (mdat_payload_abs + video_video_payload_bytes - moof.start) as i64,
        );

        // Sanity: feeding the offsets back into a slice from `moof` start
        // should reproduce the segment's expected sample bytes.
        let video_slice_start = (moof.start as i64 + video_data_offset as i64) as usize;
        let audio_slice_start = (moof.start as i64 + audio_data_offset as i64) as usize;
        assert_eq!(&out[video_slice_start..video_slice_start + 8], b"VID0VID1");
        assert_eq!(&out[audio_slice_start..audio_slice_start + 8], b"AUD0AUD1");
    }

    #[test]
    fn video_trun_omits_cts_offset_when_track_has_no_ctts() {
        let f = Fixture::build(/* video_has_ctts: */ false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let trafs = collect_trafs(&out);
        let video_trun = locate_child(&out, &trafs[0], *b"trun");
        let p = video_trun.payload_offset as usize;
        let version = out[p];
        let flags = BigEndian::read_u32(&out[p..p + 4]) & 0x00FF_FFFF;
        assert_eq!(version, 0);
        assert_eq!(flags & TRUN_FLAG_SAMPLE_CTS_OFFSET, 0);
    }

    #[test]
    fn video_trun_includes_cts_offset_when_track_has_ctts() {
        let f = Fixture::build(/* video_has_ctts: */ true);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let trafs = collect_trafs(&out);
        let video_trun = locate_child(&out, &trafs[0], *b"trun");
        let p = video_trun.payload_offset as usize;
        let version = out[p];
        let flags = BigEndian::read_u32(&out[p..p + 4]) & 0x00FF_FFFF;
        assert_eq!(
            version, 1,
            "version 1 → signed sample_composition_time_offset"
        );
        assert!(flags & TRUN_FLAG_SAMPLE_CTS_OFFSET != 0);

        // sample_count
        let sc_pos = p + 4;
        assert_eq!(BigEndian::read_u32(&out[sc_pos..sc_pos + 4]), 2);
        // skip data_offset(4) + first_sample_flags(4) → first sample row.
        let first_row = sc_pos + 4 + 4 + 4;
        // Each row: duration(4) + size(4) + cts_offset(4 signed)
        let cts0 = BigEndian::read_i32(&out[first_row + 8..first_row + 12]);
        assert_eq!(cts0, 50);
        let cts1 = BigEndian::read_i32(&out[first_row + 12 + 8..first_row + 12 + 12]);
        assert_eq!(cts1, -10);
    }

    #[test]
    fn video_trun_first_sample_flags_mark_idr_as_sync() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let trafs = collect_trafs(&out);
        let video_trun = locate_child(&out, &trafs[0], *b"trun");
        let p = video_trun.payload_offset as usize;
        // first_sample_flags lives after ver+flags(4) + sample_count(4) +
        // data_offset(4) (always present in our flag set).
        let first_flags = BigEndian::read_u32(&out[p + 12..p + 16]);
        assert_eq!(first_flags, VIDEO_FIRST_SAMPLE_FLAGS_SYNC);
    }

    #[test]
    fn audio_trun_omits_first_sample_flags() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        write_media_segment(&view, 0, &src, &mut Cursor::new(&mut out)).expect("write");

        let trafs = collect_trafs(&out);
        let audio_trun = locate_child(&out, &trafs[1], *b"trun");
        let p = audio_trun.payload_offset as usize;
        let flags = BigEndian::read_u32(&out[p..p + 4]) & 0x00FF_FFFF;
        assert_eq!(flags & TRUN_FLAG_FIRST_SAMPLE_FLAGS, 0);
    }

    #[test]
    fn out_of_range_segment_idx_returns_typed_error() {
        let f = Fixture::build(false);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        let mut out: Vec<u8> = Vec::new();
        let err = write_media_segment(&view, 99, &src, &mut Cursor::new(&mut out)).unwrap_err();
        match err {
            PackagerError::SegmentIndexOutOfRange { idx, count } => {
                assert_eq!(idx, 99);
                assert_eq!(count, 2);
            }
            other => panic!("expected SegmentIndexOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn writes_both_segments_without_panicking_box_size_invariant() {
        // Each emitted box runs the always-on size assertion in
        // `BoxWriter::finish` / `finish_expecting`. Walking both segments
        // exercises every box site in the writer; reaching this assertion
        // proves no invariant fired.
        let f = Fixture::build(true);
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&f, &avc1, &mp4a, &blake3);
        let src = SliceReader(&f.source);

        for idx in 0..view.segment_count() {
            let mut out: Vec<u8> = Vec::new();
            write_media_segment(&view, idx, &src, &mut Cursor::new(&mut out)).expect("write");
            assert!(out.len() > 24, "segment {idx} should contain payload");
        }
    }

    fn locate_top(buf: &[u8], target: [u8; 4]) -> crate::demux::reader::BoxHeader {
        let reader = SliceReader(buf);
        let mut iter = BoxIter::new(&reader, 0, buf.len() as u64);
        while let Some(h) = iter.next_header().unwrap() {
            if h.box_type == target {
                return h;
            }
        }
        panic!("top-level {target:?} missing");
    }

    fn locate_child(
        buf: &[u8],
        parent: &crate::demux::reader::BoxHeader,
        target: [u8; 4],
    ) -> crate::demux::reader::BoxHeader {
        let reader = SliceReader(buf);
        let mut iter = BoxIter::new(&reader, parent.payload_offset, parent.end());
        while let Some(h) = iter.next_header().unwrap() {
            if h.box_type == target {
                return h;
            }
        }
        panic!("child {target:?} missing in {:?}", parent.box_type);
    }

    fn locate_descendant(buf: &[u8], path: &[[u8; 4]]) -> crate::demux::reader::BoxHeader {
        let mut current = locate_top(buf, path[0]);
        for &kind in &path[1..] {
            current = locate_child(buf, &current, kind);
        }
        current
    }

    fn collect_trafs(buf: &[u8]) -> Vec<crate::demux::reader::BoxHeader> {
        let moof = locate_top(buf, *b"moof");
        let reader = SliceReader(buf);
        let mut iter = BoxIter::new(&reader, moof.payload_offset, moof.end());
        let mut out = Vec::new();
        while let Some(h) = iter.next_header().unwrap() {
            if &h.box_type == b"traf" {
                out.push(h);
            }
        }
        out
    }

    fn read_trun_data_offset(
        buf: &[u8],
        trun: &crate::demux::reader::BoxHeader,
        has_first_flags: bool,
    ) -> i32 {
        // trun: ver+flags(4) + sample_count(4) + data_offset(4) [+ first_sample_flags(4)]
        let p = trun.payload_offset as usize;
        let _ = has_first_flags;
        BigEndian::read_i32(&buf[p + 8..p + 12])
    }
}
