//! Sample-table walker.
//!
//! Combines the six (plus two optional) sample-table atoms surfaced by the
//! `moov` parser into a single per-sample row table. Each row carries
//! everything the segment splitter and fMP4 writer need:
//! `(offset, size, dts_delta, cts_offset, is_sync)`.
//!
//! Tables are loaded in full before walking; in-memory cost scales with the
//! source's sample count at roughly 30 B / sample.

use byteorder::{BigEndian, ByteOrder};

use super::moov::{ByteRange, ChunkOffsets, SampleTableLocs};
use super::reader::read_exact;
use crate::ReadAt;
use crate::error::PackagerError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawSampleEntry {
    pub(crate) offset: u64,
    pub(crate) size: u32,
    pub(crate) dts_delta: u32,
    pub(crate) cts_offset: i32,
    pub(crate) is_sync: bool,
}

/// Walker output. `stss_absent` is surfaced so the indexer can flag that
/// every video sample was treated as sync because the input had no `stss`.
#[derive(Debug)]
pub(crate) struct WalkedSamples {
    pub(crate) samples: Vec<RawSampleEntry>,
    pub(crate) stss_absent: bool,
}

pub(crate) fn walk<R: ReadAt + ?Sized>(
    reader: &R,
    locs: &SampleTableLocs,
) -> Result<WalkedSamples, PackagerError> {
    let sizes = read_stsz(reader, locs.stsz)?;
    let n = sizes.len();
    let stts = read_stts(reader, locs.stts)?;
    let stsc = read_stsc(reader, locs.stsc)?;
    let chunk_offs = read_chunk_offsets(reader, locs.chunk_offsets)?;

    let stss_absent = locs.stss.is_none();
    let stss_table = locs.stss.map(|r| read_stss(reader, r)).transpose()?;
    let ctts_table = locs.ctts.map(|r| read_ctts(reader, r)).transpose()?;

    let dts_deltas = expand_run_table(&stts, n, "stts")?;
    let cts_offsets: Vec<i32> = match ctts_table {
        Some(t) => expand_run_table(&t, n, "ctts")?,
        None => vec![0i32; n],
    };

    let mut sync = vec![stss_absent; n];
    if let Some(stss) = stss_table {
        for s in stss {
            if s == 0 || (s as usize) > n {
                return Err(PackagerError::SampleTableInconsistent(
                    "stss sample_number out of range",
                ));
            }
            sync[(s - 1) as usize] = true;
        }
    }

    let samples_per_chunk = expand_stsc(&stsc, chunk_offs.len())?;

    let mut samples = Vec::with_capacity(n);
    let mut sample_idx: usize = 0;
    for (chunk_idx, &spc) in samples_per_chunk.iter().enumerate() {
        let mut off = chunk_offs[chunk_idx];
        for _ in 0..spc {
            if sample_idx >= n {
                return Err(PackagerError::SampleTableInconsistent(
                    "stsc sample count exceeds stsz",
                ));
            }
            let size = sizes[sample_idx];
            samples.push(RawSampleEntry {
                offset: off,
                size,
                dts_delta: dts_deltas[sample_idx],
                cts_offset: cts_offsets[sample_idx],
                is_sync: sync[sample_idx],
            });
            off = off
                .checked_add(size as u64)
                .ok_or(PackagerError::SampleTableInconsistent(
                    "sample offset overflows u64",
                ))?;
            sample_idx += 1;
        }
    }
    if sample_idx != n {
        return Err(PackagerError::SampleTableInconsistent(
            "stsc sample count disagrees with stsz",
        ));
    }

    Ok(WalkedSamples {
        samples,
        stss_absent,
    })
}

