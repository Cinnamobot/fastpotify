//! Beat grids and transition planning for automix.
//!
//! The official desktop client takes its transitions from Spotify's servers:
//! `spclient` publishes per-track beats, cuepoints and a per-pair recipe with
//! the tempo ratio and the bar counts to overlap. That service is only
//! computed for playlists Spotify has flagged for mixing, so a client that
//! wants the same behaviour everywhere has to derive it locally.
//!
//! This module does that. [`Analysis::of`] turns interleaved PCM into a beat
//! grid with downbeats, and [`plan`] pairs two grids into a [`Transition`]:
//! where each track's fade starts, how many bars it runs, and the ratio one
//! deck is stretched by so the beats line up. When the two tracks cannot be
//! matched the planner returns `None` and the caller falls back to a plain
//! crossfade, which is what the server itself does for pairs it rejects.

use std::time::Duration;

use timestretch::BeatGrid;

/// The longest overlap the official clients allow, and the ceiling our
/// planner keeps to.
pub const MAX_TRANSITION: Duration = Duration::from_secs(12);

/// Refuse a pair whose tempos differ by more than this, as a fraction. The
/// server's own published recipes stay inside roughly ±7.6%, so matching
/// that keeps our output in the range listeners already accept.
pub const MAX_TEMPO_DIFF: f64 = 0.08;

/// Bars a transition may run, longest first. The server's recipes use 4
/// mostly and 2 for tighter pairs; 8 covers slow tracks where 4 bars would
/// fall under [`MIN_TRANSITION`].
const BAR_CHOICES: [u8; 3] = [4, 8, 2];

/// Below this the overlap is too short to hide the seam, so a shorter bar
/// count is skipped in favour of a longer one.
const MIN_TRANSITION: Duration = Duration::from_millis(1_500);

/// Refuse a grid the tracker was not sure about: snapping a musical moment
/// to a doubtful beat is worse than a plain crossfade.
const MIN_CONFIDENCE: f32 = 0.35;

/// Beats in a bar. The tracker does not report a time signature, and four is
/// what nearly all the material Spotify mixes uses.
const BEATS_PER_BAR: f64 = 4.0;

/// A beat grid and the tempo it was tracked at.
#[derive(Clone, Debug)]
pub struct Analysis {
    grid: BeatGrid,
    /// Beats per minute, from the grid's committed tempo.
    pub bpm: f64,
    /// Seconds from the start of the track to its first beat.
    pub first_beat: f64,
}

impl Analysis {
    /// Beat-track interleaved samples.
    ///
    /// `samples` is what the decoder hands the sink: interleaved, two
    /// channels. `timestretch` wants the mono mid downmix, so this folds the
    /// channels first.
    pub fn of(samples: &[f32], sample_rate: u32) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mono = timestretch::downmix_to_mid(samples, crate::vis::CHANNELS as usize);
        Self::from_grid(timestretch::detect_beat_grid(&mono, sample_rate))
    }

    fn from_grid(grid: BeatGrid) -> Option<Self> {
        if grid.beats.is_empty() || !(grid.bpm.is_finite() && grid.bpm > 0.0) {
            return None;
        }
        if grid.confidence < MIN_CONFIDENCE || grid.phase_untrusted {
            return None;
        }
        Some(Self {
            first_beat: grid.beats[0] / f64::from(grid.sample_rate),
            bpm: grid.bpm,
            grid,
        })
    }

    /// Downbeat positions in seconds. The grid marks these by index; when it
    /// marks none, every fourth beat stands in.
    fn downbeats(&self) -> Vec<f64> {
        let rate = f64::from(self.grid.sample_rate);
        let marked: Vec<f64> = self
            .grid
            .downbeats
            .iter()
            .filter_map(|index| self.grid.beats.get(*index))
            .map(|position| position / rate)
            .collect();
        if marked.len() >= 2 {
            return marked;
        }
        self.grid
            .beats
            .iter()
            .step_by(BEATS_PER_BAR as usize)
            .map(|position| position / rate)
            .collect()
    }

    /// The last downbeat at or before `seconds`, falling back to the first
    /// downbeat when the track has not reached one yet.
    fn downbeat_at_or_before(&self, seconds: f64) -> Option<f64> {
        let beats = self.downbeats();
        beats
            .iter()
            .rev()
            .find(|start| **start <= seconds)
            .copied()
            .or_else(|| beats.first().copied())
    }

    /// The first downbeat at or after `seconds`.
    fn downbeat_at_or_after(&self, seconds: f64) -> Option<f64> {
        let beats = self.downbeats();
        beats
            .iter()
            .find(|start| **start >= seconds)
            .copied()
            .or_else(|| beats.last().copied())
    }
}

