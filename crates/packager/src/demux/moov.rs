//! `moov` subtree parser.
//!
//! Walks the top-level `ftyp` + `moov` boxes, gathers per-track meta
//! (`tkhd` / `mdhd` / `hdlr`), snapshots the verbatim sample-entry +
//! `edts` byte ranges, and locates the sample-table child boxes
//! (`stts` / `ctts` / `stss` / `stsc` / `stsz` / `stco|co64`) for the
//! sample-table walker to consume. All input-shape validation gates fire
//! here, not later.
//!
//! The parser never copies sample data: codec config, edit lists, and sample
//! tables are returned as `(offset, len)` ranges into the original `ReadAt`
//! source. `IndexBuilder` is what later copies them into the `.idx` byte
//! image.

use byteorder::{BigEndian, ByteOrder};

use super::reader::{BoxHeader, BoxIter, read_exact};
use crate::ReadAt;
use crate::error::PackagerError;

const ALLOWED_BRANDS: [[u8; 4]; 8] = [
    *b"isom", *b"mp42", *b"iso2", *b"iso4", *b"iso5", *b"iso6", *b"cmfc", *b"mp41",
];
const VIDEO_FOURCCS: [[u8; 4]; 3] = [*b"avc1", *b"hvc1", *b"hev1"];
const AUDIO_FOURCCS: [[u8; 4]; 1] = [*b"mp4a"];

const HANDLER_VIDEO: [u8; 4] = *b"vide";
const HANDLER_AUDIO: [u8; 4] = *b"soun";

/// A half-open byte range `[offset, offset + len)` into the source MP4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

/// Locations of the sample-table atoms inside a single track's `stbl`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleTableLocs {
    pub(crate) stts: ByteRange,
    pub(crate) stsc: ByteRange,
    pub(crate) stsz: ByteRange,
    pub(crate) chunk_offsets: ChunkOffsets,
    pub(crate) stss: Option<ByteRange>,
    pub(crate) ctts: Option<ByteRange>,
}

/// Either `stco` (32-bit) or `co64` (64-bit) chunk-offset table.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ChunkOffsets {
    Stco(ByteRange),
    Co64(ByteRange),
}

/// Per-track output from the moov walker. `kind` is set after sorting tracks
/// by handler type at the end of [`parse_top_level`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ParsedTrack {
    pub(crate) timescale: u32,
    pub(crate) fourcc: [u8; 4],
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) sample_rate: u32,
    pub(crate) channel_count: u8,
    pub(crate) sample_entry: ByteRange,
    pub(crate) elst: Option<ByteRange>,
    pub(crate) sample_table: SampleTableLocs,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MoovParse {
    pub(crate) video: ParsedTrack,
    pub(crate) audio: ParsedTrack,
}

/// Walk the top-level boxes of `[0, source_len)`, run the input-shape
/// gates, and return the classified video + audio tracks. Top-level
/// `moof`, fragmented input, and any encryption marker reject early.
pub(crate) fn parse_top_level<R: ReadAt + ?Sized>(
    reader: &R,
    source_len: u64,
) -> Result<MoovParse, PackagerError> {
    let mut ftyp_seen = false;
    let mut moov: Option<BoxHeader> = None;

    let mut iter = BoxIter::new(reader, 0, source_len);
    while let Some(header) = iter.next_header()? {
        match &header.box_type {
            b"ftyp" => {
                check_ftyp(reader, &header)?;
                ftyp_seen = true;
            }
            b"moov" => {
                if moov.is_some() {
                    return Err(PackagerError::MalformedAtom {
                        atom: "moov",
                        reason: "more than one top-level moov",
                    });
                }
                moov = Some(header);
            }
            b"moof" => return Err(PackagerError::FragmentedInput),
            b"senc" | b"sbgp" | b"sgpd" | b"tenc" | b"sinf" => {
                return Err(PackagerError::EncryptedInput);
            }
            _ => {}
        }
    }

    if !ftyp_seen {
        return Err(PackagerError::MissingAtom("ftyp"));
    }
    let moov = moov.ok_or(PackagerError::MissingAtom("moov"))?;

    let mut tracks: Vec<UnclassifiedTrack> = Vec::new();
    let mut iter = BoxIter::new(reader, moov.payload_offset, moov.end());
    while let Some(child) = iter.next_header()? {
        if &child.box_type == b"trak" {
            tracks.push(parse_trak(reader, &child)?);
        }
        // mvhd / udta / meta / iods etc. are ignored.
    }

    classify_tracks(tracks)
}

