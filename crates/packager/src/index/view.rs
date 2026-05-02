//! Borrowed view over a validated `.idx` byte buffer.
//!
//! [`IndexView::open`] is the single byte-level constructor: it validates
//! the header magic, the section directory (offsets, alignment,
//! uniqueness, `section_count` bound), and the section payload framings
//! before handing out borrowed accessors. Once `open` succeeds, callers
//! walk the view via ordinary `&` slices — including the two
//! `[SampleEntry]` / `[SegmentEntry]` slices materialised through the
//! single sanctioned `unsafe` block in this library.
//!
//! [`IndexView::from_parts`] is the in-tree constructor used by
//! [`super::builder::IndexBuilder`] when it pre-renders `init.mp4` /
//! sizes media segments before the byte image is finalised. It bypasses
//! the byte-level validation `open()` performs because every borrowed
//! slice it receives originates inside the builder itself.

use std::mem::{align_of, size_of};

use byteorder::{BigEndian, ByteOrder};

use crate::error::PackagerError;

use super::format::{
    HEADER_FIXED_LEN, KIND_AUDIO_SAMPLE_TABLE, KIND_AUDIO_TRACK_META, KIND_INIT_SEGMENT_BYTES,
    KIND_PLAYLIST_BYTES, KIND_SEGMENT_TABLE, KIND_VIDEO_SAMPLE_TABLE, KIND_VIDEO_TRACK_META, MAGIC,
    MAX_SECTIONS, SECTION_DIR_ENTRY_LEN, SampleEntry, SegmentEntry,
};

/// Per-track video metadata. All `&'a [u8]` fields borrow into the
/// underlying `.idx` byte buffer (or, in the builder, into caller-owned
/// slices) and never copy.
#[derive(Debug, Clone, Copy)]
pub struct VideoTrackMeta<'a> {
    pub timescale: u32,
    pub fourcc: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// Verbatim bytes of the video sample-entry box (`avc1` / `hvc1` /
    /// `hev1`), including the codec-config child and any siblings.
    pub sample_entry: &'a [u8],
    /// Verbatim bytes of the entire `edts` box (containing `elst`); empty
    /// slice if the input had no edit list.
    pub elst: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct AudioTrackMeta<'a> {
    pub timescale: u32,
    pub fourcc: [u8; 4],
    pub sample_rate: u32,
    pub channel_count: u8,
    pub sample_entry: &'a [u8],
    pub elst: &'a [u8],
}

/// Validated, zero-copy view over a `.idx` byte buffer.
#[derive(Debug, Clone, Copy)]
pub struct IndexView<'a> {
    max_segment_size: u32,
    source_mp4_len: u64,
    source_mp4_blake3: &'a [u8; 32],
    video_track: VideoTrackMeta<'a>,
    audio_track: AudioTrackMeta<'a>,
    video_samples: &'a [SampleEntry],
    audio_samples: &'a [SampleEntry],
    segments: &'a [SegmentEntry],
    init_segment_bytes: &'a [u8],
    playlist_bytes: Option<&'a [u8]>,
}