/// Where each deck goes and how far one is stretched, for one pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    /// Seconds into the outgoing track where its fade starts.
    pub fade_out_at: f64,
    /// Seconds into the incoming track where its fade starts.
    pub fade_in_at: f64,
    /// How long the two overlap.
    pub duration: Duration,
    /// Tempo multiplier for the incoming track: the outgoing tempo over the
    /// incoming one, folded to the nearest octave.
    pub tempo_ratio: f64,
}

/// Bars the overlap spans at the outgoing track's tempo.
pub fn bars_of(duration: Duration, bpm: f64) -> f64 {
    duration.as_secs_f64() * bpm / 60.0 / BEATS_PER_BAR
}

/// Plan a transition when only the outgoing track has been analysed.
///
/// The incoming track is only preloaded, so its audio never reaches the
/// sink's collector and its grid is unknown. That costs two things: it
/// starts at its own beginning rather than on a chosen downbeat, and it is
/// not stretched. What it still buys is the part listeners notice — the
/// outgoing track leaves on a downbeat instead of wherever its last sample
/// falls, and the overlap is a whole number of bars.
pub fn plan_exit(from: &Analysis, out_duration: Duration) -> Option<Transition> {
    let beat_seconds = 60.0 / from.bpm;
    for bars in BAR_CHOICES {
        let seconds = f64::from(bars) * BEATS_PER_BAR * beat_seconds;
        let duration = Duration::from_secs_f64(seconds);
        if duration > MAX_TRANSITION || duration < MIN_TRANSITION {
            continue;
        }
        let slack = beat_seconds * BEATS_PER_BAR;
        let latest_start = out_duration.as_secs_f64() - seconds - slack;
        if latest_start <= from.first_beat {
            continue;
        }
        let Some(fade_out_at) = from.downbeat_at_or_before(latest_start) else {
            continue;
        };
        return Some(Transition {
            fade_out_at,
            fade_in_at: 0.0,
            duration,
            tempo_ratio: 1.0,
        });
    }
    None
}

/// Plan a transition from one analysed track into another.
///
/// Returns `None` when the pair is not mixable: a low-confidence grid, a
/// tempo gap too wide to stretch across, or a track too short to fade. The
/// caller then uses a plain crossfade.
pub fn plan(from: &Analysis, to: &Analysis, out_duration: Duration) -> Option<Transition> {
    // Match the tempos. A factor near 0.5 or 2.0 is the same groove at half
    // or double time, so fold those in before judging the gap.
    let ratio = fold_octave(from.bpm / to.bpm);
    if (ratio - 1.0).abs() > MAX_TEMPO_DIFF {
        return None;
    }

    let beat_seconds = 60.0 / from.bpm;
    for bars in BAR_CHOICES {
        let seconds = f64::from(bars) * BEATS_PER_BAR * beat_seconds;
        let duration = Duration::from_secs_f64(seconds);
        if duration > MAX_TRANSITION || duration < MIN_TRANSITION {
            continue;
        }

        // Leave a bar of slack so a slow decode cannot cut the fade short.
        let slack = beat_seconds * BEATS_PER_BAR;
        let latest_start = out_duration.as_secs_f64() - seconds - slack;
        if latest_start <= from.first_beat {
            continue;
        }

        let Some(fade_out_at) = from.downbeat_at_or_before(latest_start) else {
            continue;
        };
        // Start on the incoming track's first downbeat: before that the
        // tracker has no history, so its phase is the least reliable.
        let Some(fade_in_at) = to.downbeat_at_or_after(to.first_beat) else {
            continue;
        };

        return Some(Transition {
            fade_out_at,
            fade_in_at,
            duration,
            tempo_ratio: ratio,
        });
    }
    None
}