fn check_ftyp<R: ReadAt + ?Sized>(reader: &R, header: &BoxHeader) -> Result<(), PackagerError> {
    let payload_len = header.payload_len();
    if payload_len < 8 {
        return Err(PackagerError::MalformedAtom {
            atom: "ftyp",
            reason: "payload shorter than major+minor brand",
        });
    }
    let mut buf = vec![0u8; payload_len as usize];
    read_exact(reader, header.payload_offset, &mut buf)?;

    let mut major = [0u8; 4];
    major.copy_from_slice(&buf[..4]);
    if ALLOWED_BRANDS.contains(&major) {
        return Ok(());
    }
    // Skip 4-byte minor_version; remaining payload is a sequence of compatible brands.
    let compat = &buf[8..];
    if !compat.len().is_multiple_of(4) {
        return Err(PackagerError::MalformedAtom {
            atom: "ftyp",
            reason: "compatible-brands list is not a multiple of 4 bytes",
        });
    }
    for chunk in compat.chunks_exact(4) {
        let mut brand = [0u8; 4];
        brand.copy_from_slice(chunk);
        if ALLOWED_BRANDS.contains(&brand) {
            return Ok(());
        }
    }
    Err(PackagerError::UnsupportedBrand)
}

#[derive(Debug, Clone, Copy)]
struct UnclassifiedTrack {
    handler: [u8; 4],
    timescale: u32,
    width: u32,
    height: u32,
    fourcc: [u8; 4],
    sample_entry: ByteRange,
    sample_rate: u32,
    channel_count: u8,
    elst: Option<ByteRange>,
    sample_table: SampleTableLocs,
}

fn parse_trak<R: ReadAt + ?Sized>(
    reader: &R,
    trak: &BoxHeader,
) -> Result<UnclassifiedTrack, PackagerError> {
    let mut tkhd: Option<TkhdInfo> = None;
    let mut mdia: Option<BoxHeader> = None;
    let mut elst: Option<ByteRange> = None;

    let mut iter = BoxIter::new(reader, trak.payload_offset, trak.end());
    while let Some(child) = iter.next_header()? {
        match &child.box_type {
            b"tkhd" => tkhd = Some(parse_tkhd(reader, &child)?),
            b"mdia" => mdia = Some(child),
            b"edts" => {
                elst = Some(ByteRange {
                    offset: child.start,
                    len: child.declared_size,
                });
            }
            _ => {}
        }
    }

    let tkhd = tkhd.ok_or(PackagerError::MissingAtom("tkhd"))?;
    let mdia = mdia.ok_or(PackagerError::MissingAtom("mdia"))?;
    let mdia_data = parse_mdia(reader, &mdia)?;

    let (width, height) = if mdia_data.handler == HANDLER_VIDEO {
        (tkhd.width, tkhd.height)
    } else {
        (0, 0)
    };

    Ok(UnclassifiedTrack {
        handler: mdia_data.handler,
        timescale: mdia_data.timescale,
        width,
        height,
        fourcc: mdia_data.fourcc,
        sample_entry: mdia_data.sample_entry,
        sample_rate: mdia_data.sample_rate,
        channel_count: mdia_data.channel_count,
        elst,
        sample_table: mdia_data.sample_table,
    })
}

#[derive(Debug, Clone, Copy)]
struct TkhdInfo {
    width: u32,
    height: u32,
}

fn parse_tkhd<R: ReadAt + ?Sized>(reader: &R, tkhd: &BoxHeader) -> Result<TkhdInfo, PackagerError> {
    let payload_len = tkhd.payload_len();
    if payload_len < 4 {
        return Err(PackagerError::MalformedAtom {
            atom: "tkhd",
            reason: "payload missing version+flags",
        });
    }
    let mut header = [0u8; 4];
    read_exact(reader, tkhd.payload_offset, &mut header)?;
    let version = header[0];
    let body_off = tkhd.payload_offset + 4;
    // v0 body: u32 ctime + u32 mtime + u32 track_id + u32 reserved + u32 dur + 8 reserved
    //          + 2 layer + 2 alt + 2 vol + 2 reserved + 36 matrix + 4 width + 4 height = 80
    // v1 body: u64 ctime + u64 mtime + u32 track_id + u32 reserved + u64 dur + 8 reserved
    //          + 2 layer + 2 alt + 2 vol + 2 reserved + 36 matrix + 4 width + 4 height = 92
    let (body_len, w_off): (u64, u64) = match version {
        0 => (80, 80 - 8),
        1 => (92, 92 - 8),
        _ => {
            return Err(PackagerError::MalformedAtom {
                atom: "tkhd",
                reason: "unsupported version",
            });
        }
    };
    if payload_len < 4 + body_len {
        return Err(PackagerError::MalformedAtom {
            atom: "tkhd",
            reason: "payload truncated before width/height",
        });
    }
    let mut wh = [0u8; 8];
    read_exact(reader, body_off + w_off, &mut wh)?;
    let width = BigEndian::read_u32(&wh[..4]) >> 16;
    let height = BigEndian::read_u32(&wh[4..]) >> 16;
    Ok(TkhdInfo { width, height })
}

