//! CMAF init-segment writer.
//!
//! Emits `ftyp` followed by `moov` containing `mvhd`, two `trak` boxes, and
//! `mvex/trex` defaults. Sample-entry and `edts` byte-blobs from
//! [`crate::IndexView`] flow through verbatim — this writer never
//! reconstructs codec config from parsed fields.
//!
//! Track layout is fixed: `track_id = 1` is video, `track_id = 2` is audio.
//! Both [`super::media`] and `mvex/trex` here rely on this assignment.

use std::io::{self, Seek, Write};

use byteorder::{BigEndian, WriteBytesExt};

use crate::error::PackagerError;
use crate::index::view::{AudioTrackMeta, IndexView, VideoTrackMeta};

use super::boxes::{BoxWriter, write_full_box_header};

const MOVIE_TIMESCALE: u32 = 1000;
const VIDEO_TRACK_ID: u32 = 1;
const AUDIO_TRACK_ID: u32 = 2;
const NEXT_TRACK_ID: u32 = 3;

// `trex.default_sample_flags` bit layout (big-endian, byte 0 high):
//   byte 0: reserved(4) | is_leading(2) | sample_depends_on(2)
//   byte 1: sample_is_depended_on(2) | sample_has_redundancy(2) |
//           sample_padding_value(3) | sample_is_non_sync_sample(1)
//   byte 2-3: sample_degradation_priority(16)
//
// Video default (non-sync): sample_depends_on=1 (depends on others),
// sample_is_non_sync_sample=1 → 0x01_01_00_00.
// Audio default (sync): sample_depends_on=2 (independent),
// sample_is_non_sync_sample=0 → 0x02_00_00_00.
const VIDEO_DEFAULT_SAMPLE_FLAGS: u32 = 0x0101_0000;
const AUDIO_DEFAULT_SAMPLE_FLAGS: u32 = 0x0200_0000;

// Identity 3×3 transform in the 16.16 / 2.30 fixed-point matrix encoding.
#[rustfmt::skip]
const IDENTITY_MATRIX: [u32; 9] = [
    0x0001_0000, 0,           0,
    0,           0x0001_0000, 0,
    0,           0,           0x4000_0000,
];

// `tkhd` flags: track_enabled | track_in_movie | track_in_preview = 0x07.
const TKHD_FLAGS_ENABLED: u32 = 0x0000_0007;

// 'und' (undetermined) ISO-639-2/T language packed into 15 bits with 1
// leading pad bit: each letter = (ascii - 0x60), 5 bits each.
const LANGUAGE_UND: u16 = 0x55C4;

/// Write the CMAF init segment (`ftyp` + `moov`) for `index` into `out`.
///
/// `IndexBuilder::build` calls this once and embeds the result into the
/// `.idx` `InitSegmentBytes` section; `cmafly-serve` later forwards those
/// bytes verbatim.
pub fn write_init_segment<W: Write + Seek>(
    index: &IndexView<'_>,
    out: &mut W,
) -> Result<(), PackagerError> {
    write_ftyp(out)?;
    write_moov(index, out)?;
    Ok(())
}

fn write_ftyp<W: Write + Seek>(out: &mut W) -> io::Result<()> {
    let mut bx = BoxWriter::open(out, *b"ftyp")?;
    let w = bx.writer();
    w.write_all(b"cmfc")?;
    w.write_u32::<BigEndian>(0)?;
    w.write_all(b"iso6")?;
    w.write_all(b"cmfc")?;
    bx.finish()?;
    Ok(())
}

fn write_moov<W: Write + Seek>(index: &IndexView<'_>, out: &mut W) -> io::Result<()> {
    let mut moov = BoxWriter::open(out, *b"moov")?;
    write_mvhd(&mut moov)?;
    write_video_trak(&mut moov, &index.video_track())?;
    write_audio_trak(&mut moov, &index.audio_track())?;
    write_mvex(&mut moov)?;
    moov.finish()?;
    Ok(())
}

