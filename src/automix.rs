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

/// Bars a track must have been analysed for before its loudness profile is
/// worth reading as structure rather than noise.
const SECTION_BARS: usize = 8;

/// How much louder than the track's median bar a span must be to count as
/// its chorus. RMS amplitude, so this is a modest margin in decibels.
const CHORUS_PROMINENCE: f64 = 1.12;

/// The opening of a track that has been prepared but not yet played.
///
/// A player that preloads the next track can hand its opening over before
/// it starts, which is the only chance to measure a track the sink is not
/// playing yet.
#[derive(Clone, Debug)]
pub struct Probe {
    /// Interleaved samples at the player's sample rate.
    pub samples: Vec<f32>,
    /// Seconds into the track where the samples begin.
    pub position_seconds: f64,
}

/// A beat grid and the tempo it was tracked at.
#[derive(Clone, Debug)]
pub struct Analysis {
    grid: BeatGrid,
    /// Beats per minute, from the grid's committed tempo.
    pub bpm: f64,
    /// Seconds from the start of the track to its first beat.
    pub first_beat: f64,
    /// Seconds into the track where the collected audio began. The grid's
    /// positions are relative to that point, not to the track's start.
    pub offset_in_track: f64,
    /// Per-bar loudness of the analysed audio, for finding the chorus.
    bar_loudness: Vec<f64>,
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
        let grid = timestretch::detect_beat_grid(&mono, sample_rate);
        let mut analysis = Self::from_grid(grid)?;
        analysis.bar_loudness = analysis.measure_bar_loudness(samples, sample_rate);
        Some(analysis)
    }

    /// The loudness profile of the audio this analysis came from.
    pub fn bar_loudness(&self) -> &[f64] {
        &self.bar_loudness
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
            offset_in_track: 0.0,
            bar_loudness: Vec::new(),
            grid,
        })
    }

    /// Tells the analysis where in the track its audio was taken from, so
    /// its grid can be read in the track's own time.
    ///
    /// Automix only ever hears part of a track, and a listener may start
    /// anywhere in one, so the grid is anchored by the caller's offset
    /// rather than assumed to begin at zero.
    pub fn anchored_at(mut self, seconds: f64) -> Self {
        self.offset_in_track = seconds.max(0.0);
        self
    }

    /// Scores every bar by how loud it is relative to the rest of the track.
    ///
    /// A chorus or a drop is the loudest, most consistent part of a track,
    /// which is where a DJ brings the next one in. Loudness is measured per
    /// bar over the audio that was analysed and keyed by bar index, so the
    /// caller can pick the loudest span that has room for a transition.
    ///
    /// Sections cannot be detected from a few seconds of audio, so a track
    /// whose analysis covered too little returns nothing and the caller
    /// falls back to the outro.
    fn measure_bar_loudness(&self, samples: &[f32], sample_rate: u32) -> Vec<f64> {
        let channels = crate::vis::CHANNELS as usize;
        let bar = self.bar_seconds();
        if !(bar.is_finite() && bar > 0.0) {
            return Vec::new();
        }
        let frames = samples.len() / channels;
        let bar_frames = (bar * f64::from(sample_rate)) as usize;
        if bar_frames == 0 || frames < bar_frames * SECTION_BARS {
            return Vec::new();
        }
        samples
            .chunks(bar_frames * channels)
            .filter(|span| span.len() >= bar_frames * channels / 2)
            .map(|span| {
                let energy: f64 = span
                    .iter()
                    .map(|sample| f64::from(*sample) * f64::from(*sample))
                    .sum();
                (energy / span.len() as f64).sqrt()
            })
            .collect()
    }

    /// The loudest span of `bars` whole bars, as a start time in track time.
    ///
    /// This is where the track's chorus or drop sits, to the resolution of
    /// the bar grid: a DJ brings the next track in there rather than at the
    /// outro, so both tracks sound like songs while they overlap instead of
    /// one fading out.
    ///
    /// The span has to stand out from the rest of the track. Without that
    /// test a track of even loudness — a metronomic one, or any track
    /// without a quiet stretch — would match its own first bars and the exit
    /// would land wherever the analysis happened to begin.
    pub fn loudest_span(&self, loudness: &[f64], bars: usize) -> Option<f64> {
        if bars == 0 || loudness.len() < bars {
            return None;
        }
        let phase = self.downbeat_phase()?;
        let bar = self.bar_seconds();
        // Sum a sliding window so the choice is the loudest *span*, not the
        // loudest single bar, which would put the exit mid-chorus.
        let mut best_at = 0usize;
        let mut best = f64::NEG_INFINITY;
        let mut window: f64 = loudness[..bars].iter().sum();
        for start in 0..=(loudness.len() - bars) {
            if start > 0 {
                window += loudness[start + bars - 1] - loudness[start - 1];
            }
            if window > best {
                best = window;
                best_at = start;
            }
        }

        // A chorus is louder than the track's own middle. Comparing the
        // chosen span against the median bar keeps the test relative, so it
        // holds whether the track was mastered loud or quiet.
        let mut sorted = loudness.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        if median <= f64::MIN_POSITIVE || (best / bars as f64) < median * CHORUS_PROMINENCE {
            return None;
        }

        // The span ends `bars` in, so its exit is the last bar it covers.
        let exit_bar = best_at + bars - 1;
        Some(phase + exit_bar as f64 * bar)
    }

    /// The bar length in seconds.
    fn bar_seconds(&self) -> f64 {
        BEATS_PER_BAR * 60.0 / self.bpm
    }

    /// Downbeat positions in track time. The grid marks these by index;
    /// when it marks none, every fourth beat stands in.
    fn downbeats(&self) -> Vec<f64> {
        let rate = f64::from(self.grid.sample_rate);
        let marked: Vec<f64> = self
            .grid
            .downbeats
            .iter()
            .filter_map(|index| self.grid.beats.get(*index))
            .map(|position| self.offset_in_track + position / rate)
            .collect();
        if marked.len() >= 2 {
            return marked;
        }
        self.grid
            .beats
            .iter()
            .step_by(BEATS_PER_BAR as usize)
            .map(|position| self.offset_in_track + position / rate)
            .collect()
    }

    /// The first downbeat of the grid, in track time: the phase the bar
    /// repeats on.
    fn downbeat_phase(&self) -> Option<f64> {
        self.downbeats().first().copied()
    }

    /// The last downbeat at or before `seconds`.
    ///
    /// Extrapolates from the grid's first downbeat, because the audio that
    /// reaches the sink covers a window of the track rather than the whole
    /// of it, and the exit it must find usually lies outside that window.
    fn downbeat_at_or_before(&self, seconds: f64) -> Option<f64> {
        let phase = self.downbeat_phase()?;
        let bar = self.bar_seconds();
        if !(bar.is_finite() && bar > 0.0) || seconds < phase {
            return self
                .downbeats()
                .into_iter()
                .next()
                .filter(|_| seconds >= phase);
        }
        let bars = ((seconds - phase) / bar).floor();
        Some(phase + bars * bar)
    }

    /// The first downbeat at or after `seconds`, extrapolating likewise.
    pub fn downbeat_at_or_after(&self, seconds: f64) -> Option<f64> {
        let phase = self.downbeat_phase()?;
        let bar = self.bar_seconds();
        if !(bar.is_finite() && bar > 0.0) || seconds <= phase {
            return Some(phase);
        }
        let bars = ((seconds - phase) / bar).ceil();
        Some(phase + bars * bar)
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
    /// The rate the outgoing tail is played at, with its pitch held: the
    /// incoming tempo over the outgoing one, folded to the nearest octave.
    /// The outgoing track is the one stretched, because its beats are the
    /// ones that have to move onto the incoming track's grid before it fades
    /// away.
    pub tempo_ratio: f64,
}

