//! Media-playlist writer.
//!
//! Emits a single VOD HLS media playlist: `#EXT-X-VERSION:7`, an
//! `#EXT-X-MAP` referencing `init.mp4`, and one `#EXTINF` + segment URI
//! per `SegmentEntry`. The video track is the timeline master because
//! IDR / segment boundaries are video-driven; audio frame boundaries do
//! not align with segment cuts and are not used.
//!
//! Per-segment EXTINF is `(sum of segment's video dts_deltas) /
//! video_timescale`, formatted with 6 decimal places to keep cumulative
//! drift below 1 ms over a 2-hour playlist (3-decimal precision can drift
//! up to ~0.6 s at 1 200 segments). `EXT-X-TARGETDURATION` is computed
//! from the same precise rational across all segments —
//! `ceil(max(EXTINF))` via integer ceiling division — so the two values
//! are coherent regardless of float formatting.

use std::io::Write;

use crate::error::PackagerError;
use crate::index::format::{SampleEntry, SegmentEntry};
use crate::index::view::IndexView;

/// Write the media playlist text.
///
/// Pure function over the segment table — does not touch the source MP4
/// or `init.mp4` bytes. Pairs with `IndexBuilder::build`, which embeds
/// the rendered output as the optional `KIND_PLAYLIST_BYTES` section.
pub fn write_media_playlist<W: Write>(
    index: &IndexView<'_>,
    out: &mut W,
) -> Result<(), PackagerError> {
    let timescale = index.video_track().timescale;
    if timescale == 0 {
        return Err(PackagerError::SampleTableInconsistent(
            "video timescale is zero (cannot compute EXTINF)",
        ));
    }
    let video_samples = index.video_samples();
    let segments = index.segments();

    let seg_ticks: Vec<u64> = segments
        .iter()
        .map(|s| segment_video_ticks(s, video_samples))
        .collect::<Result<_, _>>()?;

    // `target_duration` is integer seconds, derived from the precise
    // tick-domain max so float formatting cannot push it out of step with
    // the EXTINF values printed below. `unwrap_or(0)` covers the
    // segments-empty case defensively; the splitter rejects empty input
    // before we reach this writer in practice.
    let max_ticks = seg_ticks.iter().copied().max().unwrap_or(0);
    let target_duration = max_ticks.div_ceil(u64::from(timescale));

    out.write_all(b"#EXTM3U\n")?;
    out.write_all(b"#EXT-X-VERSION:7\n")?;
    out.write_all(b"#EXT-X-INDEPENDENT-SEGMENTS\n")?;
    writeln!(out, "#EXT-X-TARGETDURATION:{target_duration}")?;
    out.write_all(b"#EXT-X-PLAYLIST-TYPE:VOD\n")?;
    out.write_all(b"#EXT-X-MAP:URI=\"init.mp4\"\n")?;

    for (i, ticks) in seg_ticks.iter().enumerate() {
        let secs = (*ticks as f64) / f64::from(timescale);
        let n = i + 1;
        writeln!(out, "#EXTINF:{secs:.6},")?;
        writeln!(out, "seg_{n:04}.m4s")?;
    }

    out.write_all(b"#EXT-X-ENDLIST\n")?;

    Ok(())
}