fn write_mvhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut bx = parent.child(*b"mvhd")?;
    let w = bx.writer();
    write_full_box_header(w, 1, 0)?;
    w.write_u64::<BigEndian>(0)?; // creation_time
    w.write_u64::<BigEndian>(0)?; // modification_time
    w.write_u32::<BigEndian>(MOVIE_TIMESCALE)?;
    w.write_u64::<BigEndian>(0)?; // duration (CMAF: declared via mvex/trex)
    w.write_u32::<BigEndian>(0x0001_0000)?; // rate = 1.0
    w.write_u16::<BigEndian>(0x0100)?; // volume = 1.0
    w.write_u16::<BigEndian>(0)?; // reserved
    w.write_u32::<BigEndian>(0)?; // reserved
    w.write_u32::<BigEndian>(0)?; // reserved
    write_matrix(w)?;
    for _ in 0..6 {
        w.write_u32::<BigEndian>(0)?; // pre_defined
    }
    w.write_u32::<BigEndian>(NEXT_TRACK_ID)?;
    bx.finish()?;
    Ok(())
}

fn write_video_trak<W: Write + Seek>(
    moov: &mut BoxWriter<'_, W>,
    meta: &VideoTrackMeta<'_>,
) -> io::Result<()> {
    let mut trak = moov.child(*b"trak")?;
    write_tkhd(
        &mut trak,
        VIDEO_TRACK_ID,
        /* is_audio: */ false,
        meta.width,
        meta.height,
    )?;
    if !meta.elst.is_empty() {
        // `elst` field carries the entire `edts` box, header included.
        // Emit byte-for-byte.
        trak.writer().write_all(meta.elst)?;
    }
    write_video_mdia(&mut trak, meta)?;
    trak.finish()?;
    Ok(())
}

fn write_audio_trak<W: Write + Seek>(
    moov: &mut BoxWriter<'_, W>,
    meta: &AudioTrackMeta<'_>,
) -> io::Result<()> {
    let mut trak = moov.child(*b"trak")?;
    write_tkhd(
        &mut trak,
        AUDIO_TRACK_ID,
        /* is_audio: */ true,
        /* width: */ 0,
        /* height: */ 0,
    )?;
    if !meta.elst.is_empty() {
        trak.writer().write_all(meta.elst)?;
    }
    write_audio_mdia(&mut trak, meta)?;
    trak.finish()?;
    Ok(())
}

fn write_tkhd<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    track_id: u32,
    is_audio: bool,
    width: u32,
    height: u32,
) -> io::Result<()> {
    let mut bx = parent.child(*b"tkhd")?;
    let w = bx.writer();
    write_full_box_header(w, 1, TKHD_FLAGS_ENABLED)?;
    w.write_u64::<BigEndian>(0)?; // creation_time
    w.write_u64::<BigEndian>(0)?; // modification_time
    w.write_u32::<BigEndian>(track_id)?;
    w.write_u32::<BigEndian>(0)?; // reserved
    w.write_u64::<BigEndian>(0)?; // duration
    w.write_u32::<BigEndian>(0)?; // reserved
    w.write_u32::<BigEndian>(0)?; // reserved
    w.write_u16::<BigEndian>(0)?; // layer
    w.write_u16::<BigEndian>(0)?; // alternate_group
    let volume: u16 = if is_audio { 0x0100 } else { 0 };
    w.write_u16::<BigEndian>(volume)?;
    w.write_u16::<BigEndian>(0)?; // reserved
    write_matrix(w)?;
    w.write_u32::<BigEndian>(fixed_16_16(width))?;
    w.write_u32::<BigEndian>(fixed_16_16(height))?;
    bx.finish()?;
    Ok(())
}

fn write_video_mdia<W: Write + Seek>(
    trak: &mut BoxWriter<'_, W>,
    meta: &VideoTrackMeta<'_>,
) -> io::Result<()> {
    let mut mdia = trak.child(*b"mdia")?;
    write_mdhd(&mut mdia, meta.timescale)?;
    write_hdlr(&mut mdia, *b"vide", b"VideoHandler")?;
    write_video_minf(&mut mdia, meta)?;
    mdia.finish()?;
    Ok(())
}

fn write_audio_mdia<W: Write + Seek>(
    trak: &mut BoxWriter<'_, W>,
    meta: &AudioTrackMeta<'_>,
) -> io::Result<()> {
    let mut mdia = trak.child(*b"mdia")?;
    write_mdhd(&mut mdia, meta.timescale)?;
    write_hdlr(&mut mdia, *b"soun", b"SoundHandler")?;
    write_audio_minf(&mut mdia, meta)?;
    mdia.finish()?;
    Ok(())
}

