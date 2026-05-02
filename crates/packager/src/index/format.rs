//! On-disk `.idx` format types and constants.
//!
//! `SampleEntry` and `SegmentEntry` are `#[repr(C)]` POD records with
//! deliberate field ordering that produces zero compiler padding.
//! Compile-time `assert!`s lock the size and alignment so any padding
//! regression is caught at build time, before the unsafe slice cast in
//! [`super::view::IndexView`] can hand out misaligned rows.
//!
//! Header and section-directory constants live here too: single source of
//! truth shared by [`super::builder::IndexBuilder`] and
//! [`super::view::IndexView`].

/// `.idx` magic — first 4 bytes of every file.
pub(crate) const MAGIC: [u8; 4] = *b"HCMI";

/// Fixed-length prefix: magic + max_segment_size + source_mp4_len +
/// source_mp4_blake3 + section_count + reserved.
pub(crate) const HEADER_FIXED_LEN: usize = 56;

/// Per-entry size of the section directory: `kind: u32 + _pad: u32 +
/// offset: u64`.
pub(crate) const SECTION_DIR_ENTRY_LEN: usize = 16;

/// Maximum number of sections a single `.idx` may declare. Bounded so
/// `IndexView::open` cannot loop on an attacker-supplied directory.
pub(crate) const MAX_SECTIONS: usize = 64;

pub(crate) const KIND_VIDEO_TRACK_META: u32 = 0x01;
pub(crate) const KIND_AUDIO_TRACK_META: u32 = 0x02;
pub(crate) const KIND_VIDEO_SAMPLE_TABLE: u32 = 0x03;
pub(crate) const KIND_AUDIO_SAMPLE_TABLE: u32 = 0x04;
pub(crate) const KIND_SEGMENT_TABLE: u32 = 0x05;
pub(crate) const KIND_INIT_SEGMENT_BYTES: u32 = 0x06;
pub(crate) const KIND_PLAYLIST_BYTES: u32 = 0x07;

/// Per-sample row recorded in `VideoSampleTable` / `AudioSampleTable`
/// sections of the `.idx`. Field order is contractual: the leading `u64`
/// sits at offset 0, the four 4-byte fields that follow each land on a
/// 4-byte-aligned offset; total `size_of == 24`, `align_of == 8`, zero
/// compiler padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SampleEntry {
    /// Byte offset into the source MP4 where this sample begins.
    pub offset: u64,
    /// Sample size in bytes.
    pub size: u32,
    /// Delta from the previous sample's DTS, in track timescale.
    pub dts_delta: u32,
    /// Composition offset (audio: always 0).
    pub cts_offset: i32,
    /// Bit 0 = is_sync; remaining bits are reserved (must be 0).
    pub flags: u32,
}

/// Bit 0 of [`SampleEntry::flags`] — set iff this is a sync sample.
pub const SAMPLE_FLAG_IS_SYNC: u32 = 0x0000_0001;

impl SampleEntry {
    /// True iff bit 0 of [`Self::flags`] is set.
    pub fn is_sync(&self) -> bool {
        (self.flags & SAMPLE_FLAG_IS_SYNC) != 0
    }
}

const _: () = {
    assert!(std::mem::size_of::<SampleEntry>() == 24);
    assert!(std::mem::align_of::<SampleEntry>() == 8);
};

/// One row of `SegmentTable`. `dts_delta` is stored on `SampleEntry`, so
/// each `SegmentEntry` carries the cumulative DTS at segment start; the
/// fMP4 writer reconstructs absolute DTS via
/// `prefix_sum(dts_delta) + segment.{v,a}_base_dts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SegmentEntry {
    pub video_sample_start: u32,
    pub video_sample_count: u32,
    pub audio_sample_start: u32,
    pub audio_sample_count: u32,
    /// Cumulative DTS at segment start, in video timescale.
    pub video_base_dts: u64,
    /// Cumulative DTS at segment start, in audio timescale.
    pub audio_base_dts: u64,
}

const _: () = {
    assert!(std::mem::size_of::<SegmentEntry>() == 32);
    assert!(std::mem::align_of::<SegmentEntry>() == 8);
};