impl<'a> IndexView<'a> {
    /// Validate `bytes` as a `.idx` byte image and return a borrowed view.
    ///
    /// Error mapping:
    /// - [`PackagerError::IndexMagicMismatch`] when the leading 4 bytes are
    ///   not `b"HCMI"`.
    /// - [`PackagerError::MalformedIndexDirectory`] for header / directory
    ///   issues (truncation, `section_count > 64`, non-ascending offsets,
    ///   misaligned section starts, duplicate kinds).
    /// - [`PackagerError::MalformedIndexSection`] when a payload's framing
    ///   is internally inconsistent (e.g., sample-table body length not a
    ///   multiple of 24, track-meta truncated mid-field).
    pub fn open(bytes: &'a [u8]) -> Result<Self, PackagerError> {
        if bytes.len() < HEADER_FIXED_LEN {
            return Err(PackagerError::MalformedIndexDirectory(
                "file shorter than fixed header",
            ));
        }
        if bytes[..4] != MAGIC {
            return Err(PackagerError::IndexMagicMismatch);
        }

        let max_segment_size = BigEndian::read_u32(&bytes[4..8]);
        let source_mp4_len = BigEndian::read_u64(&bytes[8..16]);
        // The slice has a known length of 32 by construction, so this
        // conversion cannot fail at runtime; surface a malformed-directory
        // error rather than `expect` to keep production code panic-free.
        let blake3_slice: &[u8; 32] = bytes[16..48].try_into().map_err(|_| {
            PackagerError::MalformedIndexDirectory("internal: blake3 slice length mismatch")
        })?;
        let section_count = BigEndian::read_u32(&bytes[48..52]) as usize;
        let reserved = BigEndian::read_u32(&bytes[52..56]);

        if reserved != 0 {
            return Err(PackagerError::MalformedIndexDirectory(
                "reserved word at offset 52 is nonzero",
            ));
        }
        if section_count > MAX_SECTIONS {
            return Err(PackagerError::MalformedIndexDirectory(
                "section_count exceeds MAX_SECTIONS",
            ));
        }
        if section_count == 0 {
            return Err(PackagerError::MalformedIndexDirectory(
                "section_count is zero",
            ));
        }

        let header_end = HEADER_FIXED_LEN + section_count * SECTION_DIR_ENTRY_LEN;
        if bytes.len() < header_end {
            return Err(PackagerError::MalformedIndexDirectory(
                "directory truncated",
            ));
        }

        let mut directory: [(u32, u64); MAX_SECTIONS] = [(0u32, 0u64); MAX_SECTIONS];
        for (i, slot) in directory.iter_mut().take(section_count).enumerate() {
            let pos = HEADER_FIXED_LEN + i * SECTION_DIR_ENTRY_LEN;
            let kind = BigEndian::read_u32(&bytes[pos..pos + 4]);
            let pad = BigEndian::read_u32(&bytes[pos + 4..pos + 8]);
            if pad != 0 {
                return Err(PackagerError::MalformedIndexDirectory(
                    "section directory _pad word is nonzero",
                ));
            }
            let offset = BigEndian::read_u64(&bytes[pos + 8..pos + 16]);
            *slot = (kind, offset);
        }
        let directory = &directory[..section_count];

        let header_end_u64 = header_end as u64;
        if directory[0].1 != header_end_u64 {
            return Err(PackagerError::MalformedIndexDirectory(
                "first section offset does not equal header end",
            ));
        }
        for w in directory.windows(2) {
            if w[1].1 <= w[0].1 {
                return Err(PackagerError::MalformedIndexDirectory(
                    "section offsets must be strictly ascending",
                ));
            }
        }
        let bytes_len = bytes.len() as u64;
        if directory[section_count - 1].1 > bytes_len {
            return Err(PackagerError::MalformedIndexDirectory(
                "last section offset exceeds file length",
            ));
        }
        for &(_, offset) in directory {
            if !offset.is_multiple_of(8) {
                return Err(PackagerError::MalformedIndexDirectory(
                    "section offset is not 8-byte aligned",
                ));
            }
        }
        for i in 0..section_count {
            for j in (i + 1)..section_count {
                if directory[i].0 == directory[j].0 {
                    return Err(PackagerError::MalformedIndexDirectory(
                        "duplicate section kind",
                    ));
                }
            }
        }

        let section_payload = |kind: u32| -> Option<&'a [u8]> {
            let i = directory.iter().position(|&(k, _)| k == kind)?;
            let start = directory[i].1 as usize;
            let end = directory
                .get(i + 1)
                .map(|&(_, off)| off as usize)
                .unwrap_or(bytes.len());
            Some(&bytes[start..end])
        };

        let video_meta_bytes = section_payload(KIND_VIDEO_TRACK_META).ok_or(
            PackagerError::MalformedIndexSection("missing VideoTrackMeta section"),
        )?;
        let audio_meta_bytes = section_payload(KIND_AUDIO_TRACK_META).ok_or(
            PackagerError::MalformedIndexSection("missing AudioTrackMeta section"),
        )?;
        let video_samples_bytes = section_payload(KIND_VIDEO_SAMPLE_TABLE).ok_or(
            PackagerError::MalformedIndexSection("missing VideoSampleTable section"),
        )?;
        let audio_samples_bytes = section_payload(KIND_AUDIO_SAMPLE_TABLE).ok_or(
            PackagerError::MalformedIndexSection("missing AudioSampleTable section"),
        )?;
        let segments_bytes = section_payload(KIND_SEGMENT_TABLE).ok_or(
            PackagerError::MalformedIndexSection("missing SegmentTable section"),
        )?;
        let init_segment_bytes = section_payload(KIND_INIT_SEGMENT_BYTES).ok_or(
            PackagerError::MalformedIndexSection("missing InitSegmentBytes section"),
        )?;
        let playlist_bytes = section_payload(KIND_PLAYLIST_BYTES);

