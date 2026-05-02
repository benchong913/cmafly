//! `.idx` format types and reader/writer entry points.
//!
//! - [`format`] — on-disk constants and `#[repr(C)]` row types
//!   (`SampleEntry`, `SegmentEntry`). Single source of truth for both the
//!   `IndexBuilder` writer and the `IndexView::open` reader.
//! - [`view`] — borrowed [`view::IndexView`] over a validated `.idx` byte
//!   buffer. The fMP4 writers consume this view.
//! - [`builder`] — emits the `.idx` bytes that round-trip through `IndexView`.

pub(crate) mod builder;
pub(crate) mod format;
pub(crate) mod view;

pub use builder::IndexBuilder;
pub use format::{SampleEntry, SegmentEntry};
pub use view::{AudioTrackMeta, IndexView, VideoTrackMeta};
