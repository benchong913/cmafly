use std::io;

use thiserror::Error;

/// Errors returned by the `cmafly` library.
///
/// Variant names form the public failure contract — downstream callers match
/// on them, so renaming is a breaking change.
#[derive(Debug, Error)]
pub enum PackagerError {
    // ---------- Demux validation (raised by `IndexBuilder::build`). ----------
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("unsupported `ftyp` brand")]
    UnsupportedBrand,

    #[error("required atom `{0}` is missing")]
    MissingAtom(&'static str),

    #[error("malformed atom `{atom}`: {reason}")]
    MalformedAtom {
        atom: &'static str,
        reason: &'static str,
    },

    #[error(
        "unsupported track layout: {video} video track(s), {audio} audio track(s); expected exactly 1 video + 1 audio"
    )]
    UnsupportedTrackLayout { video: u32, audio: u32 },

    #[error("unsupported video codec (sample-entry fourcc not in allow-list)")]
    UnsupportedVideoCodec,

    #[error("unsupported audio codec (sample-entry fourcc not in allow-list)")]
    UnsupportedAudioCodec,

    #[error("input is fragmented (top-level `moof` present)")]
    FragmentedInput,

    #[error(
        "input is encrypted (`senc` / `tenc` / `sinf` present, or `sbgp` / `sgpd` carrying `seig` grouping)"
    )]
    EncryptedInput,

    #[error("sample-table cross-reference inconsistent: {0}")]
    SampleTableInconsistent(&'static str),

    // ---------- `.idx` parsing (raised by `IndexView::open`). ----------
    #[error("`.idx` magic mismatch: first 4 bytes are not `HCMI`")]
    IndexMagicMismatch,

    #[error("malformed `.idx` directory: {0}")]
    MalformedIndexDirectory(&'static str),

    #[error("malformed `.idx` section: {0}")]
    MalformedIndexSection(&'static str),

    // ---------- Output assembly (raised by `fmp4::write_*_segment`). ----------
    #[error("segment index {idx} out of range (segment count = {count})")]
    SegmentIndexOutOfRange { idx: u32, count: u32 },
}