        let video_track = parse_video_meta(video_meta_bytes)?;
        let audio_track = parse_audio_meta(audio_meta_bytes)?;
        let video_samples = parse_sample_table(video_samples_bytes, "VideoSampleTable")?;
        let audio_samples = parse_sample_table(audio_samples_bytes, "AudioSampleTable")?;
        let segments = parse_segment_table(segments_bytes)?;

        Ok(Self {
            max_segment_size,
            source_mp4_len,
            source_mp4_blake3: blake3_slice,
            video_track,
            audio_track,
            video_samples,
            audio_samples,
            segments,
            init_segment_bytes,
            playlist_bytes,
        })
    }

    pub fn max_segment_size(&self) -> u32 {
        self.max_segment_size
    }

    pub fn source_mp4_len(&self) -> u64 {
        self.source_mp4_len
    }

    pub fn source_mp4_blake3(&self) -> &'a [u8; 32] {
        self.source_mp4_blake3
    }

    pub fn video_track(&self) -> VideoTrackMeta<'a> {
        self.video_track
    }

    pub fn audio_track(&self) -> AudioTrackMeta<'a> {
        self.audio_track
    }

    pub fn video_samples(&self) -> &'a [SampleEntry] {
        self.video_samples
    }

    pub fn audio_samples(&self) -> &'a [SampleEntry] {
        self.audio_samples
    }

    pub fn segments(&self) -> &'a [SegmentEntry] {
        self.segments
    }

    pub fn segment_count(&self) -> u32 {
        self.segments.len() as u32
    }

    pub fn init_segment_bytes(&self) -> &'a [u8] {
        self.init_segment_bytes
    }

    pub fn playlist_bytes(&self) -> Option<&'a [u8]> {
        self.playlist_bytes
    }
}

impl<'a> IndexView<'a> {
    /// Direct constructor used by the in-tree builder and by inline
    /// writer tests. Bypasses the byte-level validation `open()` performs;
    /// every caller is library-internal and provides slices it owns.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        max_segment_size: u32,
        source_mp4_len: u64,
        source_mp4_blake3: &'a [u8; 32],
        video_track: VideoTrackMeta<'a>,
        audio_track: AudioTrackMeta<'a>,
        video_samples: &'a [SampleEntry],
        audio_samples: &'a [SampleEntry],
        segments: &'a [SegmentEntry],
        init_segment_bytes: &'a [u8],
        playlist_bytes: Option<&'a [u8]>,
    ) -> Self {
        Self {
            max_segment_size,
            source_mp4_len,
            source_mp4_blake3,
            video_track,
            audio_track,
            video_samples,
            audio_samples,
            segments,
            init_segment_bytes,
            playlist_bytes,
        }
    }
}

fn parse_video_meta(payload: &[u8]) -> Result<VideoTrackMeta<'_>, PackagerError> {
    if payload.len() < 20 {
        return Err(PackagerError::MalformedIndexSection(
            "VideoTrackMeta truncated before sample_entry_size",
        ));
    }
    let timescale = BigEndian::read_u32(&payload[0..4]);
    let mut fourcc = [0u8; 4];
    fourcc.copy_from_slice(&payload[4..8]);
    let width = BigEndian::read_u32(&payload[8..12]);
    let height = BigEndian::read_u32(&payload[12..16]);
    let sample_entry_size = BigEndian::read_u32(&payload[16..20]) as usize;
    let sample_entry_start: usize = 20;
    let sample_entry_end = sample_entry_start.checked_add(sample_entry_size).ok_or(
        PackagerError::MalformedIndexSection("VideoTrackMeta sample_entry_size overflows usize"),
    )?;
    if payload.len() < sample_entry_end + 4 {
        return Err(PackagerError::MalformedIndexSection(
            "VideoTrackMeta truncated before elst_size",
        ));
    }
    let sample_entry = &payload[sample_entry_start..sample_entry_end];
    let elst_size = BigEndian::read_u32(&payload[sample_entry_end..sample_entry_end + 4]) as usize;
    let elst_start = sample_entry_end + 4;
    let elst_end =
        elst_start
            .checked_add(elst_size)
            .ok_or(PackagerError::MalformedIndexSection(
                "VideoTrackMeta elst_size overflows usize",
            ))?;
    if payload.len() < elst_end {
        return Err(PackagerError::MalformedIndexSection(
            "VideoTrackMeta truncated before elst end",
        ));
    }
    validate_trailing_padding("VideoTrackMeta", &payload[elst_end..])?;
    let elst = &payload[elst_start..elst_end];
    Ok(VideoTrackMeta {
        timescale,
        fourcc,
        width,
        height,
        sample_entry,
        elst,
    })
}