#[derive(Debug, Clone, Copy)]
struct MdiaData {
    handler: [u8; 4],
    timescale: u32,
    fourcc: [u8; 4],
    sample_entry: ByteRange,
    sample_rate: u32,
    channel_count: u8,
    sample_table: SampleTableLocs,
}

fn parse_mdia<R: ReadAt + ?Sized>(reader: &R, mdia: &BoxHeader) -> Result<MdiaData, PackagerError> {
    let mut mdhd: Option<u32> = None;
    let mut handler: Option<[u8; 4]> = None;
    let mut minf: Option<BoxHeader> = None;

    let mut iter = BoxIter::new(reader, mdia.payload_offset, mdia.end());
    while let Some(child) = iter.next_header()? {
        match &child.box_type {
            b"mdhd" => mdhd = Some(parse_mdhd_timescale(reader, &child)?),
            b"hdlr" => handler = Some(parse_hdlr_type(reader, &child)?),
            b"minf" => minf = Some(child),
            _ => {}
        }
    }

    let timescale = mdhd.ok_or(PackagerError::MissingAtom("mdhd"))?;
    let handler = handler.ok_or(PackagerError::MissingAtom("hdlr"))?;
    let minf = minf.ok_or(PackagerError::MissingAtom("minf"))?;
    let stbl = locate_stbl(reader, &minf)?;
    let stbl_data = parse_stbl(reader, &stbl, handler)?;

    Ok(MdiaData {
        handler,
        timescale,
        fourcc: stbl_data.fourcc,
        sample_entry: stbl_data.sample_entry,
        sample_rate: stbl_data.sample_rate,
        channel_count: stbl_data.channel_count,
        sample_table: stbl_data.locs,
    })
}

fn parse_mdhd_timescale<R: ReadAt + ?Sized>(
    reader: &R,
    mdhd: &BoxHeader,
) -> Result<u32, PackagerError> {
    let payload_len = mdhd.payload_len();
    if payload_len < 4 {
        return Err(PackagerError::MalformedAtom {
            atom: "mdhd",
            reason: "payload missing version+flags",
        });
    }
    let mut version_flags = [0u8; 4];
    read_exact(reader, mdhd.payload_offset, &mut version_flags)?;
    let version = version_flags[0];
    // v0: 4 ctime + 4 mtime + 4 timescale + 4 dur + 4 lang_pre = 20
    // v1: 8 ctime + 8 mtime + 4 timescale + 8 dur + 4 lang_pre = 32
    let timescale_off = match version {
        0 => 8,  // ctime + mtime
        1 => 16, // ctime + mtime
        _ => {
            return Err(PackagerError::MalformedAtom {
                atom: "mdhd",
                reason: "unsupported version",
            });
        }
    };
    if payload_len < 4 + timescale_off + 4 {
        return Err(PackagerError::MalformedAtom {
            atom: "mdhd",
            reason: "payload truncated before timescale",
        });
    }
    let mut ts = [0u8; 4];
    read_exact(reader, mdhd.payload_offset + 4 + timescale_off, &mut ts)?;
    Ok(BigEndian::read_u32(&ts))
}

fn parse_hdlr_type<R: ReadAt + ?Sized>(
    reader: &R,
    hdlr: &BoxHeader,
) -> Result<[u8; 4], PackagerError> {
    // version+flags(4) + pre_defined(4) + handler_type(4)
    if hdlr.payload_len() < 12 {
        return Err(PackagerError::MalformedAtom {
            atom: "hdlr",
            reason: "payload truncated before handler_type",
        });
    }
    let mut buf = [0u8; 4];
    read_exact(reader, hdlr.payload_offset + 8, &mut buf)?;
    Ok(buf)
}

fn locate_stbl<R: ReadAt + ?Sized>(
    reader: &R,
    minf: &BoxHeader,
) -> Result<BoxHeader, PackagerError> {
    let mut iter = BoxIter::new(reader, minf.payload_offset, minf.end());
    while let Some(child) = iter.next_header()? {
        if &child.box_type == b"stbl" {
            return Ok(child);
        }
    }
    Err(PackagerError::MissingAtom("stbl"))
}

#[derive(Debug, Clone, Copy)]
struct StblData {
    fourcc: [u8; 4],
    sample_entry: ByteRange,
    sample_rate: u32,
    channel_count: u8,
    locs: SampleTableLocs,
}