/// Fold a tempo ratio into the octave nearest 1.0.
///
/// A pair 1.9× apart is the same groove at double time and folds to 0.95,
/// which keylock handles. This is what the server does under
/// `auto_transition_allow_octave_bpm_correction`.
///
/// Folding only helps when one tempo is near a multiple of the other. 136
/// against 80 is 1.7, whose nearest octave is 0.85 — still 15% off, so the
/// pair is refused here just as the server refuses it.
fn fold_octave(ratio: f64) -> f64 {
    if !(ratio.is_finite() && ratio > 0.0) {
        return 1.0;
    }
    let mut best = ratio;
    let mut best_distance = (ratio - 1.0).abs();
    for shift in [-2i32, -1, 1, 2] {
        let candidate = ratio * 2f64.powi(shift);
        let distance = (candidate - 1.0).abs();
        if distance < best_distance {
            best = candidate;
            best_distance = distance;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click track: a short pulse every beat, so the tracker has a period
    /// with no ambiguity to lock onto.
    fn click_track(bpm: f64, seconds: f64, rate: u32) -> Vec<f32> {
        let beat = 60.0 / bpm;
        let total = (seconds * f64::from(rate)) as usize;
        let mut samples = vec![0.0f32; total * crate::vis::CHANNELS as usize];
        let mut t = 0.0;
        while t < seconds {
            let start = (t * f64::from(rate)) as usize * crate::vis::CHANNELS as usize;
            for i in 0..(rate as usize / 50) {
                let index = start + i * crate::vis::CHANNELS as usize;
                if index + 1 < samples.len() {
                    let decay = (-(i as f32) / 40.0).exp();
                    samples[index] = decay;
                    samples[index + 1] = decay;
                }
            }
            t += beat;
        }
        samples
    }

    #[test]
    fn an_empty_buffer_is_not_analysable() {
        assert!(Analysis::of(&[], 44_100).is_none());
    }

    #[test]
    fn silence_is_not_analysable() {
        let samples = vec![0.0f32; 44_100 * crate::vis::CHANNELS as usize * 5];
        assert!(Analysis::of(&samples, 44_100).is_none());
    }

    #[test]
    fn a_click_track_is_tracked_at_its_tempo() {
        let samples = click_track(128.0, 25.0, 44_100);
        let analysis = Analysis::of(&samples, 44_100).expect("a click track is analysable");
        assert!(
            (analysis.bpm - 128.0).abs() < 4.0,
            "tracked {} for a 128 BPM click",
            analysis.bpm
        );
    }

    #[test]
    fn octave_folding_pulls_a_double_tempo_back_to_one() {
        assert!((fold_octave(2.0) - 1.0).abs() < 1e-9);
        assert!((fold_octave(0.5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn octave_folding_rescues_a_ratio_that_is_otherwise_hopeless() {
        // 1.9x is far outside the limit, but double time is the same groove.
        let raw: f64 = 1.9;
        assert!((raw - 1.0).abs() > MAX_TEMPO_DIFF);
        let folded = fold_octave(raw);
        assert!(
            (folded - 0.95).abs() < 1e-9,
            "1.9 should fold down to 0.95, got {folded}"
        );
        assert!((folded - 1.0).abs() <= MAX_TEMPO_DIFF);
    }

    #[test]
    fn folding_does_not_rescue_a_ratio_outside_every_octave() {
        // 136 against 80 is 1.7, and its nearest octave is 0.85: still a 15%
        // stretch. The server refuses this pair too, returning a zero-bar
        // recipe, so refusing it here matches what listeners already get.
        let raw: f64 = 136.0 / 80.0;
        assert!((fold_octave(raw) - 1.0).abs() > MAX_TEMPO_DIFF);
    }

    #[test]
    fn octave_folding_leaves_a_close_tempo_alone() {
        let ratio = 1.03;
        assert!((fold_octave(ratio) - ratio).abs() < 1e-9);
    }

    #[test]
    fn a_nonsense_ratio_folds_to_one() {
        assert_eq!(fold_octave(0.0), 1.0);
        assert_eq!(fold_octave(f64::NAN), 1.0);
        assert_eq!(fold_octave(-2.0), 1.0);
    }

    #[test]
    fn an_unmatched_incoming_track_still_plans_a_downbeat_exit() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        let planned =
            plan_exit(&a, Duration::from_secs(40)).expect("one analysed track is enough");
        // Nothing is stretched when the other grid is unknown.
        assert_eq!(planned.tempo_ratio, 1.0);
        assert_eq!(planned.fade_in_at, 0.0);
        assert!(planned.duration >= MIN_TRANSITION);
        // The exit lands on a downbeat inside what is left of the track.
        assert!(a.downbeats().iter().any(|beat| (beat - planned.fade_out_at).abs() < 1e-9));
        assert!(planned.fade_out_at + planned.duration.as_secs_f64() <= 40.0);
    }

    #[test]
    fn an_unmatched_track_too_short_to_fade_is_refused() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        assert!(plan_exit(&a, Duration::from_secs(2)).is_none());
    }

    #[test]
    fn a_pair_with_matched_tempos_plans_a_transition() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        let b = Analysis::of(&click_track(130.0, 40.0, 44_100), 44_100).unwrap();
        let planned = plan(&a, &b, Duration::from_secs(40)).expect("matched tempos mix");
        assert!(planned.duration >= MIN_TRANSITION);
        assert!(planned.duration <= MAX_TRANSITION);
        assert!((planned.tempo_ratio - 1.0).abs() <= MAX_TEMPO_DIFF);
        // The fade has to finish before the outgoing track does.
        assert!(planned.fade_out_at + planned.duration.as_secs_f64() <= 40.0);
    }

    #[test]
    fn a_pair_with_a_hopeless_tempo_gap_is_refused() {
        let a = Analysis::of(&click_track(90.0, 40.0, 44_100), 44_100).unwrap();
        // 90 against 150 folds to 1.2, past the stretch limit.
        let b = Analysis::of(&click_track(150.0, 40.0, 44_100), 44_100).unwrap();
        assert!(plan(&a, &b, Duration::from_secs(40)).is_none());
    }

    #[test]
    fn a_track_too_short_to_fade_is_refused() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        let b = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        assert!(plan(&a, &b, Duration::from_secs(2)).is_none());
    }

    #[test]
    fn bars_of_reports_whole_bars_for_a_four_bar_overlap() {
        // Four bars at 120 BPM is eight seconds.
        assert!((bars_of(Duration::from_secs(8), 120.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn downbeats_fall_back_to_every_fourth_beat() {
        let a = Analysis::of(&click_track(120.0, 25.0, 44_100), 44_100).unwrap();
        let downbeats = a.downbeats();
        assert!(downbeats.len() >= 2);
        let spacing = downbeats[1] - downbeats[0];
        // A quarter-note fallback at 120 BPM is half a second.
        assert!(
            spacing > 0.0 && spacing <= 2.5,
            "unexpected downbeat spacing {spacing}"
        );
    }

    #[test]
    fn downbeat_lookup_snaps_inside_the_track() {
        let a = Analysis::of(&click_track(120.0, 25.0, 44_100), 44_100).unwrap();
        let before = a.downbeat_at_or_before(5.0).unwrap();
        assert!(before <= 5.0);
        let after = a.downbeat_at_or_after(5.0).unwrap();
        assert!(after >= 5.0);
    }
}