fn write_mdhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>, timescale: u32) -> io::Result<()> {
    let mut bx = parent.child(*b"mdhd")?;
    let w = bx.writer();
    write_full_box_header(w, 1, 0)?;
    w.write_u64::<BigEndian>(0)?; // creation_time
    w.write_u64::<BigEndian>(0)?; // modification_time
    w.write_u32::<BigEndian>(timescale)?;
    w.write_u64::<BigEndian>(0)?; // duration
    w.write_u16::<BigEndian>(LANGUAGE_UND)?;
    w.write_u16::<BigEndian>(0)?; // pre_defined
    bx.finish()?;
    Ok(())
}

fn write_hdlr<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    handler_type: [u8; 4],
    name: &[u8],
) -> io::Result<()> {
    let mut bx = parent.child(*b"hdlr")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 0)?;
    w.write_u32::<BigEndian>(0)?; // pre_defined
    w.write_all(&handler_type)?;
    w.write_u32::<BigEndian>(0)?;
    w.write_u32::<BigEndian>(0)?;
    w.write_u32::<BigEndian>(0)?;
    w.write_all(name)?;
    w.write_u8(0)?; // null-terminator
    bx.finish()?;
    Ok(())
}

fn write_video_minf<W: Write + Seek>(
    mdia: &mut BoxWriter<'_, W>,
    meta: &VideoTrackMeta<'_>,
) -> io::Result<()> {
    let mut minf = mdia.child(*b"minf")?;
    write_vmhd(&mut minf)?;
    write_dinf(&mut minf)?;
    write_stbl(&mut minf, meta.sample_entry)?;
    minf.finish()?;
    Ok(())
}

fn write_audio_minf<W: Write + Seek>(
    mdia: &mut BoxWriter<'_, W>,
    meta: &AudioTrackMeta<'_>,
) -> io::Result<()> {
    let mut minf = mdia.child(*b"minf")?;
    write_smhd(&mut minf)?;
    write_dinf(&mut minf)?;
    write_stbl(&mut minf, meta.sample_entry)?;
    minf.finish()?;
    Ok(())
}

fn write_vmhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut bx = parent.child(*b"vmhd")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 1)?; // flags=1: graphicsmode/opcolor present
    w.write_u16::<BigEndian>(0)?; // graphicsmode
    w.write_u16::<BigEndian>(0)?; // opcolor R
    w.write_u16::<BigEndian>(0)?; // opcolor G
    w.write_u16::<BigEndian>(0)?; // opcolor B
    bx.finish()?;
    Ok(())
}

fn write_smhd<W: Write + Seek>(parent: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut bx = parent.child(*b"smhd")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 0)?;
    w.write_u16::<BigEndian>(0)?; // balance
    w.write_u16::<BigEndian>(0)?; // reserved
    bx.finish()?;
    Ok(())
}

fn write_dinf<W: Write + Seek>(parent: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut dinf = parent.child(*b"dinf")?;
    {
        let mut dref = dinf.child(*b"dref")?;
        write_full_box_header(dref.writer(), 0, 0)?;
        dref.writer().write_u32::<BigEndian>(1)?; // entry_count
        {
            // `url ` with self-contained flag (0x000001) and no location.
            let mut url = dref.child(*b"url ")?;
            write_full_box_header(url.writer(), 0, 0x0000_0001)?;
            url.finish()?;
        }
        dref.finish()?;
    }
    dinf.finish()?;
    Ok(())
}

fn write_stbl<W: Write + Seek>(minf: &mut BoxWriter<'_, W>, sample_entry: &[u8]) -> io::Result<()> {
    let mut stbl = minf.child(*b"stbl")?;
    write_stsd(&mut stbl, sample_entry)?;
    write_empty_entry_count_box(&mut stbl, *b"stts")?;
    write_empty_entry_count_box(&mut stbl, *b"stsc")?;
    write_empty_stsz(&mut stbl)?;
    write_empty_entry_count_box(&mut stbl, *b"stco")?;
    stbl.finish()?;
    Ok(())
}

fn write_stsd<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    sample_entry: &[u8],
) -> io::Result<()> {
    let mut bx = parent.child(*b"stsd")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 0)?;
    w.write_u32::<BigEndian>(1)?; // entry_count
    w.write_all(sample_entry)?; // verbatim avc1 / hvc1 / mp4a box
    bx.finish()?;
    Ok(())
}