fn parse_stbl<R: ReadAt + ?Sized>(
    reader: &R,
    stbl: &BoxHeader,
    handler: [u8; 4],
) -> Result<StblData, PackagerError> {
    let mut stsd: Option<BoxHeader> = None;
    let mut stts: Option<ByteRange> = None;
    let mut stsc: Option<ByteRange> = None;
    let mut stsz: Option<ByteRange> = None;
    let mut stco: Option<ByteRange> = None;
    let mut co64: Option<ByteRange> = None;
    let mut stss: Option<ByteRange> = None;
    let mut ctts: Option<ByteRange> = None;

    let mut iter = BoxIter::new(reader, stbl.payload_offset, stbl.end());
    while let Some(child) = iter.next_header()? {
        match &child.box_type {
            b"stsd" => stsd = Some(child),
            b"stts" => stts = Some(payload_range(&child)),
            b"stsc" => stsc = Some(payload_range(&child)),
            b"stsz" => stsz = Some(payload_range(&child)),
            b"stco" => stco = Some(payload_range(&child)),
            b"co64" => co64 = Some(payload_range(&child)),
            b"stss" => stss = Some(payload_range(&child)),
            b"ctts" => ctts = Some(payload_range(&child)),
            // `sbgp` / `sgpd` are general-purpose sample-group containers;
            // they only signal encryption when their `grouping_type` is
            // `seig` (ISO Common Encryption sample auxiliary information).
            // Other grouping types — `rap `, `roll`, `sync`, etc. — are
            // unrelated to encryption and appear in plain MP4s. Reject only
            // the encryption-signalling type so well-formed unencrypted
            // sources with sample-group metadata are not falsely rejected.
            b"sbgp" => check_grouping_type(reader, &child, "sbgp")?,
            b"sgpd" => check_grouping_type(reader, &child, "sgpd")?,
            b"senc" => return Err(PackagerError::EncryptedInput),
            _ => {}
        }
    }

    let stsd = stsd.ok_or(PackagerError::MissingAtom("stsd"))?;
    let entry = parse_stsd_entry(reader, &stsd, handler)?;

    let chunk_offsets = match (stco, co64) {
        (Some(_), Some(_)) => {
            return Err(PackagerError::MalformedAtom {
                atom: "stbl",
                reason: "both stco and co64 present",
            });
        }
        (Some(r), None) => ChunkOffsets::Stco(r),
        (None, Some(r)) => ChunkOffsets::Co64(r),
        (None, None) => return Err(PackagerError::MissingAtom("stco|co64")),
    };

    Ok(StblData {
        fourcc: entry.fourcc,
        sample_entry: entry.sample_entry,
        sample_rate: entry.sample_rate,
        channel_count: entry.channel_count,
        locs: SampleTableLocs {
            stts: stts.ok_or(PackagerError::MissingAtom("stts"))?,
            stsc: stsc.ok_or(PackagerError::MissingAtom("stsc"))?,
            stsz: stsz.ok_or(PackagerError::MissingAtom("stsz"))?,
            chunk_offsets,
            stss,
            ctts,
        },
    })
}

#[derive(Debug, Clone, Copy)]
struct SampleEntryInfo {
    fourcc: [u8; 4],
    sample_entry: ByteRange,
    sample_rate: u32,
    channel_count: u8,
}

fn parse_stsd_entry<R: ReadAt + ?Sized>(
    reader: &R,
    stsd: &BoxHeader,
    handler: [u8; 4],
) -> Result<SampleEntryInfo, PackagerError> {
    if stsd.payload_len() < 8 {
        return Err(PackagerError::MalformedAtom {
            atom: "stsd",
            reason: "payload missing version+flags+entry_count",
        });
    }
    let mut head = [0u8; 8];
    read_exact(reader, stsd.payload_offset, &mut head)?;
    let entry_count = BigEndian::read_u32(&head[4..]);
    // CMAF requires uniform sample format per track; v1 scope is single bitrate
    // / single codec, so a single sample-description entry is the contract.
    if entry_count != 1 {
        return Err(PackagerError::MalformedAtom {
            atom: "stsd",
            reason: "entry_count must be 1",
        });
    }
    let first_off = stsd.payload_offset + 8;
    if first_off >= stsd.end() {
        return Err(PackagerError::MalformedAtom {
            atom: "stsd",
            reason: "no entry follows the entry_count field",
        });
    }
    let mut iter = BoxIter::new(reader, first_off, stsd.end());
    let entry = iter.next_header()?.ok_or(PackagerError::MalformedAtom {
        atom: "stsd",
        reason: "entry table empty",
    })?;
    if entry.end() != stsd.end() {
        return Err(PackagerError::MalformedAtom {
            atom: "stsd",
            reason: "entry_count is 1 but extra bytes follow the sample entry",
        });
    }

    // Encryption check fires before the codec allow-list so an `encv` / `enca`
    // wrapped sample entry surfaces as `EncryptedInput`, not as the misleading
    // `UnsupportedVideoCodec`. Unknown handlers are silently accepted with
    // zeroed audio fields — the caller rejects them via
    // `UnsupportedTrackLayout` once tracks are classified.
    let (sample_rate, channel_count) = if handler == HANDLER_VIDEO {
        verify_no_encryption(reader, &entry, /*audio:*/ false)?;
        if !VIDEO_FOURCCS.contains(&entry.box_type) {
            return Err(PackagerError::UnsupportedVideoCodec);
        }
        (0, 0)
    } else if handler == HANDLER_AUDIO {
        verify_no_encryption(reader, &entry, /*audio:*/ true)?;
        if !AUDIO_FOURCCS.contains(&entry.box_type) {
            return Err(PackagerError::UnsupportedAudioCodec);
        }
        parse_audio_sample_entry(reader, &entry)?
    } else {
        (0, 0)
    };

    Ok(SampleEntryInfo {
        fourcc: entry.box_type,
        sample_entry: ByteRange {
            offset: entry.start,
            len: entry.declared_size,
        },
        sample_rate,
        channel_count,
    })
}

