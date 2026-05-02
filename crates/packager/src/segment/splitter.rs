//! Segment splitter.
//!
//! Produces the segment table from the per-track raw sample tables. A segment
//! covers a half-open video range `[video_start, video_end)` whose first
//! sample is a sync sample (IDR); audio samples are partitioned by mapping
//! the video boundary's cumulative DTS into the audio timescale.
//!
//! Algorithm:
//! 1. Open the first segment at sample 0 — which must be sync; a non-sync
//!    first sample violates the "every segment starts at an IDR" invariant.
//! 2. Walk video samples; when a later sync sample is reached and the
//!    cumulative duration since the segment's start meets `nominal_secs`,
//!    close the segment at that sync sample and open the next one there.
//! 3. If no further sync sample is reached before EOF, the tail forms a
//!    single trailing segment (legal — happens when the input lacked
//!    `stss` only for sample 0 or had a sole leading IDR).
//! 4. For each video boundary, pin the audio boundary to the first audio
//!    sample whose cumulative DTS — projected into the audio timescale —
//!    meets or exceeds the video boundary's DTS. Audio samples that
//!    straddle the boundary are kept with the segment they started in;
//!    sample 0 always belongs to segment 0 so AAC priming is preserved.

use crate::error::PackagerError;
use crate::index::format::SegmentEntry;

use super::super::demux::sample_table::RawSampleEntry;

/// Build the segment table.
///
/// Returns `SampleTableInconsistent` for inputs the splitter cannot honour:
/// missing tracks, a non-sync first video sample, non-positive
/// `nominal_secs`, zero timescales, or any segment that would otherwise
/// contain zero video / audio samples.
pub(crate) fn split(
    video: &[RawSampleEntry],
    audio: &[RawSampleEntry],
    video_timescale: u32,
    audio_timescale: u32,
    nominal_secs: f64,
) -> Result<Vec<SegmentEntry>, PackagerError> {
    if video.is_empty() {
        return Err(PackagerError::SampleTableInconsistent(
            "splitter requires at least one video sample",
        ));
    }
    if audio.is_empty() {
        return Err(PackagerError::SampleTableInconsistent(
            "splitter requires at least one audio sample",
        ));
    }
    if !video[0].is_sync {
        return Err(PackagerError::SampleTableInconsistent(
            "first video sample is not a sync sample",
        ));
    }
    if video_timescale == 0 || audio_timescale == 0 {
        return Err(PackagerError::SampleTableInconsistent(
            "track timescale is zero",
        ));
    }
    if !(nominal_secs.is_finite() && nominal_secs > 0.0) {
        return Err(PackagerError::SampleTableInconsistent(
            "segment nominal duration must be positive and finite",
        ));
    }

    let video_count: u32 = u32::try_from(video.len()).map_err(|_| {
        PackagerError::SampleTableInconsistent("video sample count exceeds u32::MAX")
    })?;
    let audio_count: u32 = u32::try_from(audio.len()).map_err(|_| {
        PackagerError::SampleTableInconsistent("audio sample count exceeds u32::MAX")
    })?;

    // Per-segment minimum duration in the video track's timescale. Computed
    // once via float, then integer math throughout.
    let nominal_ticks: u64 = (nominal_secs * f64::from(video_timescale)).ceil() as u64;
    let nominal_ticks = nominal_ticks.max(1);

    let mut segments: Vec<SegmentEntry> = Vec::new();

    // Active segment state.
    let mut v_start: u32 = 0;
    let mut v_base_dts: u64 = 0;
    let mut v_dts: u64 = 0; // cumulative DTS of samples [0..i)

    // Audio cursor advances monotonically; `a_dts` is the DTS of `a_cursor`.
    let mut a_start: u32 = 0;
    let mut a_base_dts: u64 = 0;
    let mut a_cursor: u32 = 0;
    let mut a_dts: u64 = 0;

    for (i, sample) in video.iter().enumerate() {
        let dts_i = v_dts; // start DTS of video sample i

        // Close the active segment when we reach a sync sample (other than
        // the segment's own start) and the elapsed duration meets nominal.
        let here = u32::try_from(i).map_err(|_| {
            PackagerError::SampleTableInconsistent("video sample index exceeds u32::MAX")
        })?;
        if here > v_start && sample.is_sync && dts_i.saturating_sub(v_base_dts) >= nominal_ticks {
            advance_audio_to(
                &mut a_cursor,
                &mut a_dts,
                audio,
                scale_dts_ceil(dts_i, video_timescale, audio_timescale)?,
            )?;

            push_segment(
                &mut segments,
                SegmentEntry {
                    video_sample_start: v_start,
                    video_sample_count: here - v_start,
                    audio_sample_start: a_start,
                    audio_sample_count: a_cursor - a_start,
                    video_base_dts: v_base_dts,
                    audio_base_dts: a_base_dts,
                },
            )?;

            v_start = here;
            v_base_dts = dts_i;
            a_start = a_cursor;
            a_base_dts = a_dts;
        }

        v_dts = v_dts.checked_add(u64::from(sample.dts_delta)).ok_or(
            PackagerError::SampleTableInconsistent("video cumulative DTS overflows u64"),
        )?;
    }

    // Tail segment: everything from `v_start` to the end of both tracks.
    push_segment(
        &mut segments,
        SegmentEntry {
            video_sample_start: v_start,
            video_sample_count: video_count - v_start,
            audio_sample_start: a_start,
            audio_sample_count: audio_count - a_start,
            video_base_dts: v_base_dts,
            audio_base_dts: a_base_dts,
        },
    )?;

    Ok(segments)
}