/// Emit a full-box whose only payload is `entry_count = 0`. Used for the
/// `stts` / `stsc` / `stco` placeholders that CMAF init segments carry — the
/// real per-sample tables live in each media segment's `trun`.
fn write_empty_entry_count_box<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    fourcc: [u8; 4],
) -> io::Result<()> {
    let mut bx = parent.child(fourcc)?;
    write_full_box_header(bx.writer(), 0, 0)?;
    bx.writer().write_u32::<BigEndian>(0)?; // entry_count
    bx.finish()?;
    Ok(())
}

fn write_empty_stsz<W: Write + Seek>(parent: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut bx = parent.child(*b"stsz")?;
    write_full_box_header(bx.writer(), 0, 0)?;
    bx.writer().write_u32::<BigEndian>(0)?; // sample_size = 0 (variable)
    bx.writer().write_u32::<BigEndian>(0)?; // sample_count
    bx.finish()?;
    Ok(())
}

fn write_mvex<W: Write + Seek>(moov: &mut BoxWriter<'_, W>) -> io::Result<()> {
    let mut mvex = moov.child(*b"mvex")?;
    write_trex(&mut mvex, VIDEO_TRACK_ID, VIDEO_DEFAULT_SAMPLE_FLAGS)?;
    write_trex(&mut mvex, AUDIO_TRACK_ID, AUDIO_DEFAULT_SAMPLE_FLAGS)?;
    mvex.finish()?;
    Ok(())
}

fn write_trex<W: Write + Seek>(
    parent: &mut BoxWriter<'_, W>,
    track_id: u32,
    default_sample_flags: u32,
) -> io::Result<()> {
    let mut bx = parent.child(*b"trex")?;
    let w = bx.writer();
    write_full_box_header(w, 0, 0)?;
    w.write_u32::<BigEndian>(track_id)?;
    w.write_u32::<BigEndian>(1)?; // default_sample_description_index
    w.write_u32::<BigEndian>(0)?; // default_sample_duration
    w.write_u32::<BigEndian>(0)?; // default_sample_size
    w.write_u32::<BigEndian>(default_sample_flags)?;
    bx.finish()?;
    Ok(())
}

fn write_matrix<W: Write>(out: &mut W) -> io::Result<()> {
    for v in IDENTITY_MATRIX {
        out.write_u32::<BigEndian>(v)?;
    }
    Ok(())
}