fn expand_run_table<T: Copy + Default>(
    runs: &[(u32, T)],
    expected_total: usize,
    atom: &'static str,
) -> Result<Vec<T>, PackagerError> {
    let mut out = Vec::with_capacity(expected_total);
    for (count, value) in runs {
        for _ in 0..*count {
            if out.len() == expected_total {
                return Err(PackagerError::SampleTableInconsistent(match atom {
                    "stts" => "stts run-counts exceed stsz sample count",
                    "ctts" => "ctts run-counts exceed stsz sample count",
                    _ => "run-table exceeds expected sample count",
                }));
            }
            out.push(*value);
        }
    }
    if out.len() != expected_total {
        return Err(PackagerError::SampleTableInconsistent(match atom {
            "stts" => "stts run-counts disagree with stsz sample count",
            "ctts" => "ctts run-counts disagree with stsz sample count",
            _ => "run-table disagrees with expected sample count",
        }));
    }
    Ok(out)
}

fn expand_stsc(stsc: &[(u32, u32, u32)], chunk_count: usize) -> Result<Vec<u32>, PackagerError> {
    let mut samples_per_chunk = vec![0u32; chunk_count];
    if stsc.is_empty() {
        if chunk_count != 0 {
            return Err(PackagerError::SampleTableInconsistent(
                "stsc empty but stco/co64 has chunks",
            ));
        }
        return Ok(samples_per_chunk);
    }
    if stsc[0].0 != 1 {
        return Err(PackagerError::SampleTableInconsistent(
            "stsc first entry does not start at chunk 1",
        ));
    }
    for i in 0..stsc.len() {
        let (first_chunk, spc, _sdi) = stsc[i];
        if first_chunk == 0 {
            return Err(PackagerError::SampleTableInconsistent(
                "stsc first_chunk is zero",
            ));
        }
        let to_exclusive = if i + 1 < stsc.len() {
            let next = stsc[i + 1].0;
            if next <= first_chunk {
                return Err(PackagerError::SampleTableInconsistent(
                    "stsc first_chunk not strictly ascending",
                ));
            }
            (next - 1) as usize
        } else {
            chunk_count
        };
        let from = (first_chunk - 1) as usize;
        // Each entry must cover at least one chunk; `from == chunk_count`
        // would mean the entry starts one past the last chunk.
        if from >= chunk_count || to_exclusive > chunk_count {
            return Err(PackagerError::SampleTableInconsistent(
                "stsc first_chunk index out of range vs stco length",
            ));
        }
        for slot in &mut samples_per_chunk[from..to_exclusive] {
            *slot = spc;
        }
    }
    Ok(samples_per_chunk)
}

fn read_stsz<R: ReadAt + ?Sized>(reader: &R, range: ByteRange) -> Result<Vec<u32>, PackagerError> {
    let payload = read_payload(reader, range, 12)?;
    ensure_full_box_version(&payload, "stsz", &[0])?;
    let sample_size = BigEndian::read_u32(&payload[4..8]);
    let sample_count = BigEndian::read_u32(&payload[8..12]) as usize;
    let body = &payload[12..];
    if sample_size != 0 {
        if !body.is_empty() {
            return Err(PackagerError::MalformedAtom {
                atom: "stsz",
                reason: "constant sample_size has trailing per-sample entries",
            });
        }
        return Ok(vec![sample_size; sample_count]);
    }
    if body.len() != entry_bytes(sample_count, 4, "stsz")? {
        return Err(PackagerError::MalformedAtom {
            atom: "stsz",
            reason: "entry table size disagrees with sample_count",
        });
    }
    let mut out = Vec::with_capacity(sample_count);
    for chunk in body.chunks_exact(4) {
        out.push(BigEndian::read_u32(chunk));
    }
    Ok(out)
}