fn push_segment(
    segments: &mut Vec<SegmentEntry>,
    entry: SegmentEntry,
) -> Result<(), PackagerError> {
    if entry.video_sample_count == 0 || entry.audio_sample_count == 0 {
        return Err(PackagerError::SampleTableInconsistent(
            "splitter produced an empty segment",
        ));
    }
    segments.push(entry);
    Ok(())
}

/// Project a video-timescale DTS onto the first audio tick at or after it.
///
/// Ceiling division is required so that an audio sample whose start tick is
/// strictly before the real (fractional) video boundary stays with the
/// preceding segment, matching the straddle rule documented on
/// `advance_audio_to`. Floor division would silently shift those samples
/// into the wrong segment whenever `video_timescale` does not divide
/// `video_dts × audio_timescale`.
fn scale_dts_ceil(
    video_dts: u64,
    video_timescale: u32,
    audio_timescale: u32,
) -> Result<u64, PackagerError> {
    let numerator = u128::from(video_dts) * u128::from(audio_timescale);
    let denominator = u128::from(video_timescale);
    let scaled = numerator.div_ceil(denominator);
    u64::try_from(scaled)
        .map_err(|_| PackagerError::SampleTableInconsistent("scaled audio DTS overflows u64"))
}

/// Advance `cursor` until `cursor_dts >= target_audio_dts`. A sample whose
/// start is before the boundary but end crosses past it is kept with the
/// preceding segment (cursor steps over it), so audio coverage is
/// contiguous and every sample lands in exactly one segment.
fn advance_audio_to(
    cursor: &mut u32,
    cursor_dts: &mut u64,
    audio: &[RawSampleEntry],
    target_audio_dts: u64,
) -> Result<(), PackagerError> {
    while (*cursor as usize) < audio.len() && *cursor_dts < target_audio_dts {
        *cursor_dts = cursor_dts
            .checked_add(u64::from(audio[*cursor as usize].dts_delta))
            .ok_or(PackagerError::SampleTableInconsistent(
                "audio cumulative DTS overflows u64",
            ))?;
        *cursor += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vsample(dts_delta: u32, is_sync: bool) -> RawSampleEntry {
        RawSampleEntry {
            offset: 0,
            size: 1,
            dts_delta,
            cts_offset: 0,
            is_sync,
        }
    }

    fn asample(dts_delta: u32) -> RawSampleEntry {
        RawSampleEntry {
            offset: 0,
            size: 1,
            dts_delta,
            cts_offset: 0,
            is_sync: true,
        }
    }

    /// 60 frames at 30 fps with sync every 30 frames, nominal 1.0 s →
    /// 2 segments of 30 video samples each.
    #[test]
    fn sync_every_30_frames_produces_two_one_second_segments() {
        let video: Vec<RawSampleEntry> = (0..60).map(|i| vsample(1, i % 30 == 0)).collect();
        let audio: Vec<RawSampleEntry> = (0..60).map(|_| asample(1)).collect();

        let segments = split(&video, &audio, 30, 30, 1.0).expect("split succeeds");
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].video_sample_start, 0);
        assert_eq!(segments[0].video_sample_count, 30);
        assert_eq!(segments[0].video_base_dts, 0);
        assert_eq!(segments[0].audio_sample_start, 0);
        assert_eq!(segments[0].audio_sample_count, 30);
        assert_eq!(segments[0].audio_base_dts, 0);

        assert_eq!(segments[1].video_sample_start, 30);
        assert_eq!(segments[1].video_sample_count, 30);
        assert_eq!(segments[1].video_base_dts, 30);
        assert_eq!(segments[1].audio_sample_start, 30);
        assert_eq!(segments[1].audio_sample_count, 30);
        assert_eq!(segments[1].audio_base_dts, 30);
    }

    /// 60 frames with a sync sample only at frame 0, nominal 1.0 s →
    /// 1 segment of 60 video samples (the tail-segment fallback).
    #[test]
    fn sparse_sync_yields_single_tail_segment() {
        let video: Vec<RawSampleEntry> = (0..60).map(|i| vsample(1, i == 0)).collect();
        let audio: Vec<RawSampleEntry> = (0..60).map(|_| asample(1)).collect();

        let segments = split(&video, &audio, 30, 30, 1.0).expect("split succeeds");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].video_sample_start, 0);
        assert_eq!(segments[0].video_sample_count, 60);
        assert_eq!(segments[0].audio_sample_start, 0);
        assert_eq!(segments[0].audio_sample_count, 60);
        assert_eq!(segments[0].video_base_dts, 0);
        assert_eq!(segments[0].audio_base_dts, 0);
    }

    /// Audio in a different timescale than video. Boundary must respect
    /// rational scaling: 30 video ticks @ ts=30 ≡ 45 audio ticks @ ts=45.
    #[test]
    fn audio_split_respects_track_timescale_difference() {
        // Video: 60 frames at ts=30, sync every 30 frames (2 s of media).
        let video: Vec<RawSampleEntry> = (0..60).map(|i| vsample(1, i % 30 == 0)).collect();
        // Audio: 90 frames at ts=45, dts_delta=1 (also 2 s of media).
        let audio: Vec<RawSampleEntry> = (0..90).map(|_| asample(1)).collect();

        let segments = split(&video, &audio, 30, 45, 1.0).expect("split succeeds");
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].audio_sample_start, 0);
        assert_eq!(segments[0].audio_sample_count, 45);
        assert_eq!(segments[1].audio_sample_start, 45);
        assert_eq!(segments[1].audio_base_dts, 45);
        assert_eq!(segments[1].audio_sample_count, 45);

        // Audio coverage is contiguous and fully accounted for.
        assert_eq!(
            segments[0].audio_sample_start + segments[0].audio_sample_count,
            segments[1].audio_sample_start
        );
        assert_eq!(
            segments[1].audio_sample_start + segments[1].audio_sample_count,
            audio.len() as u32
        );
    }

    /// Sync samples land at non-uniform spacing; the first sync sample past
    /// the nominal mark wins, even if a closer one came too early.
    #[test]
    fn cuts_snap_forward_to_next_sync_after_nominal() {
        // Sync at frames 0, 25, 70 in a 90-frame stream at ts=30, nominal=1.0 s.
        // At frame 25 elapsed is 25 ticks < 30, so the cut waits; at frame 70
        // elapsed is 70 ≥ 30, so the cut lands there.
        let video: Vec<RawSampleEntry> = (0..90)
            .map(|i| vsample(1, [0, 25, 70].contains(&i)))
            .collect();

        let audio: Vec<RawSampleEntry> = (0..90).map(|_| asample(1)).collect();
        let segments = split(&video, &audio, 30, 30, 1.0).expect("split succeeds");
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].video_sample_start, 0);
        assert_eq!(segments[0].video_sample_count, 70);
        assert_eq!(segments[1].video_sample_start, 70);
        assert_eq!(segments[1].video_sample_count, 20);
    }

    /// Fractional video→audio projection: an audio sample whose start lies
    /// strictly before the real boundary must remain with the preceding
    /// segment. Floor-division would have moved it forward.
    #[test]
    fn fractional_audio_boundary_keeps_straddling_sample_with_previous_segment() {
        // Cut at video DTS 2 in ts=3 (= 2/3 s). Projected to ts=4 that is
        // 8/3 = 2.666… — the audio sample starting at tick 2 still belongs
        // to segment 0; the sample starting at tick 3 opens segment 1.
        let video: Vec<RawSampleEntry> = (0..4).map(|i| vsample(1, i == 0 || i == 2)).collect();
        let audio: Vec<RawSampleEntry> = (0..5).map(|_| asample(1)).collect();

        let segments = split(&video, &audio, 3, 4, 0.5).expect("split succeeds");
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].video_sample_start, 0);
        assert_eq!(segments[0].video_sample_count, 2);
        assert_eq!(segments[0].audio_sample_start, 0);
        assert_eq!(segments[0].audio_sample_count, 3);

        assert_eq!(segments[1].video_sample_start, 2);
        assert_eq!(segments[1].video_sample_count, 2);
        assert_eq!(segments[1].audio_sample_start, 3);
        assert_eq!(segments[1].audio_sample_count, 2);
        assert_eq!(segments[1].audio_base_dts, 3);
    }

    #[test]
    fn first_video_sample_must_be_sync() {
        let video = vec![vsample(1, false), vsample(1, true)];
        let audio = vec![asample(1)];
        let err = split(&video, &audio, 30, 30, 1.0).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn empty_video_rejected() {
        let video: Vec<RawSampleEntry> = vec![];
        let audio = vec![asample(1)];
        let err = split(&video, &audio, 30, 30, 1.0).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn empty_audio_rejected() {
        let video = vec![vsample(1, true)];
        let audio: Vec<RawSampleEntry> = vec![];
        let err = split(&video, &audio, 30, 30, 1.0).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }

    #[test]
    fn nonpositive_nominal_rejected() {
        let video = vec![vsample(1, true)];
        let audio = vec![asample(1)];
        for bad in [0.0_f64, -1.0, f64::NAN, f64::INFINITY] {
            let err = split(&video, &audio, 30, 30, bad).unwrap_err();
            assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
        }
    }

    #[test]
    fn zero_timescale_rejected() {
        let video = vec![vsample(1, true)];
        let audio = vec![asample(1)];
        let err = split(&video, &audio, 0, 30, 1.0).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
        let err = split(&video, &audio, 30, 0, 1.0).unwrap_err();
        assert!(matches!(err, PackagerError::SampleTableInconsistent(_)));
    }
}