fn segment_video_ticks(
    segment: &SegmentEntry,
    video_samples: &[SampleEntry],
) -> Result<u64, PackagerError> {
    let start = segment.video_sample_start as usize;
    let count = segment.video_sample_count as usize;
    let end = start
        .checked_add(count)
        .ok_or(PackagerError::SampleTableInconsistent(
            "segment sample range overflows usize",
        ))?;
    if end > video_samples.len() {
        return Err(PackagerError::SampleTableInconsistent(
            "segment sample range exceeds video sample table",
        ));
    }
    let mut ticks: u64 = 0;
    for s in &video_samples[start..end] {
        ticks = ticks.checked_add(u64::from(s.dts_delta)).ok_or(
            PackagerError::SampleTableInconsistent("segment dts ticks overflow u64"),
        )?;
    }
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::format::{SampleEntry, SegmentEntry};
    use crate::index::view::{AudioTrackMeta, IndexView, VideoTrackMeta};

    fn vsample(dts_delta: u32, is_sync: bool) -> SampleEntry {
        SampleEntry {
            offset: 0,
            size: 1,
            dts_delta,
            cts_offset: 0,
            flags: if is_sync { 1 } else { 0 },
        }
    }

    fn segment(start: u32, count: u32, base: u64) -> SegmentEntry {
        SegmentEntry {
            video_sample_start: start,
            video_sample_count: count,
            audio_sample_start: 0,
            audio_sample_count: 0,
            video_base_dts: base,
            audio_base_dts: 0,
        }
    }

    fn build_view<'a>(
        timescale: u32,
        video_samples: &'a [SampleEntry],
        segments: &'a [SegmentEntry],
        blake3: &'a [u8; 32],
    ) -> IndexView<'a> {
        IndexView::from_parts(
            0,
            0,
            blake3,
            VideoTrackMeta {
                timescale,
                fourcc: *b"avc1",
                width: 1920,
                height: 1080,
                sample_entry: &[],
                elst: &[],
            },
            AudioTrackMeta {
                timescale: 48_000,
                fourcc: *b"mp4a",
                sample_rate: 48_000,
                channel_count: 2,
                sample_entry: &[],
                elst: &[],
            },
            video_samples,
            &[],
            segments,
            &[],
            None,
        )
    }

    /// 3-segment fixture with deliberately distinct durations:
    ///   seg 0:  6006 ticks @ ts=1000 → 6.006000 s
    ///   seg 1:  6500 ticks @ ts=1000 → 6.500000 s   ← drives target_duration
    ///   seg 2:  3000 ticks @ ts=1000 → 3.000000 s
    /// `EXT-X-TARGETDURATION` = ceil(6.500) = 7.
    #[test]
    fn three_segments_emit_exact_spec_text() {
        let blake3 = [0u8; 32];
        let video_samples = [
            vsample(3003, true),
            vsample(3003, false),
            vsample(3250, true),
            vsample(3250, false),
            vsample(1500, true),
            vsample(1500, false),
        ];
        let segments = [
            segment(0, 2, 0),
            segment(2, 2, 6_006),
            segment(4, 2, 12_506),
        ];
        let view = build_view(1000, &video_samples, &segments, &blake3);

        let mut out = Vec::new();
        write_media_playlist(&view, &mut out).expect("write");
        let text = String::from_utf8(out).expect("ascii");

        let expected = concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:7\n",
            "#EXT-X-INDEPENDENT-SEGMENTS\n",
            "#EXT-X-TARGETDURATION:7\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n",
            "#EXT-X-MAP:URI=\"init.mp4\"\n",
            "#EXTINF:6.006000,\n",
            "seg_0001.m4s\n",
            "#EXTINF:6.500000,\n",
            "seg_0002.m4s\n",
            "#EXTINF:3.000000,\n",
            "seg_0003.m4s\n",
            "#EXT-X-ENDLIST\n",
        );
        assert_eq!(text, expected);
    }

    /// Integer-second EXTINF still gets six decimal zeros, and
    /// `target_duration` matches when the max EXTINF is exactly an
    /// integer.
    #[test]
    fn target_duration_for_integer_max_extinf() {
        let blake3 = [0u8; 32];
        let video_samples = [vsample(30_000, true), vsample(30_000, true)];
        let segments = [segment(0, 1, 0), segment(1, 1, 30_000)];
        let view = build_view(30_000, &video_samples, &segments, &blake3);

        let mut out = Vec::new();
        write_media_playlist(&view, &mut out).expect("write");
        let text = String::from_utf8(out).expect("ascii");

        assert!(text.contains("#EXT-X-TARGETDURATION:1\n"));
        assert!(text.contains("#EXTINF:1.000000,\n"));
    }

    /// `seg_NNNN.m4s` is a *minimum-width* format: segment 9999 stays
    /// 4-digit, segment 10000 widens to 5 digits without breaking the
    /// route shape. Confirms the writer doesn't artificially cap input
    /// at 9999 segments.
    #[test]
    fn segment_numbering_uses_four_digits_as_minimum_width() {
        let blake3 = [0u8; 32];
        let video_samples: Vec<SampleEntry> = (0..10_000).map(|_| vsample(1, true)).collect();
        let segments: Vec<SegmentEntry> = (0u32..10_000)
            .map(|i| segment(i, 1, u64::from(i)))
            .collect();
        let view = build_view(1, &video_samples, &segments, &blake3);

        let mut out = Vec::new();
        write_media_playlist(&view, &mut out).expect("write");
        let text = String::from_utf8(out).expect("ascii");

        assert!(text.contains("seg_9999.m4s\n"));
        assert!(text.contains("seg_10000.m4s\n"));
    }

    #[test]
    fn segment_index_out_of_range_returns_inconsistent() {
        let blake3 = [0u8; 32];
        let video_samples = [vsample(1000, true)];
        // Segment claims 5 video samples but the table only has 1.
        let segments = [segment(0, 5, 0)];
        let view = build_view(1000, &video_samples, &segments, &blake3);

        let mut out = Vec::new();
        let err = write_media_playlist(&view, &mut out).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn zero_timescale_rejected() {
        let blake3 = [0u8; 32];
        let video_samples = [vsample(1, true)];
        let segments = [segment(0, 1, 0)];
        let view = build_view(0, &video_samples, &segments, &blake3);

        let mut out = Vec::new();
        let err = write_media_playlist(&view, &mut out).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }
}