fn parse_audio_meta(payload: &[u8]) -> Result<AudioTrackMeta<'_>, PackagerError> {
    // Layout: timescale(4) + fourcc(4) + sample_rate(4) + channel_count(1)
    //         + pad(3) + sample_entry_size(4) + sample_entry_bytes
    //         + elst_size(4) + elst_bytes.
    if payload.len() < 20 {
        return Err(PackagerError::MalformedIndexSection(
            "AudioTrackMeta truncated before sample_entry_size",
        ));
    }
    let timescale = BigEndian::read_u32(&payload[0..4]);
    let mut fourcc = [0u8; 4];
    fourcc.copy_from_slice(&payload[4..8]);
    let sample_rate = BigEndian::read_u32(&payload[8..12]);
    let channel_count = payload[12];
    if payload[13..16] != [0, 0, 0] {
        return Err(PackagerError::MalformedIndexSection(
            "AudioTrackMeta padding bytes are nonzero",
        ));
    }
    let sample_entry_size = BigEndian::read_u32(&payload[16..20]) as usize;
    let sample_entry_start: usize = 20;
    let sample_entry_end = sample_entry_start.checked_add(sample_entry_size).ok_or(
        PackagerError::MalformedIndexSection("AudioTrackMeta sample_entry_size overflows usize"),
    )?;
    if payload.len() < sample_entry_end + 4 {
        return Err(PackagerError::MalformedIndexSection(
            "AudioTrackMeta truncated before elst_size",
        ));
    }
    let sample_entry = &payload[sample_entry_start..sample_entry_end];
    let elst_size = BigEndian::read_u32(&payload[sample_entry_end..sample_entry_end + 4]) as usize;
    let elst_start = sample_entry_end + 4;
    let elst_end =
        elst_start
            .checked_add(elst_size)
            .ok_or(PackagerError::MalformedIndexSection(
                "AudioTrackMeta elst_size overflows usize",
            ))?;
    if payload.len() < elst_end {
        return Err(PackagerError::MalformedIndexSection(
            "AudioTrackMeta truncated before elst end",
        ));
    }
    validate_trailing_padding("AudioTrackMeta", &payload[elst_end..])?;
    let elst = &payload[elst_start..elst_end];
    Ok(AudioTrackMeta {
        timescale,
        fourcc,
        sample_rate,
        channel_count,
        sample_entry,
        elst,
    })
}

/// Reject track-meta sections whose declared body is followed by more
/// than the canonical zero-pad. The builder emits at most 7 zero bytes
/// of inter-section pad to align the next section to 8 B; anything
/// longer or anything non-zero indicates a corrupted or attacker-crafted
/// `.idx`.
fn validate_trailing_padding(section: &'static str, padding: &[u8]) -> Result<(), PackagerError> {
    if padding.len() >= 8 {
        return Err(PackagerError::MalformedIndexSection(match section {
            "VideoTrackMeta" => "VideoTrackMeta has non-canonical trailing padding",
            _ => "AudioTrackMeta has non-canonical trailing padding",
        }));
    }
    if padding.iter().any(|&b| b != 0) {
        return Err(PackagerError::MalformedIndexSection(match section {
            "VideoTrackMeta" => "VideoTrackMeta trailing padding is nonzero",
            _ => "AudioTrackMeta trailing padding is nonzero",
        }));
    }
    Ok(())
}

