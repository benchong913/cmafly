//! Internal MP4 demuxer.
//!
//! Identifies exactly one video and one audio track, snapshots
//! codec-config / `edts` / sample-table byte ranges, and rejects anything
//! outside the allow-list. Library-private; consumed only by `IndexBuilder`.
#![allow(dead_code)]

pub(crate) mod moov;
pub(crate) mod reader;
pub(crate) mod sample_table;