/// Bars the overlap spans at the outgoing track's tempo.
pub fn bars_of(duration: Duration, bpm: f64) -> f64 {
    duration.as_secs_f64() * bpm / 60.0 / BEATS_PER_BAR
}

/// Plan a transition when only the outgoing track has been analysed.
///
/// The incoming track's grid may be unknown: it does not reach the sink, and
/// a probe of it may not have produced one. Without it the pair is not
/// stretched and the incoming track starts at its own beginning, but the
/// outgoing track still leaves on a downbeat instead of wherever its last
/// sample falls, and the overlap is still a whole number of bars.
pub fn plan_exit(from: &Analysis, out_duration: Duration) -> Option<Transition> {
    plan_exit_for(from, out_duration)
}

/// Plan an exit, preferring the track's loudest section when its structure
/// has been measured.
///
/// A DJ does not wait for the outro. The next track comes in over the
/// chorus or the drop, where both tracks sound like songs, so the exit is
/// taken from there when `bar_loudness` has a span that fits the overlap
/// and still leaves the outgoing track playing afterwards.
pub fn plan_exit_for(from: &Analysis, out_duration: Duration) -> Option<Transition> {
    plan_exit_matched(from, None, out_duration, 0.0)
}

/// Plan an exit knowing the incoming track's grid as well.
///
/// With the other grid in hand the pair can be tempo-matched: the outgoing
/// tail is played at the ratio between the two, and the transition also
/// starts the incoming track on a downbeat of its own. `incoming` must
/// already be anchored in its track's time.
///
/// `earliest` is where the play head is now, and no exit before it is worth
/// planning. That matters because the analysis covers a window of the track
/// rather than the whole of it: the loudest span it finds is the loudest part
/// *it heard*, which for a track played from the start is an early chorus.
/// Without this, the planner would keep proposing an exit hundreds of
/// seconds behind the play head and never arm anything.
///
/// Returns `None` when the two tempos are too far apart to stretch across,
/// so an unmatched pair falls back to a plain exit rather than a smear.
pub fn plan_exit_matched(
    from: &Analysis,
    incoming: Option<&Analysis>,
    out_duration: Duration,
    earliest: f64,
) -> Option<Transition> {
    // Match the tempos. A factor near 0.5 or 2.0 is the same groove at half
    // or double time, so fold those in before judging the gap.
    let tempo_ratio = match incoming {
        Some(to) => {
            let ratio = fold_octave(to.bpm / from.bpm);
            if (ratio - 1.0).abs() > MAX_TEMPO_DIFF {
                return None;
            }
            ratio
        }
        None => 1.0,
    };
    let beat_seconds = 60.0 / from.bpm;
    for bars in BAR_CHOICES {
        // `seconds` is how much of the outgoing track the overlap eats, and
        // `overlap` is how long that takes to play. The two differ once the
        // tail is stretched: at `tempo_ratio`, a wall-clock second of output
        // consumes `tempo_ratio` seconds of the outgoing material.
        let seconds = f64::from(bars) * BEATS_PER_BAR * beat_seconds;
        let overlap = match incoming {
            // Matched, the overlap is the same whole number of bars of the
            // incoming track. Because the ratio is the two tempos' quotient
            // and the outgoing tail is the one stretched, that leaves the
            // outgoing track traversing exactly `seconds` of its own bars —
            // both tracks cross the overlap having played the same count.
            Some(to) => f64::from(bars) * BEATS_PER_BAR * 60.0 / to.bpm,
            None => seconds,
        };
        let duration = Duration::from_secs_f64(overlap);
        if duration > MAX_TRANSITION || duration < MIN_TRANSITION {
            continue;
        }
        let slack = beat_seconds * BEATS_PER_BAR;
        let end_of_track = out_duration.as_secs_f64();

        // Where the chorus is, if the profile is deep enough to say. The
        // span's exit is used, and it has to leave a bar of music after it
        // so the overlap is not simply the track ending.
        let chorus_exit = from
            .loudest_span(from.bar_loudness(), bars as usize)
            .filter(|exit| *exit + seconds + slack <= end_of_track)
            .filter(|exit| *exit >= earliest);

        let latest_start = end_of_track - seconds - slack;
        if latest_start <= from.first_beat && chorus_exit.is_none() {
            continue;
        }

        let preferred = chorus_exit.or_else(|| from.downbeat_at_or_before(latest_start));
        // A track already past its own outro has nowhere left to fade from.
        let Some(fade_out_at) = preferred.filter(|exit| *exit >= earliest) else {
            continue;
        };
        return Some(Transition {
            fade_out_at,
            // The incoming track starts on one of its own downbeats, so both
            // tracks land their bar together instead of one sliding under
            // the other.
            fade_in_at: incoming
                .and_then(|to| to.downbeat_at_or_after(0.0))
                .unwrap_or(0.0),
            duration,
            tempo_ratio,
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
    plan_exit_matched(from, Some(to), out_duration, 0.0)
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
        // The exit lands on the bar lattice the grid defines, which may be
        // extrapolated past the audio that was actually analysed.
        let phase = a.downbeat_phase().expect("a grid has a phase");
        let bar = a.bar_seconds();
        let bars = (planned.fade_out_at - phase) / bar;
        assert!(
            (bars - bars.round()).abs() < 1e-6,
            "exit {} is not a whole bar from the phase {phase} (spacing {bar})",
            planned.fade_out_at
        );
        assert!(planned.fade_out_at + planned.duration.as_secs_f64() <= 40.0);
    }

    #[test]
    fn an_unmatched_track_too_short_to_fade_is_refused() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        assert!(plan_exit(&a, Duration::from_secs(2)).is_none());
    }

    /// The bug this covers: automix hears a window of a track, not the whole
    /// of it, so a grid taken from 100 seconds in has its beats near zero.
    /// Read without the offset, every exit it computes lands in the wrong
    /// place and is discarded as already past.
    #[test]
    fn a_grid_anchored_mid_track_still_plans_an_exit_near_the_end() {
        let a = Analysis::of(&click_track(128.0, 60.0, 44_100), 44_100)
            .unwrap()
            .anchored_at(100.0);
        let planned = plan_exit(&a, Duration::from_secs(200)).expect("a long track has room");
        assert!(
            planned.fade_out_at > 180.0,
            "exit at {} should sit near the end of a 200s track",
            planned.fade_out_at
        );
        assert!(planned.fade_out_at + planned.duration.as_secs_f64() <= 200.0);
        // The phase carries the offset, so the bar lattice lines up with the
        // track rather than with the moment collection happened to start.
        assert!(a.downbeat_phase().unwrap() >= 100.0);
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

    /// The point of matching the tempo: across the overlap both tracks must
    /// play the same number of bars, so every beat of one lands on a beat of
    /// the other. If the overlap were measured in the outgoing track's bars
    /// the stretched tail would cross a different count than the incoming
    /// track, and the two would drift apart by the end of the fade.
    #[test]
    fn a_matched_pair_crosses_the_same_bars_on_both_decks() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        let b = Analysis::of(&click_track(130.0, 40.0, 44_100), 44_100).unwrap();
        let planned = plan(&a, &b, Duration::from_secs(40)).expect("matched tempos mix");

        let overlap = planned.duration.as_secs_f64();
        // The incoming track plays `overlap` seconds at its own tempo.
        let incoming_bars = overlap * b.bpm / 60.0 / BEATS_PER_BAR;
        // The outgoing tail is played at the ratio, so it crosses this much
        // of its own material.
        let outgoing_material = overlap * planned.tempo_ratio;
        let outgoing_bars = outgoing_material * a.bpm / 60.0 / BEATS_PER_BAR;

        assert!(
            (incoming_bars - incoming_bars.round()).abs() < 1e-6,
            "the overlap is not a whole number of incoming bars: {incoming_bars}"
        );
        assert!(
            (incoming_bars - outgoing_bars).abs() < 1e-6,
            "the decks cross different bar counts: incoming {incoming_bars}, \
             outgoing {outgoing_bars}"
        );
        // The ratio comes from the grids the tracker measured, which are near
        // but not exactly the tempos the clicks were generated at.
        let expected = fold_octave(b.bpm / a.bpm);
        assert!(
            (planned.tempo_ratio - expected).abs() < 1e-9,
            "the ratio must be the two measured tempos' quotient"
        );
        assert!(planned.tempo_ratio > 1.0, "the faster incoming track speeds the tail up");
    }

    /// The outgoing tail must not run out before the fade does: the material
    /// the overlap needs has to fit between the exit and the track's end.
    #[test]
    fn the_matched_overlap_leaves_the_outgoing_track_playing() {
        let a = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).unwrap();
        let b = Analysis::of(&click_track(136.0, 40.0, 44_100), 44_100).unwrap();
        let planned = plan(&a, &b, Duration::from_secs(40)).expect("matched tempos mix");
        // A faster incoming track makes the tail play faster than its own
        // tempo, which is the case that can run off the end of the material:
        // it eats more of the track than the overlap lasts.
        assert!(planned.tempo_ratio > 1.0, "the tail is sped up");
        let consumed = planned.fade_out_at + planned.duration.as_secs_f64() * planned.tempo_ratio;
        assert!(
            consumed <= 40.0,
            "the tail needs {consumed:.2}s of a 40s track"
        );
    }

    /// The exit must still be planned when the play head is inside the
    /// window the analysis covered, which is the normal case near a real
    /// track's end.
    #[test]
    fn an_exit_ahead_of_the_play_head_is_planned() {
        let a = Analysis::of(&click_track(128.0, 60.0, 44_100), 44_100).unwrap();
        let planned = plan_exit_matched(&a, None, Duration::from_secs(300), 100.0)
            .expect("a track with room ahead still plans");
        assert!(planned.fade_out_at >= 100.0);
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