fn parse_sample_table<'a>(
    payload: &'a [u8],
    name: &'static str,
) -> Result<&'a [SampleEntry], PackagerError> {
    if payload.len() < 8 {
        return Err(PackagerError::MalformedIndexSection(match name {
            "VideoSampleTable" => "VideoSampleTable shorter than 8-byte prefix",
            _ => "AudioSampleTable shorter than 8-byte prefix",
        }));
    }
    let n = BigEndian::read_u32(&payload[0..4]) as usize;
    if BigEndian::read_u32(&payload[4..8]) != 0 {
        return Err(PackagerError::MalformedIndexSection(match name {
            "VideoSampleTable" => "VideoSampleTable padding word is nonzero",
            _ => "AudioSampleTable padding word is nonzero",
        }));
    }
    let body = &payload[8..];
    let expected_len =
        n.checked_mul(size_of::<SampleEntry>())
            .ok_or(PackagerError::MalformedIndexSection(match name {
                "VideoSampleTable" => "VideoSampleTable n_samples overflows usize",
                _ => "AudioSampleTable n_samples overflows usize",
            }))?;
    if body.len() != expected_len {
        return Err(PackagerError::MalformedIndexSection(match name {
            "VideoSampleTable" => "VideoSampleTable body length disagrees with n_samples",
            _ => "AudioSampleTable body length disagrees with n_samples",
        }));
    }
    let ptr = body.as_ptr();
    if !(ptr as usize).is_multiple_of(align_of::<SampleEntry>()) {
        return Err(PackagerError::MalformedIndexSection(match name {
            "VideoSampleTable" => "VideoSampleTable rows misaligned for SampleEntry",
            _ => "AudioSampleTable rows misaligned for SampleEntry",
        }));
    }
    // SAFETY:
    // - section payloads are placed at 8-byte-aligned file offsets — checked
    //   above against `directory[i].1 % 8`.
    // - `body` is the per-section bytes following the 8-byte n_samples + pad
    //   prefix, so its absolute address is also 8-aligned. This is verified
    //   by the `align_of::<SampleEntry>()` check above; the sole sanctioned
    //   `unsafe` site in the library does not rely on the check being
    //   tautological.
    // - `body.len() == n * size_of::<SampleEntry>()`, validated immediately
    //   above; a length mismatch returns `MalformedIndexSection`.
    // - `SampleEntry` is `#[repr(C)]` with deliberate field ordering that
    //   produces zero compiler padding; the on-disk byte sequence is the
    //   native-endian image of the in-memory layout. Fields are POD
    //   (u64/u32/i32) — no `Drop`, no references, no interior mutability.
    // - The returned slice's lifetime is tied to `&payload`, which is itself
    //   a borrow of the caller's `&'a [u8]`. The slice cannot outlive the
    //   underlying buffer.
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<SampleEntry>(), n) };
    Ok(slice)
}