/// Read a `[version+flags(4) | entry_count(4) | entries...]` full-box table
/// where every entry has a fixed `bytes_per_entry`. The `parse_entry` callback
/// receives the box's full-box version and one entry slice; it may surface a
/// per-entry validation error (e.g. `stsc` rejecting `sample_description_index
/// != 1`).
fn read_full_box_table<R: ReadAt + ?Sized, T>(
    reader: &R,
    range: ByteRange,
    atom: &'static str,
    allowed_versions: &[u8],
    bytes_per_entry: usize,
    mut parse_entry: impl FnMut(u8, &[u8]) -> Result<T, PackagerError>,
) -> Result<Vec<T>, PackagerError> {
    let payload = read_payload(reader, range, 8)?;
    let version = ensure_full_box_version(&payload, atom, allowed_versions)?;
    let entry_count = BigEndian::read_u32(&payload[4..8]) as usize;
    let body = &payload[8..];
    if body.len() != entry_bytes(entry_count, bytes_per_entry, atom)? {
        return Err(PackagerError::MalformedAtom {
            atom,
            reason: "entry table size disagrees with entry_count",
        });
    }
    let mut out = Vec::with_capacity(entry_count);
    for chunk in body.chunks_exact(bytes_per_entry) {
        out.push(parse_entry(version, chunk)?);
    }
    Ok(out)
}

fn read_stts<R: ReadAt + ?Sized>(
    reader: &R,
    range: ByteRange,
) -> Result<Vec<(u32, u32)>, PackagerError> {
    read_full_box_table(reader, range, "stts", &[0], 8, |_, c| {
        Ok((BigEndian::read_u32(&c[..4]), BigEndian::read_u32(&c[4..])))
    })
}

fn read_ctts<R: ReadAt + ?Sized>(
    reader: &R,
    range: ByteRange,
) -> Result<Vec<(u32, i32)>, PackagerError> {
    read_full_box_table(reader, range, "ctts", &[0, 1], 8, |version, c| {
        let count = BigEndian::read_u32(&c[..4]);
        let off = if version == 0 {
            let raw = BigEndian::read_u32(&c[4..]);
            i32::try_from(raw).map_err(|_| PackagerError::MalformedAtom {
                atom: "ctts",
                reason: "version 0 sample_offset exceeds i32 range",
            })?
        } else {
            BigEndian::read_i32(&c[4..])
        };
        Ok((count, off))
    })
}

fn read_stss<R: ReadAt + ?Sized>(reader: &R, range: ByteRange) -> Result<Vec<u32>, PackagerError> {
    read_full_box_table(reader, range, "stss", &[0], 4, |_, c| {
        Ok(BigEndian::read_u32(c))
    })
}

fn read_stsc<R: ReadAt + ?Sized>(
    reader: &R,
    range: ByteRange,
) -> Result<Vec<(u32, u32, u32)>, PackagerError> {
    read_full_box_table(reader, range, "stsc", &[0], 12, |_, c| {
        let fc = BigEndian::read_u32(&c[..4]);
        let spc = BigEndian::read_u32(&c[4..8]);
        let sdi = BigEndian::read_u32(&c[8..]);
        // stsd enforces entry_count == 1, so every sample must reference
        // sample-description index 1.
        if sdi != 1 {
            return Err(PackagerError::MalformedAtom {
                atom: "stsc",
                reason: "sample_description_index must be 1",
            });
        }
        Ok((fc, spc, sdi))
    })
}

fn read_chunk_offsets<R: ReadAt + ?Sized>(
    reader: &R,
    co: ChunkOffsets,
) -> Result<Vec<u64>, PackagerError> {
    match co {
        ChunkOffsets::Stco(range) => read_full_box_table(reader, range, "stco", &[0], 4, |_, c| {
            Ok(BigEndian::read_u32(c) as u64)
        }),
        ChunkOffsets::Co64(range) => read_full_box_table(reader, range, "co64", &[0], 8, |_, c| {
            Ok(BigEndian::read_u64(c))
        }),
    }
}

