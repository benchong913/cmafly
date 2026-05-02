//! `IndexBuilder` — produce a `.idx` byte image from a source MP4.
//!
//! The builder is the single orchestrator that ties demux, segmentation,
//! the fMP4 writers, and the on-disk schema together. Output is a
//! `Vec<u8>` ready to mmap and re-open via [`IndexView::open`].
//!
//! Build pipeline:
//! 1. Walk `moov` → per-track meta + sample-table locations.
//! 2. Read sample tables → `RawSampleEntry` per track.
//! 3. Convert to on-disk [`SampleEntry`] rows (sync flag preserved as
//!    `flags` bit 0; cts_offset 0 for audio).
//! 4. IDR-aligned splitter → `[SegmentEntry]`.
//! 5. Read sample-entry / `edts` byte ranges into owned blobs.
//! 6. Build a transient [`IndexView`] over the (still-zero
//!    `max_segment_size`) parts to drive the fMP4 writers.
//! 7. Pre-render `init.mp4` via [`write_init_segment`].
//! 8. Compute `max_segment_size` by writing each segment to a counting
//!    sink (zero allocation; just tracks the high-water mark).
//! 9. Hash the source MP4 with BLAKE3-256.
//! 10. Lay out the `.idx` byte image: header + section directory +
//!     payloads, with inter-section 8-byte alignment padding.

use std::io::{self, Cursor, Seek, SeekFrom, Write};

use blake3::Hasher;
use byteorder::{BigEndian, ByteOrder, WriteBytesExt};

use crate::ReadAt;
use crate::demux::moov::{ByteRange, ParsedTrack, parse_top_level};
use crate::demux::reader::read_exact;
use crate::demux::sample_table::{RawSampleEntry, walk};
use crate::error::PackagerError;
use crate::fmp4::{write_init_segment, write_media_segment};
use crate::playlist::write_media_playlist;
use crate::segment::splitter;

use super::format::{
    HEADER_FIXED_LEN, KIND_AUDIO_SAMPLE_TABLE, KIND_AUDIO_TRACK_META, KIND_INIT_SEGMENT_BYTES,
    KIND_PLAYLIST_BYTES, KIND_SEGMENT_TABLE, KIND_VIDEO_SAMPLE_TABLE, KIND_VIDEO_TRACK_META, MAGIC,
    SAMPLE_FLAG_IS_SYNC, SECTION_DIR_ENTRY_LEN, SampleEntry, SegmentEntry,
};
use super::view::{AudioTrackMeta, IndexView, VideoTrackMeta};

/// Stream-hash buffer for [`blake3_of_source`] and [`read_byte_range`].
/// 64 KiB matches the fMP4 writer's per-segment copy buffer; sized to
/// amortise mmap page faults without inflating the builder's stack.
const COPY_BUF_SIZE: usize = 64 * 1024;

/// Produce a `.idx` byte image.
pub struct IndexBuilder;

