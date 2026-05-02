//! fMP4 (CMAF) writers.
//!
//! [`write_init_segment`] produces the `ftyp` + `moov` payload that lives
//! once per archive (embedded in the `.idx`); [`write_media_segment`]
//! produces the per-request `styp` + `moof` + `mdat` payload assembled by
//! `cmafly-serve`. Both writers sit on top of [`boxes::BoxWriter`], whose
//! always-on box-size assertion guards output correctness.

pub(crate) mod boxes;
pub(crate) mod init;
pub(crate) mod media;

pub use init::write_init_segment;
pub use media::write_media_segment;