fn read_payload<R: ReadAt + ?Sized>(
    reader: &R,
    range: ByteRange,
    min_len: usize,
) -> Result<Vec<u8>, PackagerError> {
    let len = usize::try_from(range.len).map_err(|_| PackagerError::MalformedAtom {
        atom: "<sample-table>",
        reason: "payload length does not fit usize",
    })?;
    if len < min_len {
        return Err(PackagerError::MalformedAtom {
            atom: "<sample-table>",
            reason: "payload shorter than minimum full-box header",
        });
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|_| PackagerError::MalformedAtom {
            atom: "<sample-table>",
            reason: "sample-table payload too large to allocate",
        })?;
    buf.resize(len, 0);
    read_exact(reader, range.offset, &mut buf)?;
    Ok(buf)
}

fn ensure_full_box_version(
    payload: &[u8],
    atom: &'static str,
    allowed: &[u8],
) -> Result<u8, PackagerError> {
    let version = payload[0];
    if allowed.contains(&version) {
        Ok(version)
    } else {
        Err(PackagerError::MalformedAtom {
            atom,
            reason: "unsupported full-box version",
        })
    }
}

fn entry_bytes(
    entry_count: usize,
    bytes_per_entry: usize,
    atom: &'static str,
) -> Result<usize, PackagerError> {
    entry_count
        .checked_mul(bytes_per_entry)
        .ok_or(PackagerError::MalformedAtom {
            atom,
            reason: "entry table size overflows usize",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_at::SliceReader;

    /// Compose a contiguous payload-only buffer plus a `SampleTableLocs`
    /// that points into it. The buffer here is not a real MP4 — each table's
    /// `ByteRange` indexes directly into the payload bytes for that table.
    struct SampleTableHarness {
        bytes: Vec<u8>,
        locs: SampleTableLocs,
    }

    fn full_box_payload(version: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + body.len());
        out.push(version);
        out.extend_from_slice(&[0, 0, 0]); // flags
        out.extend_from_slice(body);
        out
    }

    fn append(bytes: &mut Vec<u8>, payload: Vec<u8>) -> ByteRange {
        let offset = bytes.len() as u64;
        let len = payload.len() as u64;
        bytes.extend_from_slice(&payload);
        ByteRange { offset, len }
    }

    fn build_harness(
        sample_sizes: &[u32],
        stts_runs: &[(u32, u32)],
        stsc_entries: &[(u32, u32, u32)],
        chunk_offsets: ChunkOffsetsBuilder,
        stss: Option<&[u32]>,
        ctts_runs: Option<&[(u32, i32)]>,
    ) -> SampleTableHarness {
        let mut bytes = Vec::new();

        let mut stsz_body = Vec::new();
        stsz_body.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0
        stsz_body.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
        for s in sample_sizes {
            stsz_body.extend_from_slice(&s.to_be_bytes());
        }
        let stsz = append(&mut bytes, full_box_payload(0, &stsz_body));

        let mut stts_body = Vec::new();
        stts_body.extend_from_slice(&(stts_runs.len() as u32).to_be_bytes());
        for (count, delta) in stts_runs {
            stts_body.extend_from_slice(&count.to_be_bytes());
            stts_body.extend_from_slice(&delta.to_be_bytes());
        }
        let stts = append(&mut bytes, full_box_payload(0, &stts_body));

        let mut stsc_body = Vec::new();
        stsc_body.extend_from_slice(&(stsc_entries.len() as u32).to_be_bytes());
        for (fc, spc, sdi) in stsc_entries {
            stsc_body.extend_from_slice(&fc.to_be_bytes());
            stsc_body.extend_from_slice(&spc.to_be_bytes());
            stsc_body.extend_from_slice(&sdi.to_be_bytes());
        }
        let stsc = append(&mut bytes, full_box_payload(0, &stsc_body));

        let chunk_offsets = match chunk_offsets {
            ChunkOffsetsBuilder::Stco(offs) => {
                let mut body = Vec::new();
                body.extend_from_slice(&(offs.len() as u32).to_be_bytes());
                for o in &offs {
                    body.extend_from_slice(&o.to_be_bytes());
                }
                let r = append(&mut bytes, full_box_payload(0, &body));
                ChunkOffsets::Stco(r)
            }
            ChunkOffsetsBuilder::Co64(offs) => {
                let mut body = Vec::new();
                body.extend_from_slice(&(offs.len() as u32).to_be_bytes());
                for o in &offs {
                    body.extend_from_slice(&o.to_be_bytes());
                }
                let r = append(&mut bytes, full_box_payload(0, &body));
                ChunkOffsets::Co64(r)
            }
        };

        let stss_range = stss.map(|samples| {
            let mut body = Vec::new();
            body.extend_from_slice(&(samples.len() as u32).to_be_bytes());
            for s in samples {
                body.extend_from_slice(&s.to_be_bytes());
            }
            append(&mut bytes, full_box_payload(0, &body))
        });

        let ctts_range = ctts_runs.map(|runs| {
            let mut body = Vec::new();
            body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
            for (count, off) in runs {
                body.extend_from_slice(&count.to_be_bytes());
                body.extend_from_slice(&(*off as u32).to_be_bytes());
            }
            append(&mut bytes, full_box_payload(0, &body))
        });

        SampleTableHarness {
            bytes,
            locs: SampleTableLocs {
                stts,
                stsc,
                stsz,
                chunk_offsets,
                stss: stss_range,
                ctts: ctts_range,
            },
        }
    }

    enum ChunkOffsetsBuilder {
        Stco(Vec<u32>),
        Co64(Vec<u64>),
    }

    #[test]
    fn four_samples_mixed_sync() {
        let h = build_harness(
            &[10, 20, 30, 40],
            &[(4, 100)],
            &[(1, 4, 1)],
            ChunkOffsetsBuilder::Stco(vec![1000]),
            Some(&[1, 3]),
            None,
        );
        let reader = SliceReader(&h.bytes);
        let walked = walk(&reader, &h.locs).unwrap();
        assert!(!walked.stss_absent);
        let s = &walked.samples;
        assert_eq!(s.len(), 4);
        assert_eq!(
            s[0],
            RawSampleEntry {
                offset: 1000,
                size: 10,
                dts_delta: 100,
                cts_offset: 0,
                is_sync: true
            }
        );
        assert_eq!(
            s[1],
            RawSampleEntry {
                offset: 1010,
                size: 20,
                dts_delta: 100,
                cts_offset: 0,
                is_sync: false
            }
        );
        assert_eq!(
            s[2],
            RawSampleEntry {
                offset: 1030,
                size: 30,
                dts_delta: 100,
                cts_offset: 0,
                is_sync: true
            }
        );
        assert_eq!(
            s[3],
            RawSampleEntry {
                offset: 1060,
                size: 40,
                dts_delta: 100,
                cts_offset: 0,
                is_sync: false
            }
        );
    }

    #[test]
    fn stss_absent_means_all_sync() {
        let h = build_harness(
            &[10, 10, 10, 10],
            &[(4, 100)],
            &[(1, 4, 1)],
            ChunkOffsetsBuilder::Stco(vec![1000]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let walked = walk(&reader, &h.locs).unwrap();
        assert!(walked.stss_absent);
        assert!(walked.samples.iter().all(|s| s.is_sync));
    }

    #[test]
    fn ctts_offsets_are_applied() {
        let h = build_harness(
            &[10, 10, 10, 10],
            &[(4, 100)],
            &[(1, 4, 1)],
            ChunkOffsetsBuilder::Stco(vec![1000]),
            None,
            Some(&[(1, 0), (2, 50), (1, 0)]),
        );
        let reader = SliceReader(&h.bytes);
        let walked = walk(&reader, &h.locs).unwrap();
        let cts: Vec<i32> = walked.samples.iter().map(|s| s.cts_offset).collect();
        assert_eq!(cts, vec![0, 50, 50, 0]);
    }

    #[test]
    fn co64_chunk_offsets_supported() {
        let h = build_harness(
            &[5, 5],
            &[(2, 33)],
            &[(1, 1, 1), (2, 1, 1)],
            ChunkOffsetsBuilder::Co64(vec![5_000_000_000, 5_000_000_010]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let walked = walk(&reader, &h.locs).unwrap();
        assert_eq!(walked.samples.len(), 2);
        assert_eq!(walked.samples[0].offset, 5_000_000_000);
        assert_eq!(walked.samples[1].offset, 5_000_000_010);
    }

    #[test]
    fn stsc_index_out_of_range_rejected() {
        let h = build_harness(
            &[10, 10],
            &[(2, 100)],
            &[(1, 1, 1), (5, 1, 1)], // claims chunk 5 but only 2 chunks present
            ChunkOffsetsBuilder::Stco(vec![100, 200]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let err = walk(&reader, &h.locs).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn stts_count_mismatch_rejected() {
        let h = build_harness(
            &[10, 10, 10, 10],
            &[(3, 100)], // covers only 3 of 4 samples
            &[(1, 4, 1)],
            ChunkOffsetsBuilder::Stco(vec![100]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let err = walk(&reader, &h.locs).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    fn raw_ctts(version: u8, entries: &[(u32, u32)]) -> (Vec<u8>, ByteRange) {
        let mut bytes = Vec::new();
        let mut body = Vec::new();
        body.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (count, off_bits) in entries {
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&off_bits.to_be_bytes());
        }
        let range = append(&mut bytes, full_box_payload(version, &body));
        (bytes, range)
    }

    #[test]
    fn ctts_v1_negative_offsets_preserved() {
        let raw = (-7i32) as u32;
        let (bytes, range) = raw_ctts(1, &[(2, raw)]);
        let reader = SliceReader(&bytes);
        assert_eq!(read_ctts(&reader, range).unwrap(), vec![(2, -7)]);
    }

    #[test]
    fn ctts_v0_offset_above_i32_max_rejected() {
        let (bytes, range) = raw_ctts(0, &[(1, 0x8000_0000)]);
        let reader = SliceReader(&bytes);
        let err = read_ctts(&reader, range).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::MalformedAtom { atom: "ctts", .. }
        ));
    }

    #[test]
    fn ctts_unsupported_version_rejected() {
        let (bytes, range) = raw_ctts(2, &[]);
        let reader = SliceReader(&bytes);
        let err = read_ctts(&reader, range).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::MalformedAtom { atom: "ctts", .. }
        ));
    }

    #[test]
    fn stsc_first_chunk_one_past_chunk_count_rejected() {
        // chunk_count = 2, but stsc[1].first_chunk = 3 — entry covers no chunk.
        let h = build_harness(
            &[10, 10],
            &[(2, 100)],
            &[(1, 1, 1), (3, 1, 1)],
            ChunkOffsetsBuilder::Stco(vec![100, 200]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let err = walk(&reader, &h.locs).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn stsc_sample_description_index_must_be_one() {
        let h = build_harness(
            &[10],
            &[(1, 100)],
            &[(1, 1, 2)],
            ChunkOffsetsBuilder::Stco(vec![100]),
            None,
            None,
        );
        let reader = SliceReader(&h.bytes);
        let err = walk(&reader, &h.locs).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::MalformedAtom { atom: "stsc", .. }
        ));
    }

    #[test]
    fn stsz_constant_size_with_trailing_bytes_rejected() {
        // sample_size != 0 must mean "no per-sample table follows."
        let mut bytes = Vec::new();
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_be_bytes()); // sample_size
        body.extend_from_slice(&3u32.to_be_bytes()); // sample_count
        body.extend_from_slice(&[0u8; 12]); // illegal trailing per-sample data
        let range = append(&mut bytes, full_box_payload(0, &body));
        let reader = SliceReader(&bytes);
        let err = read_stsz(&reader, range).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::MalformedAtom { atom: "stsz", .. }
        ));
    }
}
