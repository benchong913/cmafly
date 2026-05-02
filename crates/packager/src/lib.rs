//! `cmafly` — library spine for offline indexing and on-demand
//! CMAF / HLS assembly.
//!
//! Public surface: typed errors, a random-access I/O abstraction, the index
//! builder / view, and the fMP4 / playlist writers.

mod demux;
mod error;
pub mod fmp4;
mod index;
pub mod playlist;
mod read_at;
mod segment;

pub use error::PackagerError;
pub use index::{
    AudioTrackMeta, IndexBuilder, IndexView, SampleEntry, SegmentEntry, VideoTrackMeta,
};
pub use read_at::ReadAt;