impl IndexBuilder {
    /// Build a complete `.idx` byte image from a source MP4.
    ///
    /// `source` is a random-access reader over the entire source file
    /// (typically `Mmap`); `source_len` is its byte length;
    /// `segment_duration_secs` is the nominal cut duration. Real cuts snap
    /// forward to the next IDR.
    pub fn build<R: ReadAt + ?Sized>(
        source: &R,
        source_len: u64,
        segment_duration_secs: f64,
    ) -> Result<Vec<u8>, PackagerError> {
        let moov = parse_top_level(source, source_len)?;
        let video_walked = walk(source, &moov.video.sample_table)?;
        let audio_walked = walk(source, &moov.audio.sample_table)?;

        let video_samples: Vec<SampleEntry> = video_walked
            .samples
            .iter()
            .map(raw_to_sample_entry)
            .collect();
        let audio_samples: Vec<SampleEntry> = audio_walked
            .samples
            .iter()
            .map(raw_to_sample_entry)
            .collect();

        let segments: Vec<SegmentEntry> = splitter::split(
            &video_walked.samples,
            &audio_walked.samples,
            moov.video.timescale,
            moov.audio.timescale,
            segment_duration_secs,
        )?;

        let video_sample_entry = read_byte_range(source, moov.video.sample_entry)?;
        let video_elst = moov
            .video
            .elst
            .map(|r| read_byte_range(source, r))
            .transpose()?
            .unwrap_or_default();
        let audio_sample_entry = read_byte_range(source, moov.audio.sample_entry)?;
        let audio_elst = moov
            .audio
            .elst
            .map(|r| read_byte_range(source, r))
            .transpose()?
            .unwrap_or_default();

        let blake3_digest = blake3_of_source(source, source_len)?;

        let temp_view = IndexView::from_parts(
            0,
            source_len,
            &blake3_digest,
            VideoTrackMeta {
                timescale: moov.video.timescale,
                fourcc: moov.video.fourcc,
                width: moov.video.width,
                height: moov.video.height,
                sample_entry: &video_sample_entry,
                elst: &video_elst,
            },
            AudioTrackMeta {
                timescale: moov.audio.timescale,
                fourcc: moov.audio.fourcc,
                sample_rate: moov.audio.sample_rate,
                channel_count: moov.audio.channel_count,
                sample_entry: &audio_sample_entry,
                elst: &audio_elst,
            },
            &video_samples,
            &audio_samples,
            &segments,
            &[],
            None,
        );

        let mut init_bytes: Vec<u8> = Vec::new();
        write_init_segment(&temp_view, &mut Cursor::new(&mut init_bytes))?;

        let playlist_bytes = render_playlist(&temp_view)?;

        let mut max_segment_size: u32 = 0;
        for idx in 0..segments.len() {
            let mut sink = CountingSink::default();
            let segment_idx = u32::try_from(idx).map_err(|_| {
                PackagerError::SampleTableInconsistent("segment index exceeds u32::MAX")
            })?;
            write_media_segment(&temp_view, segment_idx, source, &mut sink)?;
            let bytes_for_segment = u32::try_from(sink.len).map_err(|_| {
                PackagerError::Io(io::Error::other(format!(
                    "media segment {idx} size {} exceeds u32::MAX (max_segment_size invariant)",
                    sink.len,
                )))
            })?;
            if bytes_for_segment > max_segment_size {
                max_segment_size = bytes_for_segment;
            }
        }

        serialise(
            max_segment_size,
            source_len,
            &blake3_digest,
            &moov.video,
            &video_sample_entry,
            &video_elst,
            &moov.audio,
            &audio_sample_entry,
            &audio_elst,
            &video_samples,
            &audio_samples,
            &segments,
            &playlist_bytes,
            &init_bytes,
        )
    }
}

