//! IDR-aligned segmentation.
//!
//! Walks the per-track sample tables from `demux::sample_table` and emits
//! the segment table that `IndexBuilder` serializes into the `.idx`. Cuts
//! snap forward to the next IDR (sync sample) so a segment never starts
//! mid-GOP.
#![allow(dead_code)]

pub(crate) mod splitter;