fn parse_segment_table(payload: &[u8]) -> Result<&[SegmentEntry], PackagerError> {
    if payload.len() < 8 {
        return Err(PackagerError::MalformedIndexSection(
            "SegmentTable shorter than 8-byte prefix",
        ));
    }
    let n = BigEndian::read_u32(&payload[0..4]) as usize;
    if BigEndian::read_u32(&payload[4..8]) != 0 {
        return Err(PackagerError::MalformedIndexSection(
            "SegmentTable padding word is nonzero",
        ));
    }
    let body = &payload[8..];
    let expected_len =
        n.checked_mul(size_of::<SegmentEntry>())
            .ok_or(PackagerError::MalformedIndexSection(
                "SegmentTable n_segments overflows usize",
            ))?;
    if body.len() != expected_len {
        return Err(PackagerError::MalformedIndexSection(
            "SegmentTable body length disagrees with n_segments",
        ));
    }
    let ptr = body.as_ptr();
    if !(ptr as usize).is_multiple_of(align_of::<SegmentEntry>()) {
        return Err(PackagerError::MalformedIndexSection(
            "SegmentTable rows misaligned for SegmentEntry",
        ));
    }
    // SAFETY: same invariants as `parse_sample_table` above; this site is
    // gated by the same alignment + length checks. `SegmentEntry` is
    // `#[repr(C)]` with zero compiler padding (u32 × 4 then u64 × 2);
    // `size_of::<SegmentEntry>() == 32`, `align_of::<SegmentEntry>() == 8`.
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<SegmentEntry>(), n) };
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::format::{
        KIND_AUDIO_SAMPLE_TABLE, KIND_AUDIO_TRACK_META, KIND_INIT_SEGMENT_BYTES,
        KIND_SEGMENT_TABLE, KIND_VIDEO_SAMPLE_TABLE, KIND_VIDEO_TRACK_META,
    };

    /// Build a minimal valid `.idx` byte image for `IndexView::open` tests.
    /// Layout matches what `IndexBuilder::build` will emit; kept inline here
    /// so view-level tests do not depend on the builder landing first.
    ///
    /// 8-byte alignment of the buffer itself: `Vec<u64>` is pointer-aligned
    /// to `align_of::<u64>() == 8` on every supported target. The `as_ptr()
    /// .cast()` over a `Vec<u64>` gives a buffer whose start address is
    /// 8-byte-aligned, which the unsafe slice cast in `IndexView` requires
    /// for `parse_sample_table` / `parse_segment_table` to succeed.
    /// `Vec<u8>` has only `align_of::<u8>() == 1`, so a freshly-allocated
    /// `Vec<u8>` would not give us the stricter alignment we need.
    fn build_minimal_idx() -> Vec<u8> {
        let video_sample_entry = vec![0xAAu8, 0xBB, 0xCC];
        let audio_sample_entry = vec![0x11u8, 0x22];
        let init_bytes = vec![1u8, 2, 3, 4, 5];
        let blake3 = [0xEEu8; 32];

        let video_samples: Vec<SampleEntry> = vec![SampleEntry {
            offset: 100,
            size: 50,
            dts_delta: 3000,
            cts_offset: 0,
            flags: 1,
        }];
        let audio_samples: Vec<SampleEntry> = vec![SampleEntry {
            offset: 200,
            size: 30,
            dts_delta: 1024,
            cts_offset: 0,
            flags: 1,
        }];
        let segments: Vec<SegmentEntry> = vec![SegmentEntry {
            video_sample_start: 0,
            video_sample_count: 1,
            audio_sample_start: 0,
            audio_sample_count: 1,
            video_base_dts: 0,
            audio_base_dts: 0,
        }];

        // Build per-section payloads, then lay them out with inter-section
        // 8-byte padding only (no trailing padding after the last section,
        // because the final section's length is `file_len − directory[last]`,
        // and padding there would corrupt e.g. exact-bytes round-trip on
        // `init_segment_bytes()`).
        let mut sections: Vec<(u32, Vec<u8>)> = Vec::new();

        let mut s: Vec<u8> = Vec::new();
        s.extend_from_slice(&90_000u32.to_be_bytes());
        s.extend_from_slice(b"avc1");
        s.extend_from_slice(&1920u32.to_be_bytes());
        s.extend_from_slice(&1080u32.to_be_bytes());
        s.extend_from_slice(&(video_sample_entry.len() as u32).to_be_bytes());
        s.extend_from_slice(&video_sample_entry);
        s.extend_from_slice(&0u32.to_be_bytes());
        sections.push((KIND_VIDEO_TRACK_META, s));

        let mut s: Vec<u8> = Vec::new();
        s.extend_from_slice(&48_000u32.to_be_bytes());
        s.extend_from_slice(b"mp4a");
        s.extend_from_slice(&48_000u32.to_be_bytes());
        s.push(2u8);
        s.extend_from_slice(&[0u8; 3]);
        s.extend_from_slice(&(audio_sample_entry.len() as u32).to_be_bytes());
        s.extend_from_slice(&audio_sample_entry);
        s.extend_from_slice(&0u32.to_be_bytes());
        sections.push((KIND_AUDIO_TRACK_META, s));

        let mut s: Vec<u8> = Vec::new();
        s.extend_from_slice(&(video_samples.len() as u32).to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        for x in &video_samples {
            s.extend_from_slice(&x.offset.to_ne_bytes());
            s.extend_from_slice(&x.size.to_ne_bytes());
            s.extend_from_slice(&x.dts_delta.to_ne_bytes());
            s.extend_from_slice(&x.cts_offset.to_ne_bytes());
            s.extend_from_slice(&x.flags.to_ne_bytes());
        }
        sections.push((KIND_VIDEO_SAMPLE_TABLE, s));

        let mut s: Vec<u8> = Vec::new();
        s.extend_from_slice(&(audio_samples.len() as u32).to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        for x in &audio_samples {
            s.extend_from_slice(&x.offset.to_ne_bytes());
            s.extend_from_slice(&x.size.to_ne_bytes());
            s.extend_from_slice(&x.dts_delta.to_ne_bytes());
            s.extend_from_slice(&x.cts_offset.to_ne_bytes());
            s.extend_from_slice(&x.flags.to_ne_bytes());
        }
        sections.push((KIND_AUDIO_SAMPLE_TABLE, s));

        let mut s: Vec<u8> = Vec::new();
        s.extend_from_slice(&(segments.len() as u32).to_be_bytes());
        s.extend_from_slice(&0u32.to_be_bytes());
        for x in &segments {
            s.extend_from_slice(&x.video_sample_start.to_ne_bytes());
            s.extend_from_slice(&x.video_sample_count.to_ne_bytes());
            s.extend_from_slice(&x.audio_sample_start.to_ne_bytes());
            s.extend_from_slice(&x.audio_sample_count.to_ne_bytes());
            s.extend_from_slice(&x.video_base_dts.to_ne_bytes());
            s.extend_from_slice(&x.audio_base_dts.to_ne_bytes());
        }
        sections.push((KIND_SEGMENT_TABLE, s));

        sections.push((KIND_INIT_SEGMENT_BYTES, init_bytes.clone()));

        let n_sections = sections.len();
        let header_end = HEADER_FIXED_LEN + n_sections * SECTION_DIR_ENTRY_LEN;
        let mut bytes: Vec<u8> = vec![0u8; header_end];
        debug_assert_eq!(bytes.len() % 8, 0);

        let mut directory: Vec<(u32, u64)> = Vec::with_capacity(n_sections);
        for (i, (kind, content)) in sections.iter().enumerate() {
            let off = bytes.len() as u64;
            debug_assert_eq!(off % 8, 0);
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
        bytes[4..8].copy_from_slice(&77u32.to_be_bytes());
        bytes[8..16].copy_from_slice(&12_345u64.to_be_bytes());
        bytes[16..48].copy_from_slice(&blake3);
        bytes[48..52].copy_from_slice(&(n_sections as u32).to_be_bytes());
        bytes[52..56].copy_from_slice(&0u32.to_be_bytes());

        for (i, (kind, offset)) in directory.iter().enumerate() {
            let pos = HEADER_FIXED_LEN + i * SECTION_DIR_ENTRY_LEN;
            bytes[pos..pos + 4].copy_from_slice(&kind.to_be_bytes());
            bytes[pos + 4..pos + 8].copy_from_slice(&0u32.to_be_bytes());
            bytes[pos + 8..pos + 16].copy_from_slice(&offset.to_be_bytes());
        }

        bytes
    }

    /// Wrapper that returns an owned `Vec<u8>` whose backing pointer is
    /// 8-byte-aligned. The `Vec<u8>::to_vec()` round-trip in
    /// `build_minimal_idx` does not preserve alignment; the helper below
    /// goes through `Vec<u64>` and exposes its bytes directly.
    fn build_aligned_idx() -> AlignedBuffer {
        let raw = build_minimal_idx();
        AlignedBuffer::from_bytes(&raw)
    }

    struct AlignedBuffer {
        backing: Vec<u64>,
        len: usize,
    }

    impl AlignedBuffer {
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

        fn as_slice(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.backing.as_ptr().cast::<u8>(), self.len) }
        }

        fn as_mut_slice(&mut self) -> &mut [u8] {
            unsafe {
                std::slice::from_raw_parts_mut(self.backing.as_mut_ptr().cast::<u8>(), self.len)
            }
        }
    }

    #[test]
    fn open_minimal_idx_round_trips_accessors() {
        let buf = build_aligned_idx();
        let view = IndexView::open(buf.as_slice()).expect("open");
        assert_eq!(view.max_segment_size(), 77);
        assert_eq!(view.source_mp4_len(), 12_345);
        assert_eq!(view.source_mp4_blake3(), &[0xEEu8; 32]);
        assert_eq!(view.video_track().timescale, 90_000);
        assert_eq!(view.video_track().fourcc, *b"avc1");
        assert_eq!(view.video_track().width, 1920);
        assert_eq!(view.video_track().height, 1080);
        assert_eq!(view.video_track().sample_entry, &[0xAAu8, 0xBB, 0xCC]);
        assert!(view.video_track().elst.is_empty());
        assert_eq!(view.audio_track().timescale, 48_000);
        assert_eq!(view.audio_track().fourcc, *b"mp4a");
        assert_eq!(view.audio_track().sample_rate, 48_000);
        assert_eq!(view.audio_track().channel_count, 2);
        assert_eq!(view.audio_track().sample_entry, &[0x11u8, 0x22]);
        assert!(view.audio_track().elst.is_empty());
        assert_eq!(view.video_samples().len(), 1);
        assert_eq!(view.video_samples()[0].offset, 100);
        assert_eq!(view.video_samples()[0].size, 50);
        assert_eq!(view.audio_samples().len(), 1);
        assert_eq!(view.audio_samples()[0].size, 30);
        assert_eq!(view.segments().len(), 1);
        assert_eq!(view.segments()[0].video_sample_count, 1);
        assert_eq!(view.init_segment_bytes(), &[1u8, 2, 3, 4, 5]);
        assert!(view.playlist_bytes().is_none());
    }

    #[test]
    fn nonzero_track_meta_trailing_padding_returns_malformed_section() {
        // Layout of the synthetic VideoTrackMeta payload from
        // `build_minimal_idx`:
        //   timescale(4) + fourcc(4) + width(4) + height(4) +
        //   sample_entry_size(4) + sample_entry_bytes(3) +
        //   elst_size(4) = 27 bytes.
        // The first byte after that is inter-section zero pad. Flip it
        // to a non-zero and confirm the parser rejects.
        let mut buf = build_aligned_idx();
        let off_pos = HEADER_FIXED_LEN + 8;
        let video_meta_start =
            u64::from_be_bytes(buf.as_slice()[off_pos..off_pos + 8].try_into().unwrap()) as usize;
        let trailing_pad_offset = video_meta_start + 27;
        buf.as_mut_slice()[trailing_pad_offset] = 0xFF;

        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexSection(_)),
        ));
    }

    #[test]
    fn corrupt_magic_returns_index_magic_mismatch() {
        let mut buf = build_aligned_idx();
        buf.as_mut_slice()[0] = b'X';
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::IndexMagicMismatch),
        ));
    }

    #[test]
    fn truncated_header_returns_malformed_directory() {
        let raw = build_minimal_idx();
        let aligned = AlignedBuffer::from_bytes(&raw[..40]);
        assert!(matches!(
            IndexView::open(aligned.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn section_count_above_max_returns_malformed_directory() {
        let mut buf = build_aligned_idx();
        buf.as_mut_slice()[48..52].copy_from_slice(&((MAX_SECTIONS as u32) + 1).to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn nonzero_reserved_returns_malformed_directory() {
        let mut buf = build_aligned_idx();
        buf.as_mut_slice()[52..56].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn duplicate_section_kind_returns_malformed_directory() {
        let mut buf = build_aligned_idx();
        // Overwrite the second directory entry's kind to match the first.
        let pos = HEADER_FIXED_LEN + SECTION_DIR_ENTRY_LEN;
        buf.as_mut_slice()[pos..pos + 4].copy_from_slice(&KIND_VIDEO_TRACK_META.to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn non_ascending_offsets_return_malformed_directory() {
        let mut buf = build_aligned_idx();
        // Swap the first two directory entries' offsets — first now points
        // past the second.
        let pos1 = HEADER_FIXED_LEN + 8;
        let pos2 = HEADER_FIXED_LEN + SECTION_DIR_ENTRY_LEN + 8;
        let off1 = u64::from_be_bytes(buf.as_slice()[pos1..pos1 + 8].try_into().unwrap());
        let off2 = u64::from_be_bytes(buf.as_slice()[pos2..pos2 + 8].try_into().unwrap());
        buf.as_mut_slice()[pos1..pos1 + 8].copy_from_slice(&off2.to_be_bytes());
        buf.as_mut_slice()[pos2..pos2 + 8].copy_from_slice(&off1.to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn misaligned_section_offset_returns_malformed_directory() {
        let mut buf = build_aligned_idx();
        // Bump the first section offset by 1 — now no longer 8-aligned.
        let pos = HEADER_FIXED_LEN + 8;
        let off = u64::from_be_bytes(buf.as_slice()[pos..pos + 8].try_into().unwrap());
        buf.as_mut_slice()[pos..pos + 8].copy_from_slice(&(off + 1).to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }

    #[test]
    fn first_offset_below_header_end_returns_malformed_directory() {
        let mut buf = build_aligned_idx();
        let pos = HEADER_FIXED_LEN + 8;
        // Force the first offset to 8 — way before header_end.
        buf.as_mut_slice()[pos..pos + 8].copy_from_slice(&8u64.to_be_bytes());
        assert!(matches!(
            IndexView::open(buf.as_slice()),
            Err(PackagerError::MalformedIndexDirectory(_)),
        ));
    }
}