/// Render the media playlist and pad the byte image with trailing `\n`
/// to the next 8-byte boundary.
///
/// The padding lives in the section content rather than as inter-section
/// zero pad: `IndexView::playlist_bytes()` returns the section's raw
/// bytes (no length-prefix demarcation in the on-disk schema), so any
/// trailing zeros from the builder's alignment loop would surface in the
/// playlist body the server forwards to clients. RFC 8216 §4.1 treats
/// blank lines as ignored, so trailing `\n` characters are
/// parser-neutral; trailing NUL bytes are not.
fn render_playlist(view: &IndexView<'_>) -> Result<Vec<u8>, PackagerError> {
    let mut bytes: Vec<u8> = Vec::new();
    write_media_playlist(view, &mut bytes)?;
    let rem = bytes.len() & 7;
    if rem != 0 {
        bytes.resize(bytes.len() + (8 - rem), b'\n');
    }
    debug_assert!(bytes.len().is_multiple_of(8));
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn serialise(
    max_segment_size: u32,
    source_len: u64,
    blake3: &[u8; 32],
    video_meta: &ParsedTrack,
    video_sample_entry: &[u8],
    video_elst: &[u8],
    audio_meta: &ParsedTrack,
    audio_sample_entry: &[u8],
    audio_elst: &[u8],
    video_samples: &[SampleEntry],
    audio_samples: &[SampleEntry],
    segments: &[SegmentEntry],
    playlist_bytes: &[u8],
    init_bytes: &[u8],
) -> Result<Vec<u8>, PackagerError> {
    // Each section's length is implied by the next directory offset, or
    // by EOF for the final section. InitSegmentBytes sits last so no
    // trailing pad bytes leak into the bytes cmafly-serve forwards to
    // clients.
    //
    // PlaylistBytes sits second-to-last and is pre-padded with trailing
    // `\n` to 8-byte alignment by `render_playlist`, so the inter-section
    // pad below is a no-op for it and `view.playlist_bytes()` returns no
    // zero bytes.
    let sections: Vec<(u32, Vec<u8>)> = vec![
        (
            KIND_VIDEO_TRACK_META,
            encode_video_meta(video_meta, video_sample_entry, video_elst)?,
        ),
        (
            KIND_AUDIO_TRACK_META,
            encode_audio_meta(audio_meta, audio_sample_entry, audio_elst)?,
        ),
        (
            KIND_VIDEO_SAMPLE_TABLE,
            encode_sample_table(video_samples, "VideoSampleTable")?,
        ),
        (
            KIND_AUDIO_SAMPLE_TABLE,
            encode_sample_table(audio_samples, "AudioSampleTable")?,
        ),
        (KIND_SEGMENT_TABLE, encode_segment_table(segments)?),
        (KIND_PLAYLIST_BYTES, playlist_bytes.to_vec()),
        (KIND_INIT_SEGMENT_BYTES, init_bytes.to_vec()),
    ];

    let n_sections = sections.len();
    let header_end = HEADER_FIXED_LEN + n_sections * SECTION_DIR_ENTRY_LEN;
    let mut bytes: Vec<u8> = vec![0u8; header_end];
    debug_assert!(bytes.len().is_multiple_of(8));

    let mut directory: Vec<(u32, u64)> = Vec::with_capacity(n_sections);
    for (i, (kind, content)) in sections.iter().enumerate() {
        let off = bytes.len() as u64;
        debug_assert!(off.is_multiple_of(8), "section start must be 8-aligned");
        bytes.extend_from_slice(content);
        directory.push((*kind, off));
        if i + 1 < n_sections {
            let rem = bytes.len() & 7;
            if rem != 0 {
                bytes.resize(bytes.len() + (8 - rem), 0);
            }
        }
    }

    bytes[0..4].copy_from_slice(&MAGIC);
    BigEndian::write_u32(&mut bytes[4..8], max_segment_size);
    BigEndian::write_u64(&mut bytes[8..16], source_len);
    bytes[16..48].copy_from_slice(blake3);
    // `n_sections` is bounded by `MAX_SECTIONS = 64` and so always fits in
    // u32; surface a malformed-directory error rather than `expect` to keep
    // production code panic-free.
    let n_sections_u32 = u32::try_from(n_sections).map_err(|_| {
        PackagerError::MalformedIndexDirectory("internal: section_count does not fit in u32")
    })?;
    BigEndian::write_u32(&mut bytes[48..52], n_sections_u32);
    BigEndian::write_u32(&mut bytes[52..56], 0);

    for (i, (kind, offset)) in directory.iter().enumerate() {
        let pos = HEADER_FIXED_LEN + i * SECTION_DIR_ENTRY_LEN;
        BigEndian::write_u32(&mut bytes[pos..pos + 4], *kind);
        BigEndian::write_u32(&mut bytes[pos + 4..pos + 8], 0);
        BigEndian::write_u64(&mut bytes[pos + 8..pos + 16], *offset);
    }

    Ok(bytes)
}

fn encode_video_meta(
    meta: &ParsedTrack,
    sample_entry: &[u8],
    elst: &[u8],
) -> Result<Vec<u8>, PackagerError> {
    let mut s: Vec<u8> = Vec::with_capacity(20 + sample_entry.len() + 4 + elst.len());
    s.write_u32::<BigEndian>(meta.timescale)?;
    s.write_all(&meta.fourcc)?;
    s.write_u32::<BigEndian>(meta.width)?;
    s.write_u32::<BigEndian>(meta.height)?;
    s.write_u32::<BigEndian>(byte_blob_size(sample_entry, "video sample_entry")?)?;
    s.write_all(sample_entry)?;
    s.write_u32::<BigEndian>(byte_blob_size(elst, "video elst")?)?;
    s.write_all(elst)?;
    Ok(s)
}

fn encode_audio_meta(
    meta: &ParsedTrack,
    sample_entry: &[u8],
    elst: &[u8],
) -> Result<Vec<u8>, PackagerError> {
    let mut s: Vec<u8> = Vec::with_capacity(20 + sample_entry.len() + 4 + elst.len());
    s.write_u32::<BigEndian>(meta.timescale)?;
    s.write_all(&meta.fourcc)?;
    s.write_u32::<BigEndian>(meta.sample_rate)?;
    s.write_u8(meta.channel_count)?;
    s.write_all(&[0u8; 3])?;
    s.write_u32::<BigEndian>(byte_blob_size(sample_entry, "audio sample_entry")?)?;
    s.write_all(sample_entry)?;
    s.write_u32::<BigEndian>(byte_blob_size(elst, "audio elst")?)?;
    s.write_all(elst)?;
    Ok(s)
}

fn encode_sample_table(
    samples: &[SampleEntry],
    name: &'static str,
) -> Result<Vec<u8>, PackagerError> {
    let n = u32::try_from(samples.len()).map_err(|_| match name {
        "VideoSampleTable" => {
            PackagerError::SampleTableInconsistent("video sample count exceeds u32::MAX")
        }
        _ => PackagerError::SampleTableInconsistent("audio sample count exceeds u32::MAX"),
    })?;
    let mut s: Vec<u8> = Vec::with_capacity(8 + std::mem::size_of_val(samples));
    s.write_u32::<BigEndian>(n)?;
    s.write_u32::<BigEndian>(0)?;
    for x in samples {
        s.extend_from_slice(&x.offset.to_ne_bytes());
        s.extend_from_slice(&x.size.to_ne_bytes());
        s.extend_from_slice(&x.dts_delta.to_ne_bytes());
        s.extend_from_slice(&x.cts_offset.to_ne_bytes());
        s.extend_from_slice(&x.flags.to_ne_bytes());
    }
    debug_assert_eq!(s.len(), 8 + std::mem::size_of_val(samples));
    Ok(s)
}

fn encode_segment_table(segments: &[SegmentEntry]) -> Result<Vec<u8>, PackagerError> {
    let n = u32::try_from(segments.len())
        .map_err(|_| PackagerError::SampleTableInconsistent("segment count exceeds u32::MAX"))?;
    let mut s: Vec<u8> = Vec::with_capacity(8 + std::mem::size_of_val(segments));
    s.write_u32::<BigEndian>(n)?;
    s.write_u32::<BigEndian>(0)?;
    for x in segments {
        s.extend_from_slice(&x.video_sample_start.to_ne_bytes());
        s.extend_from_slice(&x.video_sample_count.to_ne_bytes());
        s.extend_from_slice(&x.audio_sample_start.to_ne_bytes());
        s.extend_from_slice(&x.audio_sample_count.to_ne_bytes());
        s.extend_from_slice(&x.video_base_dts.to_ne_bytes());
        s.extend_from_slice(&x.audio_base_dts.to_ne_bytes());
    }
    debug_assert_eq!(s.len(), 8 + std::mem::size_of_val(segments));
    Ok(s)
}

fn byte_blob_size(blob: &[u8], name: &'static str) -> Result<u32, PackagerError> {
    u32::try_from(blob.len()).map_err(|_| {
        PackagerError::Io(io::Error::other(format!(
            "{name} byte length {} exceeds u32::MAX",
            blob.len(),
        )))
    })
}

fn raw_to_sample_entry(r: &RawSampleEntry) -> SampleEntry {
    SampleEntry {
        offset: r.offset,
        size: r.size,
        dts_delta: r.dts_delta,
        cts_offset: r.cts_offset,
        flags: if r.is_sync { SAMPLE_FLAG_IS_SYNC } else { 0 },
    }
}

fn read_byte_range<R: ReadAt + ?Sized>(
    reader: &R,
    range: ByteRange,
) -> Result<Vec<u8>, PackagerError> {
    let len = usize::try_from(range.len).map_err(|_| {
        PackagerError::Io(io::Error::other(format!(
            "byte range length {} does not fit usize",
            range.len,
        )))
    })?;
    let mut buf = vec![0u8; len];
    read_exact(reader, range.offset, &mut buf)?;
    Ok(buf)
}

fn blake3_of_source<R: ReadAt + ?Sized>(reader: &R, len: u64) -> Result<[u8; 32], PackagerError> {
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut pos: u64 = 0;
    while pos < len {
        let want = (len - pos).min(buf.len() as u64) as usize;
        let n = reader.read_at(pos, &mut buf[..want])?;
        if n == 0 {
            return Err(PackagerError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "ReadAt returned EOF mid-source while hashing at offset {pos} (still need {} bytes)",
                    len - pos,
                ),
            )));
        }
        hasher.update(&buf[..n]);
        pos += n as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Counting `Write + Seek` sink. Tracks high-water mark only — no
/// allocation per-byte. Writes extend `len`; seeks only move the cursor.
/// This matches `Cursor<Vec<u8>>` length semantics, so the fMP4 writer's
/// seek-patch of `trun.data_offset` (which seeks back inside the already-
/// written byte range, then back to the end) does not artificially grow
/// the recorded segment size.
#[derive(Default)]
struct CountingSink {
    pos: u64,
    len: u64,
}

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pos = self
            .pos
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::other("CountingSink write overflow"))?;
        if self.pos > self.len {
            self.len = self.pos;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for CountingSink {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => offset_from(self.len, d)?,
            SeekFrom::Current(d) => offset_from(self.pos, d)?,
        };
        self.pos = new_pos;
        Ok(self.pos)
    }
}

fn offset_from(base: u64, delta: i64) -> io::Result<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
            .ok_or_else(|| io::Error::other("CountingSink seek overflow"))
    } else {
        base.checked_sub((-delta) as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before zero"))
    }
}