fn fixed_16_16(value: u32) -> u32 {
    // 16.16 fixed-point: pixel count in the high 16 bits.
    // For real video dimensions (< 65 536) this is exact; saturate
    // pathologically large inputs at u32::MAX rather than panicking on
    // overflow.
    let widened = u64::from(value) << 16;
    u32::try_from(widened).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use byteorder::{BigEndian, ByteOrder};

    use super::*;
    use crate::demux::reader::BoxIter;
    use crate::index::format::{SampleEntry, SegmentEntry};

    /// Construct a minimal AVC1 visual sample-entry box. Layout follows
    /// ISO/IEC 14496-12 VisualSampleEntry: 8 B SampleEntry header + 70 B
    /// VisualSampleEntry fixed prefix + an `avcC` child. The exact bytes
    /// don't matter for the writer — we only verify they survive
    /// verbatim from `IndexView::video_track().sample_entry`.
    /// Wrap `payload` in `[size:u32 BE | fourcc:4 | payload…]` and patch the
    /// size header.
    fn make_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bx = Vec::with_capacity(8 + payload.len());
        bx.extend_from_slice(&[0u8; 8]);
        bx.extend_from_slice(payload);
        let size = bx.len() as u32;
        BigEndian::write_u32(&mut bx[..4], size);
        bx[4..8].copy_from_slice(fourcc);
        bx
    }

    fn fake_avc1() -> Vec<u8> {
        let avcc = make_box(
            b"avcC",
            &[0x01, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0x00, 0x00, 0x01, 0x00],
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0u8; 6]); // SampleEntry reserved
        payload.extend_from_slice(&[0x00, 0x01]); // data_reference_index
        payload.extend_from_slice(&[0u8; 16]); // pre_defined / reserved
        payload.extend_from_slice(&[0x07, 0x80]); // width = 1920
        payload.extend_from_slice(&[0x04, 0x38]); // height = 1080
        payload.extend_from_slice(&[0x00, 0x48, 0x00, 0x00]); // h-resolution
        payload.extend_from_slice(&[0x00, 0x48, 0x00, 0x00]); // v-resolution
        payload.extend_from_slice(&[0u8; 4]); // reserved
        payload.extend_from_slice(&[0x00, 0x01]); // frame_count
        payload.extend_from_slice(&[0u8; 32]); // compressorname
        payload.extend_from_slice(&[0x00, 0x18]); // depth = 24
        payload.extend_from_slice(&[0xFF, 0xFF]); // pre_defined
        payload.extend_from_slice(&avcc);
        make_box(b"avc1", &payload)
    }

    fn fake_mp4a() -> Vec<u8> {
        let esds = {
            let mut p = Vec::new();
            p.extend_from_slice(&[0u8; 4]); // version+flags
            p.extend_from_slice(&[0x03, 0x05, 0x00, 0x01, 0x00]); // minimal ES_Descriptor stub
            make_box(b"esds", &p)
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0u8; 6]); // SampleEntry reserved
        payload.extend_from_slice(&[0x00, 0x01]); // data_reference_index
        payload.extend_from_slice(&[0u8; 8]); // reserved (audio)
        payload.extend_from_slice(&[0x00, 0x02]); // channel_count = 2
        payload.extend_from_slice(&[0x00, 0x10]); // sample_size = 16
        payload.extend_from_slice(&[0u8; 4]); // pre_defined / reserved
        payload.extend_from_slice(&[0xBB, 0x80, 0x00, 0x00]); // sample_rate = 48000<<16
        payload.extend_from_slice(&esds);
        make_box(b"mp4a", &payload)
    }

    /// Hand-crafted `edts/elst` box to verify verbatim passthrough.
    fn fake_edts() -> Vec<u8> {
        let elst = {
            let mut p = Vec::new();
            p.extend_from_slice(&[0u8; 4]); // version+flags
            p.extend_from_slice(&[0, 0, 0, 1]); // entry_count
            p.extend_from_slice(&[0, 0, 0, 100]); // segment_duration
            p.extend_from_slice(&[0, 0, 0, 50]); // media_time
            p.extend_from_slice(&[0, 1, 0, 0]); // media_rate (1.0)
            make_box(b"elst", &p)
        };
        make_box(b"edts", &elst)
    }

    fn build_view<'a>(
        avc1: &'a [u8],
        mp4a: &'a [u8],
        video_elst: &'a [u8],
        audio_elst: &'a [u8],
        blake3: &'a [u8; 32],
    ) -> IndexView<'a> {
        let video = VideoTrackMeta {
            timescale: 90_000,
            fourcc: *b"avc1",
            width: 1920,
            height: 1080,
            sample_entry: avc1,
            elst: video_elst,
        };
        let audio = AudioTrackMeta {
            timescale: 48_000,
            fourcc: *b"mp4a",
            sample_rate: 48_000,
            channel_count: 2,
            sample_entry: mp4a,
            elst: audio_elst,
        };
        IndexView::from_parts(
            0,
            0,
            blake3,
            video,
            audio,
            &[] as &[SampleEntry],
            &[] as &[SampleEntry],
            &[] as &[SegmentEntry],
            &[],
            None,
        )
    }

    #[test]
    fn init_segment_top_level_is_ftyp_then_moov() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let reader = crate::read_at::SliceReader(&buf);
        let mut iter = BoxIter::new(&reader, 0, buf.len() as u64);
        let first = iter.next_header().unwrap().expect("ftyp");
        assert_eq!(&first.box_type, b"ftyp");
        let second = iter.next_header().unwrap().expect("moov");
        assert_eq!(&second.box_type, b"moov");
        assert!(
            iter.next_header().unwrap().is_none(),
            "init has only two boxes"
        );
        assert_eq!(first.start + first.declared_size, second.start);
        assert_eq!(second.end(), buf.len() as u64);
    }

    #[test]
    fn ftyp_carries_cmfc_and_iso6() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        // ftyp = size(4) + 'ftyp'(4) + 'cmfc'(4) + minor(4) + brands [iso6, cmfc] = 24 B
        assert_eq!(BigEndian::read_u32(&buf[0..4]), 24);
        assert_eq!(&buf[4..8], b"ftyp");
        assert_eq!(&buf[8..12], b"cmfc");
        assert_eq!(BigEndian::read_u32(&buf[12..16]), 0);
        assert_eq!(&buf[16..20], b"iso6");
        assert_eq!(&buf[20..24], b"cmfc");
    }

    #[test]
    fn moov_contains_mvhd_two_traks_and_mvex_in_order() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let reader = crate::read_at::SliceReader(&buf);
        let mut top = BoxIter::new(&reader, 0, buf.len() as u64);
        let _ftyp = top.next_header().unwrap().unwrap();
        let moov = top.next_header().unwrap().unwrap();

        let mut child = BoxIter::new(&reader, moov.payload_offset, moov.end());
        let names: Vec<[u8; 4]> = std::iter::from_fn(|| child.next_header().unwrap())
            .map(|h| h.box_type)
            .collect();
        assert_eq!(
            names,
            vec![*b"mvhd", *b"trak", *b"trak", *b"mvex"],
            "moov children in CMAF order",
        );
    }

    #[test]
    fn video_trak_carries_track_id_one_and_video_handler() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let trak = locate_trak(&buf, 0);
        let tkhd = locate_child(&buf, &trak, *b"tkhd");
        // tkhd v1 layout: 4 (ver+flags) + 8+8 (times) + 4 (track_id)
        let track_id = BigEndian::read_u32(
            &buf[(tkhd.payload_offset + 4 + 8 + 8) as usize
                ..(tkhd.payload_offset + 4 + 8 + 8 + 4) as usize],
        );
        assert_eq!(track_id, VIDEO_TRACK_ID);

        let mdia = locate_child(&buf, &trak, *b"mdia");
        let hdlr = locate_child(&buf, &mdia, *b"hdlr");
        // hdlr layout: 4 (ver+flags) + 4 (pre_defined) + 4 (handler_type)
        let handler_type =
            &buf[(hdlr.payload_offset + 8) as usize..(hdlr.payload_offset + 12) as usize];
        assert_eq!(handler_type, b"vide");
    }

    #[test]
    fn audio_trak_carries_track_id_two_and_sound_handler() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let trak = locate_trak(&buf, 1);
        let tkhd = locate_child(&buf, &trak, *b"tkhd");
        let track_id = BigEndian::read_u32(
            &buf[(tkhd.payload_offset + 4 + 8 + 8) as usize
                ..(tkhd.payload_offset + 4 + 8 + 8 + 4) as usize],
        );
        assert_eq!(track_id, AUDIO_TRACK_ID);

        let mdia = locate_child(&buf, &trak, *b"mdia");
        let hdlr = locate_child(&buf, &mdia, *b"hdlr");
        let handler_type =
            &buf[(hdlr.payload_offset + 8) as usize..(hdlr.payload_offset + 12) as usize];
        assert_eq!(handler_type, b"soun");
    }

    #[test]
    fn sample_entry_passes_through_byte_for_byte() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        for (track_idx, expected) in [(0usize, &avc1[..]), (1, &mp4a[..])] {
            let trak = locate_trak(&buf, track_idx);
            let mdia = locate_child(&buf, &trak, *b"mdia");
            let minf = locate_child(&buf, &mdia, *b"minf");
            let stbl = locate_child(&buf, &minf, *b"stbl");
            let stsd = locate_child(&buf, &stbl, *b"stsd");
            // stsd payload: 4 (ver+flags) + 4 (entry_count) + sample_entry
            let entry_start = (stsd.payload_offset + 8) as usize;
            let entry_end = entry_start + expected.len();
            assert_eq!(&buf[entry_start..entry_end], expected);
        }
    }

    #[test]
    fn elst_passes_through_byte_for_byte_when_present() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let edts = fake_edts();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &edts, &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        // The video trak's child sequence is `tkhd`, then `edts` (verbatim),
        // then `mdia`. Locate the bytes for the verbatim `edts` block.
        let trak = locate_trak(&buf, 0);
        let edts_box = locate_child(&buf, &trak, *b"edts");
        let edts_start = edts_box.start as usize;
        let edts_end = edts_box.end() as usize;
        assert_eq!(&buf[edts_start..edts_end], edts.as_slice());
    }

    #[test]
    fn elst_is_omitted_when_input_lacked_one() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let trak = locate_trak(&buf, 0);
        let reader = crate::read_at::SliceReader(&buf);
        let mut iter = BoxIter::new(&reader, trak.payload_offset, trak.end());
        let names: Vec<[u8; 4]> = std::iter::from_fn(|| iter.next_header().unwrap())
            .map(|h| h.box_type)
            .collect();
        assert_eq!(names, vec![*b"tkhd", *b"mdia"]);
    }

    #[test]
    fn mvex_emits_video_then_audio_trex_with_default_flags() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let reader = crate::read_at::SliceReader(&buf);
        let mut top = BoxIter::new(&reader, 0, buf.len() as u64);
        let _ftyp = top.next_header().unwrap().unwrap();
        let moov = top.next_header().unwrap().unwrap();
        let mvex = locate_child(&buf, &moov, *b"mvex");

        let mut iter = BoxIter::new(&reader, mvex.payload_offset, mvex.end());
        let trex_video = iter.next_header().unwrap().expect("trex video");
        assert_eq!(&trex_video.box_type, b"trex");
        // payload: 4 (ver+flags) + 4 (track_id) + 4 (idx) + 4 (dur) + 4 (size)
        // + 4 (default_sample_flags)
        let p = trex_video.payload_offset as usize;
        assert_eq!(BigEndian::read_u32(&buf[p + 4..p + 8]), VIDEO_TRACK_ID);
        assert_eq!(
            BigEndian::read_u32(&buf[p + 20..p + 24]),
            VIDEO_DEFAULT_SAMPLE_FLAGS,
        );

        let trex_audio = iter.next_header().unwrap().expect("trex audio");
        let p = trex_audio.payload_offset as usize;
        assert_eq!(BigEndian::read_u32(&buf[p + 4..p + 8]), AUDIO_TRACK_ID);
        assert_eq!(
            BigEndian::read_u32(&buf[p + 20..p + 24]),
            AUDIO_DEFAULT_SAMPLE_FLAGS,
        );
        assert!(iter.next_header().unwrap().is_none(), "mvex has two trex");
    }

    #[test]
    fn mvhd_uses_movie_timescale_and_zero_durations() {
        let avc1 = fake_avc1();
        let mp4a = fake_mp4a();
        let blake3 = [0u8; 32];
        let view = build_view(&avc1, &mp4a, &[], &[], &blake3);

        let mut buf: Vec<u8> = Vec::new();
        write_init_segment(&view, &mut Cursor::new(&mut buf)).expect("write");

        let reader = crate::read_at::SliceReader(&buf);
        let mut top = BoxIter::new(&reader, 0, buf.len() as u64);
        let _ftyp = top.next_header().unwrap().unwrap();
        let moov = top.next_header().unwrap().unwrap();
        let mvhd = locate_child(&buf, &moov, *b"mvhd");
        let p = mvhd.payload_offset as usize;
        // v1 mvhd: 4 (ver+flags) + 8+8 (times) + 4 (timescale) + 8 (duration)
        let timescale = BigEndian::read_u32(&buf[p + 20..p + 24]);
        let duration = BigEndian::read_u64(&buf[p + 24..p + 32]);
        assert_eq!(timescale, MOVIE_TIMESCALE);
        assert_eq!(duration, 0);

        // mvhd v1 closes with `next_track_id` at the very end of the box.
        let nti_offset = mvhd.end() as usize - 4;
        assert_eq!(
            BigEndian::read_u32(&buf[nti_offset..mvhd.end() as usize]),
            NEXT_TRACK_ID
        );
    }

    /// Locate the `index`-th `trak` (0 = video, 1 = audio) inside the moov.
    fn locate_trak(buf: &[u8], index: usize) -> crate::demux::reader::BoxHeader {
        let reader = crate::read_at::SliceReader(buf);
        let mut top = BoxIter::new(&reader, 0, buf.len() as u64);
        let _ftyp = top.next_header().unwrap().unwrap();
        let moov = top.next_header().unwrap().unwrap();
        let mut child = BoxIter::new(&reader, moov.payload_offset, moov.end());
        let mut found = None;
        let mut count = 0;
        while let Some(h) = child.next_header().unwrap() {
            if &h.box_type == b"trak" {
                if count == index {
                    found = Some(h);
                    break;
                }
                count += 1;
            }
        }
        found.expect("trak missing")
    }

    fn locate_child(
        buf: &[u8],
        parent: &crate::demux::reader::BoxHeader,
        target: [u8; 4],
    ) -> crate::demux::reader::BoxHeader {
        let reader = crate::read_at::SliceReader(buf);
        let mut iter = BoxIter::new(&reader, parent.payload_offset, parent.end());
        while let Some(h) = iter.next_header().unwrap() {
            if h.box_type == target {
                return h;
            }
        }
        panic!("child {:?} not found inside {:?}", target, parent.box_type);
    }
}