/// VisualSampleEntry / AudioSampleEntry both start with 8-byte SampleEntry
/// header (6 reserved + 2 data_reference_index). Children begin after a
/// codec-class-specific fixed prefix: 78 B for visual, 28 B for audio v0.
fn parse_audio_sample_entry<R: ReadAt + ?Sized>(
    reader: &R,
    entry: &BoxHeader,
) -> Result<(u32, u8), PackagerError> {
    // SampleEntry(8) + audio-specific(20 for V0 layout, 36 for V1) before children.
    let payload_len = entry.payload_len();
    if payload_len < 28 {
        return Err(PackagerError::MalformedAtom {
            atom: "mp4a",
            reason: "AudioSampleEntry header truncated",
        });
    }
    let mut buf = [0u8; 28];
    read_exact(reader, entry.payload_offset, &mut buf)?;
    // buf[0..6] reserved, buf[6..8] data_reference_index.
    // buf[8..10] version, buf[10..12] revision, buf[12..16] vendor.
    // buf[16..18] channelcount, buf[18..20] samplesize,
    // buf[20..22] compression_id, buf[22..24] packet_size,
    // buf[24..28] sample_rate (16.16 fixed-point).
    let version = BigEndian::read_u16(&buf[8..10]);
    if version > 1 {
        return Err(PackagerError::MalformedAtom {
            atom: "mp4a",
            reason: "unsupported AudioSampleEntry version",
        });
    }
    let channel_count_u16 = BigEndian::read_u16(&buf[16..18]);
    let channel_count =
        u8::try_from(channel_count_u16).map_err(|_| PackagerError::MalformedAtom {
            atom: "mp4a",
            reason: "channel_count exceeds u8 range",
        })?;
    let sample_rate = BigEndian::read_u32(&buf[24..28]) >> 16;
    Ok((sample_rate, channel_count))
}

/// Walk the child boxes of a sample-entry; reject if any indicates encryption.
/// `sinf` wraps `frma`/`schm`/`schi/tenc` for ISO Common Encryption; the rest
/// are belt-and-suspenders matches for non-standard layouts that hoist an
/// encryption marker directly under the sample-entry.
fn verify_no_encryption<R: ReadAt + ?Sized>(
    reader: &R,
    entry: &BoxHeader,
    audio: bool,
) -> Result<(), PackagerError> {
    let prefix = if audio { 28 } else { 78 };
    if entry.payload_len() <= prefix {
        return Ok(());
    }
    let children_start = entry.payload_offset + prefix;
    let children_end = entry.end();
    let mut iter = BoxIter::new(reader, children_start, children_end);
    while let Some(child) = iter.next_header()? {
        if matches!(
            &child.box_type,
            b"sinf" | b"tenc" | b"senc" | b"sbgp" | b"sgpd"
        ) {
            return Err(PackagerError::EncryptedInput);
        }
    }
    Ok(())
}

fn payload_range(header: &BoxHeader) -> ByteRange {
    ByteRange {
        offset: header.payload_offset,
        len: header.payload_len(),
    }
}

/// Read the `grouping_type` of an `sbgp` / `sgpd` box. Rejects with
/// [`PackagerError::EncryptedInput`] only when the type is `seig` (ISO
/// Common Encryption); other types are silently ignored.
fn check_grouping_type<R: ReadAt + ?Sized>(
    reader: &R,
    header: &BoxHeader,
    atom: &'static str,
) -> Result<(), PackagerError> {
    if header.payload_len() < 8 {
        return Err(PackagerError::MalformedAtom {
            atom,
            reason: "payload truncated before grouping_type",
        });
    }
    let mut buf = [0u8; 4];
    read_exact(reader, header.payload_offset + 4, &mut buf)?;
    if &buf == b"seig" {
        return Err(PackagerError::EncryptedInput);
    }
    Ok(())
}

fn classify_tracks(tracks: Vec<UnclassifiedTrack>) -> Result<MoovParse, PackagerError> {
    let total = tracks.len();
    let mut video: Option<UnclassifiedTrack> = None;
    let mut audio: Option<UnclassifiedTrack> = None;
    let mut video_count: u32 = 0;
    let mut audio_count: u32 = 0;

    for trk in tracks {
        if trk.handler == HANDLER_VIDEO {
            video_count += 1;
            if video.is_none() {
                video = Some(trk);
            }
        } else if trk.handler == HANDLER_AUDIO {
            audio_count += 1;
            if audio.is_none() {
                audio = Some(trk);
            }
        }
    }

    match (video, audio) {
        (Some(v), Some(a)) if total == 2 && video_count == 1 && audio_count == 1 => Ok(MoovParse {
            video: into_parsed(v),
            audio: into_parsed(a),
        }),
        _ => Err(PackagerError::UnsupportedTrackLayout {
            video: video_count,
            audio: audio_count,
        }),
    }
}

fn into_parsed(t: UnclassifiedTrack) -> ParsedTrack {
    ParsedTrack {
        timescale: t.timescale,
        fourcc: t.fourcc,
        width: t.width,
        height: t.height,
        sample_rate: t.sample_rate,
        channel_count: t.channel_count,
        sample_entry: t.sample_entry,
        elst: t.elst,
        sample_table: t.sample_table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_at::SliceReader;

    fn make_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(total as usize);
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(fourcc);
        out.extend_from_slice(payload);
        out
    }

    fn make_full_box(fourcc: &[u8; 4], version: u8, flags: [u8; 3], body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(4 + body.len());
        payload.push(version);
        payload.extend_from_slice(&flags);
        payload.extend_from_slice(body);
        make_box(fourcc, &payload)
    }

    fn tkhd_v0(track_id: u32, width: u32, height: u32) -> Vec<u8> {
        let mut body = Vec::with_capacity(80);
        body.extend_from_slice(&[0u8; 8]); // ctime + mtime
        body.extend_from_slice(&track_id.to_be_bytes());
        body.extend_from_slice(&[0u8; 4]); // reserved
        body.extend_from_slice(&[0u8; 4]); // duration
        body.extend_from_slice(&[0u8; 8]); // reserved
        body.extend_from_slice(&[0u8; 2]); // layer
        body.extend_from_slice(&[0u8; 2]); // alternate_group
        body.extend_from_slice(&[0u8; 2]); // volume
        body.extend_from_slice(&[0u8; 2]); // reserved
        body.extend_from_slice(&[0u8; 36]); // matrix
        body.extend_from_slice(&(width << 16).to_be_bytes());
        body.extend_from_slice(&(height << 16).to_be_bytes());
        make_full_box(b"tkhd", 0, [0, 0, 0], &body)
    }

    fn mdhd_v0(timescale: u32) -> Vec<u8> {
        let mut body = Vec::with_capacity(20);
        body.extend_from_slice(&[0u8; 4]); // ctime
        body.extend_from_slice(&[0u8; 4]); // mtime
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&[0u8; 4]); // duration
        body.extend_from_slice(&[0u8; 4]); // language + pre_defined
        make_full_box(b"mdhd", 0, [0, 0, 0], &body)
    }

    fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
        let mut body = Vec::with_capacity(20);
        body.extend_from_slice(&[0u8; 4]); // pre_defined
        body.extend_from_slice(handler);
        body.extend_from_slice(&[0u8; 12]); // reserved
        body.push(0); // empty name
        make_full_box(b"hdlr", 0, [0, 0, 0], &body)
    }

    fn avc1_sample_entry() -> Vec<u8> {
        // SampleEntry(8) + VisualSampleEntry(70) = 78 bytes payload, no children.
        let mut body = Vec::with_capacity(78);
        body.extend_from_slice(&[0u8; 6]); // reserved
        body.extend_from_slice(&[0, 1]); // data_reference_index = 1
        body.extend_from_slice(&[0u8; 16]); // pre_defined + reserved
        body.extend_from_slice(&[0x07, 0x80]); // width = 1920
        body.extend_from_slice(&[0x04, 0x38]); // height = 1080
        body.extend_from_slice(&[0u8; 4]); // horizresolution
        body.extend_from_slice(&[0u8; 4]); // vertresolution
        body.extend_from_slice(&[0u8; 4]); // reserved
        body.extend_from_slice(&[0, 1]); // frame_count
        body.extend_from_slice(&[0u8; 32]); // compressorname
        body.extend_from_slice(&[0, 24]); // depth
        body.extend_from_slice(&[0xff, 0xff]); // pre_defined
        make_box(b"avc1", &body)
    }

    fn mp4a_sample_entry(sample_rate: u32, channels: u16) -> Vec<u8> {
        let mut body = Vec::with_capacity(28);
        body.extend_from_slice(&[0u8; 6]); // reserved
        body.extend_from_slice(&[0, 1]); // data_reference_index
        body.extend_from_slice(&[0, 0]); // version (V0)
        body.extend_from_slice(&[0, 0]); // revision_level
        body.extend_from_slice(&[0u8; 4]); // vendor
        body.extend_from_slice(&channels.to_be_bytes());
        body.extend_from_slice(&[0, 16]); // samplesize
        body.extend_from_slice(&[0, 0]); // compression_id
        body.extend_from_slice(&[0, 0]); // packet_size
        body.extend_from_slice(&(sample_rate << 16).to_be_bytes());
        make_box(b"mp4a", &body)
    }

    fn stsd(entry: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes()); // entry_count
        body.extend_from_slice(entry);
        make_full_box(b"stsd", 0, [0, 0, 0], &body)
    }

    fn empty_full(fourcc: &[u8; 4]) -> Vec<u8> {
        // Most of the sample-table full boxes are version+flags + 4-byte
        // entry_count + entries. Empty entries → entry_count = 0.
        make_full_box(fourcc, 0, [0, 0, 0], &0u32.to_be_bytes())
    }

    fn stsz_empty() -> Vec<u8> {
        // version+flags + sample_size + sample_count.
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 (per-sample table follows)
        body.extend_from_slice(&0u32.to_be_bytes()); // sample_count
        make_full_box(b"stsz", 0, [0, 0, 0], &body)
    }

    fn stbl(sample_entry_box: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&stsd(sample_entry_box));
        payload.extend_from_slice(&empty_full(b"stts"));
        payload.extend_from_slice(&empty_full(b"stsc"));
        payload.extend_from_slice(&stsz_empty());
        payload.extend_from_slice(&empty_full(b"stco"));
        make_box(b"stbl", &payload)
    }

    fn minf(stbl_box: &[u8]) -> Vec<u8> {
        make_box(b"minf", stbl_box)
    }

    fn mdia(timescale: u32, handler: &[u8; 4], stbl_box: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&mdhd_v0(timescale));
        payload.extend_from_slice(&hdlr(handler));
        payload.extend_from_slice(&minf(stbl_box));
        make_box(b"mdia", &payload)
    }

    fn trak_video() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(1, 1920, 1080));
        payload.extend_from_slice(&mdia(90_000, b"vide", &stbl(&avc1_sample_entry())));
        make_box(b"trak", &payload)
    }

    fn trak_audio() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(2, 0, 0));
        payload.extend_from_slice(&mdia(48_000, b"soun", &stbl(&mp4a_sample_entry(48_000, 2))));
        make_box(b"trak", &payload)
    }

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(major);
        body.extend_from_slice(&0u32.to_be_bytes()); // minor
        for b in compat {
            body.extend_from_slice(*b);
        }
        make_box(b"ftyp", &body)
    }

    fn make_minimal_mp4() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[b"isom", b"mp42"]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&trak_video());
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));
        bytes
    }

    #[test]
    fn parses_minimal_dual_track_mp4() {
        let bytes = make_minimal_mp4();
        let reader = SliceReader(&bytes);
        let parse = parse_top_level(&reader, bytes.len() as u64).unwrap();

        assert_eq!(parse.video.timescale, 90_000);
        assert_eq!(parse.video.fourcc, *b"avc1");
        assert_eq!(parse.video.width, 1920);
        assert_eq!(parse.video.height, 1080);
        assert!(parse.video.elst.is_none());

        assert_eq!(parse.audio.timescale, 48_000);
        assert_eq!(parse.audio.fourcc, *b"mp4a");
        assert_eq!(parse.audio.sample_rate, 48_000);
        assert_eq!(parse.audio.channel_count, 2);

        // Sample-entry byte range: read it back and confirm it equals the
        // exact `avc1` box bytes we synthesized.
        let mut buf = vec![0u8; parse.video.sample_entry.len as usize];
        crate::demux::reader::read_exact(&reader, parse.video.sample_entry.offset, &mut buf)
            .unwrap();
        assert_eq!(buf, avc1_sample_entry());
    }

    #[test]
    fn rejects_unknown_brand() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"qqqq", &[b"qqqq"]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&trak_video());
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, PackagerError::UnsupportedBrand));
    }

    #[test]
    fn rejects_top_level_moof() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        bytes.extend_from_slice(&make_box(b"moof", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&trak_video());
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, PackagerError::FragmentedInput));
    }

    #[test]
    fn rejects_video_only() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        bytes.extend_from_slice(&make_box(b"moov", &trak_video()));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::UnsupportedTrackLayout { video: 1, audio: 0 }
        ));
    }

    #[test]
    fn rejects_unsupported_video_codec() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(1, 1920, 1080));
        // Replace avc1 with vp09 (not in allow-list).
        let mut entry = avc1_sample_entry();
        entry[4..8].copy_from_slice(b"vp09");
        payload.extend_from_slice(&mdia(90_000, b"vide", &stbl(&entry)));
        let video = make_box(b"trak", &payload);
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&video);
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, PackagerError::UnsupportedVideoCodec));
    }

    #[test]
    fn captures_edts_passthrough() {
        // Wrap the video trak with an `edts` box and verify the byte range.
        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(1, 1920, 1080));
        let edts_payload = b"\x00\x00\x00\x10elst\x00\x00\x00\x00\x00\x00\x00\x00";
        let edts = make_box(b"edts", edts_payload);
        payload.extend_from_slice(&edts);
        payload.extend_from_slice(&mdia(90_000, b"vide", &stbl(&avc1_sample_entry())));
        let video = make_box(b"trak", &payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&video);
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let parse = parse_top_level(&reader, bytes.len() as u64).unwrap();
        let elst = parse.video.elst.expect("edts captured");
        let mut buf = vec![0u8; elst.len as usize];
        crate::demux::reader::read_exact(&reader, elst.offset, &mut buf).unwrap();
        assert_eq!(buf, edts);
    }

    #[test]
    fn rejects_encryption_marker() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        bytes.extend_from_slice(&make_box(b"sinf", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&trak_video());
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, PackagerError::EncryptedInput));
    }

    #[test]
    fn rejects_extra_track_even_when_av_pair_present() {
        // 1 video + 1 audio + 1 hint trak — total != 2.
        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(3, 0, 0));
        payload.extend_from_slice(&mdia(90_000, b"hint", &stbl(&mp4a_sample_entry(48_000, 2))));
        let hint = make_box(b"trak", &payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&trak_video());
        moov_payload.extend_from_slice(&trak_audio());
        moov_payload.extend_from_slice(&hint);
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::UnsupportedTrackLayout { video: 1, audio: 1 }
        ));
    }

    #[test]
    fn encrypted_sample_entry_reports_encrypted_input() {
        // `encv` is the standard "encrypted video" sample-entry fourcc; it
        // wraps a `sinf` child carrying the original codec scheme. Without
        // the verify_no_encryption check ordering, this would surface as
        // UnsupportedVideoCodec, which is misleading.
        let mut entry_body = Vec::new();
        entry_body.extend_from_slice(&[0u8; 6]); // reserved
        entry_body.extend_from_slice(&[0, 1]); // data_reference_index
        entry_body.extend_from_slice(&[0u8; 70]); // VisualSampleEntry rest
        entry_body.extend_from_slice(&make_box(b"sinf", b"")); // encryption marker
        let entry = make_box(b"encv", &entry_body);

        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(1, 1920, 1080));
        payload.extend_from_slice(&mdia(90_000, b"vide", &stbl(&entry)));
        let video = make_box(b"trak", &payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&video);
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(err, PackagerError::EncryptedInput));
    }

    #[test]
    fn stsd_entry_count_above_one_rejected() {
        // Two sample-entry boxes inside stsd — still wrapped in a real moov
        // so the malformed atom is the only differentiator.
        let mut stsd_body = Vec::new();
        stsd_body.extend_from_slice(&2u32.to_be_bytes()); // entry_count
        stsd_body.extend_from_slice(&avc1_sample_entry());
        stsd_body.extend_from_slice(&avc1_sample_entry());
        let stsd = make_full_box(b"stsd", 0, [0, 0, 0], &stsd_body);

        let mut stbl_payload = Vec::new();
        stbl_payload.extend_from_slice(&stsd);
        stbl_payload.extend_from_slice(&empty_full(b"stts"));
        stbl_payload.extend_from_slice(&empty_full(b"stsc"));
        stbl_payload.extend_from_slice(&stsz_empty());
        stbl_payload.extend_from_slice(&empty_full(b"stco"));
        let stbl_box = make_box(b"stbl", &stbl_payload);

        let mut payload = Vec::new();
        payload.extend_from_slice(&tkhd_v0(1, 1920, 1080));
        payload.extend_from_slice(&mdia(90_000, b"vide", &stbl_box));
        let video = make_box(b"trak", &payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ftyp(b"mp42", &[]));
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&video);
        moov_payload.extend_from_slice(&trak_audio());
        bytes.extend_from_slice(&make_box(b"moov", &moov_payload));

        let reader = SliceReader(&bytes);
        let err = parse_top_level(&reader, bytes.len() as u64).unwrap_err();
        assert!(matches!(
            err,
            PackagerError::MalformedAtom { atom: "stsd", .. }
        ));
    }
}
