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
use timestretch::engine::{Engine, EngineConfig, EngineProfile};

/// The longest overlap the official clients allow, and the ceiling our
/// planner keeps to.
pub const MAX_TRANSITION: Duration = Duration::from_secs(12);

/// The widest gap a folded pair can have, and so the most the two decks ever
/// have to share between them.
///
/// The octave fold picks the nearest power of two within two octaves of the
/// raw ratio, and any two candidates differ by a factor of two — so the worst
/// case is the ratio equally far from both, at `4/3`. Folding that down gives
/// `2/3`, a gap of one third, and no pair inside the fold's two-octave reach
/// can be wider. That is why no pair has to be refused on tempo.
///
/// The precondition is the two-octave reach: a pair 4× apart is at the edge
/// (60 against 240 BPM), and anything past it is a tempo ratio no two real
/// tracks have.
pub const MAX_FOLDED_GAP: f64 = 1.0 / 3.0;

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

/// How much the top band must rise against the middle one, relative to the
/// track's own median, for a stretch to count as a chorus.
///
/// Measured on synthetic verse/chorus material the ratio nearly triples
/// across the boundary, so this sits well clear of noise while still
/// catching a chorus that is mixed subtly.
const LOUD_PROMINENCE: f64 = 1.35;

/// Seconds the band ratio is smoothed over before it is thresholded.
const LOUD_SMOOTH_SECONDS: f64 = 2.0;

/// Shortest stretch that can be a chorus.
const MIN_LOUD_SECONDS: f64 = 6.0;

/// Gaps shorter than this inside a loud stretch are bridged, so a chorus
/// that dips for a bar is one section rather than two.
const LOUD_MERGE_GAP_SECONDS: f64 = 3.0;

/// Fewest envelope readings worth looking at; below this there is no
/// structure to find and the answer would be noise.
const MIN_SECTION_READINGS: usize = 200;

/// A simple centred moving average, with the ends left as they are.
fn moving_average(values: &[f64], window: usize) -> Vec<f64> {
    if window <= 1 || values.len() < window {
        return values.to_vec();
    }
    let half = window / 2;
    (0..values.len())
        .map(|index| {
            let start = index.saturating_sub(half);
            let end = (index + half + 1).min(values.len());
            values[start..end].iter().sum::<f64>() / (end - start) as f64
        })
        .collect()
}

/// Joins sections separated by a gap shorter than `gap`.
fn merge_close_sections(sections: &mut Vec<LoudSection>, gap: f64) {
    if sections.len() < 2 {
        return;
    }
    let mut merged: Vec<LoudSection> = Vec::with_capacity(sections.len());
    for section in sections.drain(..) {
        match merged.last_mut() {
            Some(previous) if section.start - previous.end <= gap => {
                previous.end = previous.end.max(section.end);
            }
            _ => merged.push(section),
        }
    }
    *sections = merged;
}

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
    /// Where the track's loud sections are, in track time, when they could
    /// be told from the rest by how the signal is balanced across bands.
    ///
    /// A transition wants both edges of one of these: it leaves just after a
    /// chorus ends, and brings the next track in just before one begins.
    /// Loudness alone cannot find them, because a chorus is not merely
    /// louder — it is balanced differently.
    loud_sections: Vec<LoudSection>,
}

/// A stretch of a track that stands out as its chorus or drop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudSection {
    /// Seconds into the track where it begins.
    pub start: f64,
    /// Seconds into the track where it ends.
    pub end: f64,
}

impl Analysis {
    /// Beat-track interleaved samples.
    ///
    /// `samples` is what the decoder hands the sink: interleaved, two
    /// channels. `timestretch` wants the mono mid downmix, so this folds the
    /// channels first.
    pub fn of(samples: &[f32], sample_rate: u32) -> Option<Self> {
        Self::of_with_envelope(samples, sample_rate, &[])
    }

    /// Beat-track interleaved samples, taking the track's structure from a
    /// whole-track energy envelope when one is available.
    ///
    /// The samples only reach as far as the analysis window, so on their own
    /// they can only say where the loudest part *of that window* is. The
    /// envelope covers the whole track, so it is what knows where the last
    /// chorus is — which is the thing automix has to leave after.
    pub fn of_with_envelope(
        samples: &[f32],
        sample_rate: u32,
        envelope: &[f64],
    ) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mono = timestretch::downmix_to_mid(samples, crate::vis::CHANNELS as usize);
        let grid = timestretch::detect_beat_grid(&mono, sample_rate);
        let mut analysis = Self::from_grid(grid)?;
        if envelope.is_empty() {
            analysis.bar_loudness = analysis.measure_bar_loudness(samples, sample_rate);
        } else {
            analysis.bar_loudness = analysis.measure_envelope_loudness(envelope);
        }
        Some(analysis)
    }

    /// Seconds into the track the analysed audio reaches, or `None` when no
    /// loudness profile was measured.
    ///
    /// Automix hears a window of the track rather than the whole of it, and
    /// this is how far that window got. Anything the planner wants to say
    /// about the track's structure — where its chorus is — only holds for
    /// the part that was actually heard.
    pub fn analysed_until(&self) -> Option<f64> {
        if self.bar_loudness.is_empty() {
            return None;
        }
        Some(self.offset_in_track + self.bar_loudness.len() as f64 * self.bar_seconds())
    }

    /// Re-reads the track's structure from a fresh energy envelope, keeping
    /// the beat grid it already has.
    ///
    /// The grid comes from a bounded window and does not change; where the
    /// sections are only becomes clear as more of the track is heard. This
    /// is how the planner learns about a later chorus without paying for
    /// another beat-tracking pass.
    pub fn refresh_structure(&mut self, envelope: &[f64]) {
        if envelope.is_empty() {
            return;
        }
        self.bar_loudness = self.measure_envelope_loudness(envelope);
    }

    /// Re-reads the structure from the per-band readings, which is what can
    /// tell a chorus from a verse.
    pub fn refresh_bands(&mut self, bands: &[Vec<f64>; crate::automix_track::NUM_BANDS]) {
        self.loud_sections = self.find_loud_sections(bands);
    }

    /// Where the track's choruses are, in track time.
    ///
    /// The readings are taken in the audio's own time and only later
    /// anchored, so the offset is applied here rather than being frozen in
    /// at detection time.
    pub fn loud_sections(&self) -> Vec<LoudSection> {
        self.loud_sections
            .iter()
            .map(|section| LoudSection {
                start: self.offset_in_track + section.start,
                end: self.offset_in_track + section.end,
            })
            .collect()
    }

    /// The last chorus that has already finished by `now`, if there is one.
    ///
    /// A transition leaves just after a chorus rather than in the middle of
    /// one, so the caller wants the end of the latest section that is over.
    pub fn chorus_ended_by(&self, now: f64) -> Option<LoudSection> {
        self.loud_sections()
            .into_iter()
            .filter(|section| section.end <= now)
            .next_back()
    }

    /// The first chorus that starts at or after `from`.
    ///
    /// The incoming track is brought in ahead of this, so its chorus lands
    /// after the overlap rather than inside it.
    pub fn chorus_starting_after(&self, from: f64) -> Option<LoudSection> {
        self.loud_sections()
            .into_iter()
            .find(|section| section.start >= from)
    }

    /// Finds the stretches that stand out by band balance rather than level.
    ///
    /// A chorus carries more kick and more cymbals than a verse while the
    /// midrange makes room for the vocal, so the ratio of the top band to
    /// the middle one rises sharply across it and falls back after. That
    /// ratio is smoothed and thresholded against the track's own median, so
    /// it holds for a loud master and a quiet one alike.
    fn find_loud_sections(
        &self,
        bands: &[Vec<f64>; crate::automix_track::NUM_BANDS],
    ) -> Vec<LoudSection> {
        let [low, mid, high] = bands;
        let readings = mid.len().min(high.len()).min(low.len());
        if readings < MIN_SECTION_READINGS {
            return Vec::new();
        }
        let hop = crate::automix_track::ENERGY_HOP_SECONDS;
        // The ratio the chorus pushes up, smoothed over a couple of seconds
        // so a single busy bar cannot open a section of its own.
        let raw: Vec<f64> = (0..readings)
            .map(|index| high[index] / mid[index].max(f64::MIN_POSITIVE))
            .collect();
        let window = (LOUD_SMOOTH_SECONDS / hop).round().max(1.0) as usize;
        let smoothed = moving_average(&raw, window);
        let mut sorted = smoothed.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        if median <= f64::MIN_POSITIVE {
            return Vec::new();
        }
        let threshold = median * LOUD_PROMINENCE;
        // Walk the curve and keep the runs above the threshold, dropping
        // ones too short to be a section. Level is not consulted at all:
        // this is about balance, so a quiet chorus still counts.
        //
        // The sections are recorded in the audio's own time, not the
        // track's: the reading is made before the caller says where in the
        // track that audio came from, so baking the offset in here would
        // freeze it at zero and every later anchor would be ignored.
        let mut sections = Vec::new();
        let mut run: Option<usize> = None;
        for index in 0..=smoothed.len() {
            let above = index < smoothed.len() && smoothed[index] >= threshold;
            match (above, run) {
                (true, None) => run = Some(index),
                (false, Some(start)) => {
                    let seconds = (index - start) as f64 * hop;
                    if seconds >= MIN_LOUD_SECONDS {
                        sections.push(LoudSection {
                            start: start as f64 * hop,
                            end: index as f64 * hop,
                        });
                    }
                    run = None;
                }
                _ => {}
            }
        }
        // Merge sections separated by a gap too short to be a real break,
        // which happens when the ratio dips for a bar mid-chorus.
        merge_close_sections(&mut sections, LOUD_MERGE_GAP_SECONDS);
        sections
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
            loud_sections: Vec::new(),
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

    /// Per-bar loudness from a whole-track energy envelope.
    ///
    /// The envelope's readings are one per hop, in track order, so bar `n`
    /// covers hops `[n * bar, (n + 1) * bar)`. A bar's loudness is the mean
    /// of its hops rather than a peak: the sections a DJ mixes over are the
    /// ones that stay loud, not the ones with a single hit in them.
    fn measure_envelope_loudness(&self, envelope: &[f64]) -> Vec<f64> {
        let bar = self.bar_seconds();
        if !(bar.is_finite() && bar > 0.0) {
            return Vec::new();
        }
        let hops_per_bar = (bar / crate::automix_track::ENERGY_HOP_SECONDS).round() as usize;
        if hops_per_bar == 0 {
            return Vec::new();
        }
        envelope
            .chunks(hops_per_bar)
            .filter(|span| span.len() * 2 >= hops_per_bar)
            .map(|span| span.iter().sum::<f64>() / span.len() as f64)
            .collect()
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

    /// The bar length in seconds. Public because a caller reading the
    /// loudness profile has to know how much track each entry covers.
    pub fn bar_seconds(&self) -> f64 {
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

/// Where each deck goes and how far the pair is stretched, for one pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    /// Seconds into the outgoing track where its fade starts.
    pub fade_out_at: f64,
    /// Seconds into the incoming track where its fade starts.
    pub fade_in_at: f64,
    /// How long the two overlap.
    pub duration: Duration,
    /// How much faster the incoming track's groove is than the outgoing
    /// one's, folded to the nearest octave.
    ///
    /// Both decks move onto a shared tempo across the overlap, because the
    /// two are locked to each other: if one deck plays a beat, the other has
    /// to be playing a beat at the same moment, so their playback rates can
    /// never be set independently. Writing `c(t)` for the shared tempo as a
    /// fraction of the outgoing track's own, and `r` for this ratio, the
    /// rates are `c(t)` on the outgoing deck and `c(t) / r` on the incoming
    /// one — a constant quotient of `r`, which is what keeps them locked.
    ///
    /// `c` runs from `1.0` to `r` over the overlap: it starts on the
    /// outgoing track's tempo, so the track the listener is already hearing
    /// never changes speed, and it ends on the incoming track's, so that one
    /// arrives at its own tempo and stays there. The stretch therefore
    /// migrates from the incoming deck to the outgoing one, and each deck is
    /// at its natural rate exactly when it is loudest.
    pub tempo_ratio: f64,
}

/// The rate each deck plays at, `p` of the way through an overlap.
///
/// The two decks are locked to each other: if one is on a beat, the other has
/// to be on a beat at the same instant, so their playback rates can never be
/// chosen independently. Writing `r` for the ratio between the tracks, the
/// outgoing deck at `rate` forces the incoming one to `rate / r` — a
/// quotient of exactly `r` at every instant, which is what keeps them locked.
///
/// `r^p` runs from `1.0` to `r` across the overlap. That choice makes each
/// deck natural exactly when it owns the mix: the outgoing deck starts at its
/// own tempo, so the track the listener is already hearing never lurches
/// when the transition opens, and by the end it has handed over and the
/// incoming deck is back at `r / r = 1.0` — its own tempo — just as it
/// becomes the loud one. The stretch migrates from one deck to the other
/// while both stay locked.
///
/// A geometric run is the only shape that works: two straight lines would
/// keep the quotient constant only if they were the same line, which they
/// cannot be while one starts at 1.0 and the other ends there.
pub fn curve_rates(ratio: f64, progress: f64) -> (f64, f64) {
    let progress = progress.clamp(0.0, 1.0);
    let outgoing = ratio.powf(progress);
    (outgoing, outgoing / ratio)
}

/// How far apart the rate retargets are placed, in output frames.
///
/// The engine takes a rate per block and ramps to it over the block, so the
/// curve is a staircase in practice. Placing the steps this far apart keeps
/// the schedule shallow enough to stay under the engine's pending-retarget
/// limit, which is the one thing a long curve can trip: the calls are made
/// from the same loop that renders, so the steps land exactly where they are
/// asked for.
const CURVE_STEP_FRAMES: u64 = 1_024;

/// Renders `frames` of the incoming track under the curve, ready to be mixed.
///
/// `source` is the incoming track's audio from where the overlap starts,
/// interleaved at `channels`. The output is exactly `frames` frames: the
/// overlap is a fixed length of wall clock, and the rate decides how much of
/// the track is consumed to fill it.
///
/// This drives the same engine the live decks use, rendering ahead of time
/// rather than against an audio callback. That is deliberate. The live path
/// has to be fed from a decode loop that is also driving the sink, so a deck
/// that wants more source than has been decoded underruns — which is exactly
/// how a live version of this broke. Offline there is no deadline: the source
/// is pushed until there is room, the render is pulled until the overlap is
/// full, and the two cannot outrun each other. Pitch is held by the same
/// keylock stage either way.
///
/// Returns an empty vector when the curve cannot be rendered, so the caller
/// can fall back to a plain crossfade rather than mix a hole.
pub fn render_curve(
    source: &[f32],
    ratio: f64,
    frames: usize,
    channels: usize,
    sample_rate: u32,
) -> Vec<f32> {
    if frames == 0 || !(1..=8).contains(&channels) || !(ratio.is_finite() && ratio > 0.0) {
        return Vec::new();
    }
    if source.is_empty() {
        return Vec::new();
    }
    // The incoming deck starts away from its own tempo and ends on it, which
    // is `curve_rates` at the two ends of the overlap.
    let (_, first) = curve_rates(ratio, 0.0);
    let Ok(handles) = Engine::build(EngineConfig {
        sample_rate,
        channels,
        profile: EngineProfile::Keylock,
        initial_tempo_rate: first,
        max_block_frames: CURVE_STEP_FRAMES as usize,
        ..EngineConfig::default()
    }) else {
        return Vec::new();
    };
    let (controller, mut processor, mut source_ring) =
        (handles.controller, handles.processor, handles.source);
    source_ring.set_track_position(0);
    let latency = processor.pipeline_latency_frames();

    // The engine delays the source by its pipeline, so it has to be fed that
    // much beyond the last real frame for the final windows to come out.
    // Zero source after the music, as the batch path does: it is lookahead,
    // not audio the listener will hear past the overlap.
    let flush_frames = latency + CURVE_STEP_FRAMES as usize;
    let flush: Vec<f32> = vec![0.0; flush_frames * channels];

    let mut feed = 0usize;
    let mut flush_fed = 0usize;
    let mut finished = false;
    let mut block = vec![0.0f32; CURVE_STEP_FRAMES as usize * channels];
    let total_needed = (frames + latency) * channels;
    let mut collected: Vec<f32> = Vec::with_capacity(total_needed + block.len());

    while collected.len() < total_needed {
        // Where this block begins, as a fraction of the overlap. The step is
        // scheduled before the block that will play it, so it is already in
        // effect when that block renders.
        let done = collected.len() / channels;
        let (_, incoming) = curve_rates(ratio, done as f64 / frames as f64);
        controller.set_tempo_rate_at(incoming, (done + latency) as u64);

        while feed < source.len() && source_ring.free_frames() > 0 {
            let end = (feed + 8_192 * channels).min(source.len());
            feed += source_ring.push(&source[feed..end]) * channels;
        }
        if feed >= source.len() {
            while flush_fed < flush.len() && source_ring.free_frames() > 0 {
                flush_fed += source_ring.push(&flush[flush_fed..]) * channels;
            }
            if flush_fed >= flush.len() && !finished {
                finished = source_ring.finish();
            }
        }
        if finished && source_ring.occupied_frames() == 0 {
            // The track ran out before the overlap did. Returning what was
            // rendered lets the caller decide; a partial buffer mixed under
            // the full fade would leave the second half of the overlap with
            // one deck.
            break;
        }
        let underruns = controller.underrun_frames();
        processor.process(&mut block);
        if controller.underrun_frames() > underruns {
            // Nothing left to render: the source is spent.
            break;
        }
        collected.extend_from_slice(&block);
    }

    // The pipeline's fill is not audio the listener is meant to hear, and it
    // sits at the head of the render. Dropping it structurally is what makes
    // the overlap line up with the outgoing deck's own count.
    if collected.len() < latency * channels {
        return Vec::new();
    }
    collected.drain(..latency * channels);
    collected.truncate(frames * channels);
    collected
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
/// Returns `None` only when the outgoing track has no room left to fade in,
/// which is a property of that track alone; the pair's tempos cannot make it
/// fail, because [`fold_octave`] always brings them within reach.
pub fn plan_exit_matched(
    from: &Analysis,
    incoming: Option<&Analysis>,
    out_duration: Duration,
    earliest: f64,
) -> Option<Transition> {
    // Match the tempos. A factor near 0.5 or 2.0 is the same groove at half
    // or double time, so fold those in: what is left is a stretch both decks
    // can share, whatever the pair.
    let tempo_ratio = incoming.map_or(1.0, |to| fold_octave(to.bpm / from.bpm));
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

        // Leave from the end of the last chorus that has already finished.
        // A transition should not run through the middle of one, and it
        // should not wait for the outro either: the moment a chorus lands
        // back into a verse is where the two tracks blend most easily.
        //
        // The chorus has to be one the analysis actually heard, so the
        // search is bounded by `earliest` — the play head — rather than
        // picking a section the track has already gone past.
        let chorus_exit = from
            .chorus_ended_by(end_of_track - slack)
            .filter(|section| section.end >= earliest)
            .map(|section| section.end)
            // Snap onto the grid, so the overlap still lands on a bar even
            // though the section boundary is measured, not tracked.
            .and_then(|end| {
                from.downbeat_at_or_after(end)
                    .or_else(|| from.downbeat_at_or_before(end))
            })
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
        // Bring the incoming track in so its own chorus lands after the
        // overlap rather than inside it: the arrival is placed just ahead of
        // the next chorus, which is the moment the pair sounds like songs
        // together instead of one fading under the other.
        let fade_in_at = incoming
            .and_then(|to| {
                // The overlap is measured in the incoming track's time, so
                // its chorus has to start at least that far in.
                to.chorus_starting_after(overlap * 0.5)
                    .map(|section| section.start - overlap)
                    .or_else(|| to.downbeat_at_or_after(0.0))
            })
            .unwrap_or(0.0)
            .max(0.0);
        return Some(Transition {
            fade_out_at,
            fade_in_at,
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
/// A pair 1.9× apart is the same groove at double time and folds to 0.95.
/// This is what the server does under
/// `auto_transition_allow_octave_bpm_correction`.
///
/// Folding is also what makes every pair mixable. For any ratio the nearest
/// power of two is within a factor of `sqrt(2)`, so the folded ratio lands in
/// `[1/sqrt(2), sqrt(2)]` — a gap of at most 29.3%, and it is exactly that at
/// the worst case of `sqrt(2)`. Half of that on each deck is under 16%, well
/// inside what keylock holds, so no tempo gap is ever past stretching and no
/// caller has to refuse one.
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

    /// Clicks at `bpm`, with the span between `loud_from` and `loud_to`
    /// seconds several times louder, as a chorus is in a real track.
    fn click_track_with_chorus(
        bpm: f64,
        seconds: f64,
        rate: u32,
        loud_from: f64,
        loud_to: f64,
    ) -> Vec<f32> {
        let mut samples = click_track(bpm, seconds, rate);
        let channels = crate::vis::CHANNELS as usize;
        let from = (loud_from * f64::from(rate)) as usize * channels;
        let to = ((loud_to * f64::from(rate)) as usize * channels).min(samples.len());
        for sample in &mut samples[from..to] {
            *sample *= 6.0;
        }
        samples
    }

    /// How many times the audio crosses its own half level going up.
    ///
    /// `click_track` puts one sharp pulse per beat against silence, so a rise
    /// through half of the peak is one beat and the count is how many the
    /// buffer holds. Enough to tell a curve from a constant render, without
    /// needing a tracker.
    fn count_onsets(samples: &[f32], channels: usize) -> usize {
        let peak = samples.iter().fold(0.0f32, |worst, sample| worst.max(sample.abs()));
        if peak <= 0.0 {
            return 0;
        }
        let half = peak / 2.0;
        let mono: Vec<f32> = samples.chunks(channels).map(|frame| frame[0]).collect();
        mono.windows(2)
            .filter(|pair| pair[0] < half && pair[1] >= half)
            .count()
    }


    /// The lock the whole curve exists for: whatever the two decks are doing,
    /// their rates keep a constant quotient, or their beats would drift apart
    /// over the bars the overlap lasts.
    #[test]
    fn the_curve_keeps_the_two_decks_locked_at_every_point() {
        let ratio = 1.2652;
        for step in 0..=1000 {
            let (outgoing, incoming) = curve_rates(ratio, f64::from(step) / 1000.0);
            assert!(
                (outgoing / incoming - ratio).abs() < 1e-9,
                "at step {step} the decks drifted to a quotient of {}",
                outgoing / incoming
            );
        }
    }

    /// Each deck has to be at its own tempo at the end it owns, or the track
    /// the listener is hearing would change speed at the moment it is loudest.
    #[test]
    fn the_curve_leaves_each_deck_natural_when_it_owns_the_mix() {
        let ratio = 1.2652;
        let (outgoing, incoming) = curve_rates(ratio, 0.0);
        assert!(
            (outgoing - 1.0).abs() < 1e-9,
            "the outgoing deck must open at its own tempo, got {outgoing}"
        );
        assert!((incoming - 1.0 / ratio).abs() < 1e-9);

        let (outgoing, incoming) = curve_rates(ratio, 1.0);
        assert!((outgoing - ratio).abs() < 1e-9);
        assert!(
            (incoming - 1.0).abs() < 1e-9,
            "the incoming deck must land on its own tempo, got {incoming}"
        );
    }

    /// The stretch has to sit on the deck that is quietest. That is what
    /// makes a shared curve inaudible where a one-deck stretch is not.
    #[test]
    fn the_curve_puts_the_stretch_on_the_deck_that_is_fading() {
        let ratio = 1.2652;
        // The outgoing deck only ever leaves its own tempo, and the incoming
        // deck only ever comes towards it.
        let mut previous = 0.0;
        for step in 0..=100 {
            let (outgoing, _) = curve_rates(ratio, f64::from(step) / 100.0);
            let stretch = (outgoing - 1.0).abs();
            assert!(
                stretch >= previous - 1e-9,
                "the outgoing deck came back towards its own tempo mid-fade"
            );
            previous = stretch;
        }
        let mut previous = f64::INFINITY;
        for step in 0..=100 {
            let (_, incoming) = curve_rates(ratio, f64::from(step) / 100.0);
            let stretch = (incoming - 1.0).abs();
            assert!(
                stretch <= previous + 1e-9,
                "the incoming deck drifted further off as it grew louder"
            );
            previous = stretch;
        }
    }

    /// The rendered overlap has to be exactly as long as the transition: it
    /// is mixed under a fixed-length fade, so a short buffer would leave a
    /// hole at the end of the overlap.
    #[test]
    fn a_rendered_curve_fills_the_overlap_exactly() {
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let frames = rate as usize * 4;
        let source = click_track(128.0, 20.0, rate);
        let rendered = render_curve(&source, 1.2652, frames, channels, rate);
        assert_eq!(
            rendered.len(),
            frames * channels,
            "the overlap is a fixed length of output"
        );
    }

    /// Keylock: stretching the incoming track onto the shared tempo must not
    /// move its pitch. A varispeed stretch would drag a 220 Hz tone along
    /// with the tempo, which is what makes a one-deck transition sound wrong.
    #[test]
    fn a_rendered_curve_holds_its_pitch() {
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let hz = 220.0f64;
        let seconds = 12.0;
        let mut source: Vec<f32> = Vec::with_capacity((seconds * f64::from(rate)) as usize * channels);
        for frame in 0..(seconds * f64::from(rate)) as usize {
            let value = (2.0 * std::f32::consts::PI * hz as f32 * frame as f32 / rate as f32).sin() * 0.5;
            for _ in 0..channels {
                source.push(value);
            }
        }
        let frames = rate as usize * 8;
        let rendered = render_curve(&source, 1.2652, frames, channels, rate);
        assert!(!rendered.is_empty(), "the curve rendered nothing");

        // Count zero crossings over the steady middle, away from whatever the
        // first and last pieces do.
        let frames_out = rendered.len() / channels;
        let start = frames_out / 4;
        let end = frames_out * 3 / 4;
        let mono: Vec<f32> = rendered[start * channels..end * channels]
            .chunks(channels)
            .map(|frame| frame[0])
            .collect();
        let crossings = mono
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        let heard = crossings as f64 * f64::from(rate) / (2.0 * mono.len() as f64);
        assert!(
            (heard - hz).abs() < 10.0,
            "keylock moved the pitch: heard {heard:.1} Hz for a {hz:.0} Hz tone"
        );
    }

    /// The render has to line up with the outgoing deck's clock, because the
    /// two are mixed frame for frame. The engine holds a pipeline of its own
    /// and its fill sits at the head of the render, so a trim that is off by
    /// even a little would slide the whole overlap.
    ///
    /// Checked at a ratio of 1.0, where the beats must land where they do in
    /// the source. The engine still runs the keylock stages at that rate, so
    /// the samples are not identical — but the beats are unmoved.
    #[test]
    fn a_rendered_curve_keeps_the_beats_where_the_track_has_them() {
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let frames = rate as usize * 4;
        let source = click_track(128.0, 60.0, rate);
        let rendered = render_curve(&source, 1.0, frames, channels, rate);
        assert_eq!(rendered.len(), frames * channels);

        // Rising edges through the half level, as frame indices.
        let edges = |samples: &[f32]| -> Vec<usize> {
            let peak = samples
                .iter()
                .fold(0.0f32, |worst, sample| worst.max(sample.abs()));
            let half = peak / 2.0;
            let mono: Vec<f32> = samples.chunks(channels).map(|frame| frame[0]).collect();
            mono.windows(2)
                .enumerate()
                .filter(|(_, pair)| pair[0] < half && pair[1] >= half)
                .map(|(index, _)| index)
                .collect()
        };
        // The same span of the track, so the two counts are comparable.
        let opening = &source[..frames * channels];
        let from_source = edges(opening);
        let from_render = edges(&rendered);
        assert!(from_source.len() >= 4, "the fixture produced no beats");
        assert_eq!(
            from_source.len(),
            from_render.len(),
            "the render changed how many beats the overlap carries"
        );
        // Every beat within a millisecond of where the track has it: the
        // keylock filters, so an edge is not sample-exact, but a pipeline
        // trim that was wrong would show up as a drift, not a jitter.
        for (index, (before, after)) in from_source.iter().zip(&from_render).enumerate() {
            let drift = (*before as i64 - *after as i64).abs();
            assert!(
                drift < 44,
                "beat {index} moved {drift} frames ({:.1} ms) between the track and the render",
                drift as f64 / 44.1
            );
        }
    }

    /// Nothing to render must produce nothing, so the caller can fall back
    /// instead of mixing a hole into the overlap.
    #[test]
    fn an_empty_source_renders_nothing() {
        assert!(render_curve(&[], 1.2, 44_100, 2, 44_100).is_empty());
        assert!(render_curve(&[0.0; 8], 1.2, 0, 2, 44_100).is_empty());
        assert!(render_curve(&[0.0; 8], f64::NAN, 100, 2, 44_100).is_empty());
    }

    /// The rendered overlap has to walk the incoming track at the curve's
    /// own tempo, not at a constant one. The observable is how much of the
    /// track passes: at `ratio^(p-1)` the mean rate is the integral
    /// `(ratio - 1) / (ratio · ln ratio)`, which for this pair is about 0.891
    /// — so a four-second overlap must contain about 0.891 × 4 seconds of the
    /// track's own beats, not 4 and not the geometric middle's 1.12 × 4.
    #[test]
    fn a_rendered_curve_walks_the_track_at_the_curve_tempo() {
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let ratio = 1.2652;
        let frames = rate as usize * 4;
        let source = click_track(128.0, 60.0, rate);

        let rendered = render_curve(&source, ratio, frames, channels, rate);
        assert_eq!(rendered.len(), frames * channels);

        let beats = count_onsets(&rendered, channels);
        // What the curve says should pass, against what a constant render
        // would have given.
        let curve_seconds = (ratio - 1.0) / (ratio * ratio.ln()) * 4.0;
        let flat_seconds = 4.0;
        let beat = 60.0 / 128.0;
        let expected = curve_seconds / beat;
        let flat = flat_seconds / beat;

        assert!(
            (beats as f64 - expected).abs() < 1.5,
            "the overlap carried {beats} beats; the curve predicts {expected:.1}"
        );
        // And it must be distinguishable from a constant-rate render, or
        // this would pass for the wrong reason.
        assert!(
            (beats as f64 - flat).abs() > 1.0,
            "{beats} beats is also what a flat render gives ({flat:.1}), so nothing was proven"
        );
    }


    /// The bug this covers: the sample window only reaches the first 60
    /// seconds, so on a long track the loudest part *it* holds is an early
    /// chorus. A whole-track energy envelope knows about the later one, and
    /// the planner must leave after that instead.
    #[test]
    fn the_envelope_finds_a_chorus_the_sample_window_never_saw() {
        // A track whose chorus is at 150s, far past the 60s window.
        let samples = click_track_with_chorus(128.0, 200.0, 44_100, 150.0, 175.0);
        let rate = 44_100;
        let channels = crate::vis::CHANNELS as usize;

        // What the collector keeps: the opening only.
        let window = (crate::automix_track::ANALYSED_SECONDS * f64::from(rate)) as usize * channels;
        let opening = &samples[..window.min(samples.len())];
        let from_window = Analysis::of(opening, rate).expect("the opening is analysable");

        // What the envelope covers: the whole track.
        let envelope = crate::automix_track::envelope_of(&samples);
        let from_envelope =
            Analysis::of_with_envelope(opening, rate, &envelope).expect("analysable");

        let bars = 4usize;
        // The window's own view cannot see the late chorus, so its loudest
        // span is early — this is the trap the planner used to fall into.
        let window_span = from_window.loudest_span(from_window.bar_loudness(), bars);
        assert!(
            window_span.is_none_or(|span| span < 100.0),
            "the window should not know about a chorus at 150s, got {window_span:?}"
        );

        // The envelope does see it, and it is where the chorus is.
        let span = from_envelope
            .loudest_span(from_envelope.bar_loudness(), bars)
            .expect("the envelope sees the chorus");
        assert!(
            (145.0..180.0).contains(&span),
            "expected the chorus near 150s, got {span:.1}s"
        );

        // And it reports reaching the end of the track, which is what lets
        // the planner trust it as the *last* chorus.
        let until = from_envelope.analysed_until().expect("a loudness profile");
        assert!(
            until >= 195.0,
            "the envelope should cover the track, reached {until:.1}s"
        );
    }

    /// The envelope must be cheap enough to hold a whole track: this is the
    /// reason it exists rather than keeping every sample.
    #[test]
    fn the_envelope_is_far_smaller_than_the_audio() {
        let seconds = 360.0;
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let samples = vec![0.5f32; (seconds * f64::from(rate)) as usize * channels];
        let envelope = crate::automix_track::envelope_of(&samples);

        let audio_bytes = samples.len() * std::mem::size_of::<f32>();
        let envelope_bytes = envelope.len() * std::mem::size_of::<f64>();
        assert!(
            envelope_bytes * 1000 < audio_bytes,
            "the envelope must be orders of magnitude smaller: \
             {envelope_bytes} vs {audio_bytes} bytes"
        );
        // One reading per hop, as the contract says.
        let expected = (seconds / crate::automix_track::ENERGY_HOP_SECONDS) as usize;
        assert!(
            envelope.len().abs_diff(expected) <= 1,
            "expected about {expected} readings, got {}",
            envelope.len()
        );
    }

    /// The transition the whole design is for: leave just after a chorus
    /// ends, and arrive just before the next one begins.
    ///
    /// The chorus has to end well before the track does, or the outro
    /// fallback would land in the same place and the test could not tell
    /// whether the chorus was used at all.
    #[test]
    fn the_pair_meets_at_the_chorus_edges() {
        // The outgoing track: a chorus at 40-60s, a second at 70-90s, then a
        // long outro to 140s. Leaving after the chorus is around 90s;
        // leaving at the outro would be around 130s.
        let out_bands = band_envelope(&[
            ("intro", 8.0),
            ("verse", 32.0),
            ("chorus", 20.0),
            ("verse", 10.0),
            ("chorus", 20.0),
            ("outro", 50.0),
        ]);
        let mut from = Analysis::of(&click_track(128.0, 40.0, 44_100), 44_100).expect("a grid");
        from.refresh_bands(&out_bands);

        // The incoming track: its chorus starts at 50s.
        let in_bands = band_envelope(&[("intro", 5.0), ("verse", 45.0), ("chorus", 20.0)]);
        let mut to = Analysis::of(&click_track(128.0, 50.0, 44_100), 44_100).expect("a grid");
        to.refresh_bands(&in_bands);

        let planned = plan_exit_matched(&from, Some(&to), Duration::from_secs(140), 0.0)
            .expect("the pair is mixable");

        // It leaves after the second chorus, not at the outro: this is what
        // separates a chorus exit from the fallback.
        let exit = planned.fade_out_at;
        assert!(
            (88.0..110.0).contains(&exit),
            "the exit at {exit:.1}s should follow the second chorus (ends ~90s), \
             where the outro fallback would be near 130s"
        );
        // It arrives ahead of the incoming chorus, so its lift is not buried
        // under the outgoing track.
        let arrival = planned.fade_in_at;
        assert!(
            arrival < 50.0,
            "the arrival at {arrival:.1}s should precede the chorus at 50s"
        );
        assert!(
            arrival + planned.duration.as_secs_f64() >= 45.0,
            "the overlap should reach the chorus: arrives {arrival:.1}s for {:.1}s",
            planned.duration.as_secs_f64()
        );
    }

    /// A chorus is not simply louder: it is balanced differently, with more
    /// kick and more cymbals. Total energy cannot see that, which is why the
    /// band split exists — this pins that a signal with a kick and hats
    /// lands its high band above its low-mid balance while a plain tone does
    /// not.
    #[test]
    fn the_bands_tell_a_kick_and_hats_from_a_plain_tone() {
        let rate = crate::vis::SAMPLE_RATE;
        let seconds = 2.0;
        let frames = (seconds * f64::from(rate)) as usize;
        const CHANNELS: usize = 2;

        let plain: Vec<f32> = (0..frames)
            .flat_map(|index| {
                let t = index as f64 / f64::from(rate);
                let value = (0.3 * (std::f64::consts::TAU * 440.0 * t).sin()) as f32;
                [value; CHANNELS]
            })
            .collect();

        // A kick and hats, as a chorus carries: 60 Hz and high noise.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let rich: Vec<f32> = (0..frames)
            .flat_map(|index| {
                let t = index as f64 / f64::from(rate);
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let noise = (seed >> 40) as f64 / 8_388_608.0 - 1.0;
                let value = (0.3 * (std::f64::consts::TAU * 440.0 * t).sin()
                    + 0.4 * (std::f64::consts::TAU * 60.0 * t).sin()
                    + 0.15 * noise)
                    as f32;
                [value; CHANNELS]
            })
            .collect();

        use crate::automix_track::envelope_with_bands;
        let ratio = |samples: &[f32]| {
            let (_, bands) = envelope_with_bands(samples);
            let sum = |band: usize| bands[band].iter().sum::<f64>();
            let low = sum(0);
            let mid = sum(1);
            let high = sum(2);
            // The top band against the middle: a chorus leans high.
            high / mid.max(f64::MIN_POSITIVE)
        };

        let plain_ratio = ratio(&plain);
        let rich_ratio = ratio(&rich);
        assert!(
            rich_ratio > plain_ratio * 2.0,
            "the kick-and-hats signal must lean high: {rich_ratio:.4} against {plain_ratio:.4}"
        );
    }

    /// Builds a band envelope directly, so the detection can be tested
    /// against a known structure without synthesising audio for it.
    ///
    /// `parts` are `(kind, seconds)` where the kind picks the band balance:
    /// a chorus leans low and high with a dip in the middle, a verse leans
    /// on the middle.
    fn band_envelope(parts: &[(&str, f64)]) -> [Vec<f64>; 3] {
        let hop = crate::automix_track::ENERGY_HOP_SECONDS;
        let mut low = Vec::new();
        let mut mid = Vec::new();
        let mut high = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f64 / 8_388_608.0 - 1.0
        };
        for (kind, seconds) in parts {
            let (l, m, h) = match *kind {
                "chorus" => (1.35, 1.0, 1.35),
                "intro" => (0.7, 0.8, 0.4),
                "outro" => (0.6, 0.7, 0.35),
                _ => (1.0, 1.3, 0.55),
            };
            for _ in 0..(seconds / hop) as usize {
                low.push(l + noise() * 0.03);
                mid.push(m + noise() * 0.03);
                high.push(h + noise() * 0.03);
            }
        }
        [low, mid, high]
    }

    /// The point of the band split: the planner needs both edges of a
    /// chorus, because a transition leaves just after one ends and brings
    /// the next track in just before one begins. Loudness cannot find them —
    /// a chorus is balanced differently, not merely louder.
    #[test]
    fn a_chorus_is_found_by_band_balance_not_by_level() {
        let bands = band_envelope(&[
            ("intro", 10.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("outro", 12.0),
        ]);
        let mut analysis =
            Analysis::of(&click_track(128.0, 30.0, 44_100), 44_100).expect("a grid");
        analysis.refresh_bands(&bands);

        let sections = analysis.loud_sections();
        assert_eq!(sections.len(), 2, "expected two choruses, got {sections:?}");
        // Each chorus is found at its own boundaries, within a few seconds:
        // the ratio is smoothed over two seconds, so the edges are soft.
        for (section, expected) in sections.iter().zip([(28.0, 48.0), (66.0, 86.0)]) {
            assert!(
                (section.start - expected.0).abs() < 4.0,
                "chorus start {:.1}s, expected about {:.1}s",
                section.start,
                expected.0
            );
            assert!(
                (section.end - expected.1).abs() < 4.0,
                "chorus end {:.1}s, expected about {:.1}s",
                section.end,
                expected.1
            );
        }
    }

    /// The two edges the transition actually uses: leave after the last
    /// chorus that has finished, and arrive before the next one starts.
    #[test]
    fn the_two_chorus_edges_are_readable() {
        let bands = band_envelope(&[
            ("intro", 10.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("outro", 12.0),
        ]);
        let mut analysis =
            Analysis::of(&click_track(128.0, 30.0, 44_100), 44_100).expect("a grid");
        analysis.refresh_bands(&bands);

        // Partway through the second verse, the first chorus is the last one
        // that has finished.
        let ended = analysis
            .chorus_ended_by(50.0)
            .expect("the first chorus is over by 50s");
        assert!(
            (ended.end - 48.0).abs() < 4.0,
            "the exit should follow the first chorus, got {:.1}s",
            ended.end
        );

        // The incoming track is brought in ahead of a chorus, so it wants
        // the one that starts next.
        let next = analysis
            .chorus_starting_after(50.0)
            .expect("a chorus starts after 50s");
        assert!(
            (next.start - 66.0).abs() < 4.0,
            "the arrival should precede the second chorus, got {:.1}s",
            next.start
        );
    }

    /// A track with no chorus at all must yield no sections rather than a
    /// guess, so the caller falls back to an ordinary exit.
    #[test]
    fn a_track_with_no_chorus_reports_none() {
        let bands = band_envelope(&[("intro", 10.0), ("verse", 40.0), ("outro", 10.0)]);
        let mut analysis =
            Analysis::of(&click_track(128.0, 30.0, 44_100), 44_100).expect("a grid");
        analysis.refresh_bands(&bands);
        assert!(
            analysis.loud_sections().is_empty(),
            "expected no chorus, got {:?}",
            analysis.loud_sections()
        );
    }

    /// The bug this covers: the section times were recorded with the
    /// analysis offset baked in at detection time. Detection happens before
    /// the caller says where in the track the audio came from, so the offset
    /// was always zero when it was written, and every later anchor was
    /// ignored — a track started a third of the way in reported its choruses
    /// in the wrong place.
    #[test]
    fn an_anchored_track_reports_its_choruses_where_they_play() {
        let bands = band_envelope(&[
            ("intro", 10.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("verse", 18.0),
            ("chorus", 20.0),
            ("outro", 12.0),
        ]);
        let grid = click_track(128.0, 30.0, 44_100);
        let base = Analysis::of(&grid, 44_100).expect("a grid");
        let mut analysis = base.clone();
        analysis.refresh_bands(&bands);

        // Read from the top: the first chorus sits at 28s of audio.
        let from_start = analysis.loud_sections();
        assert_eq!(from_start.len(), 2);
        assert!(
            (from_start[0].start - 28.0).abs() < 4.0,
            "the first chorus should be near 28s, got {:.1}s",
            from_start[0].start
        );

        // The same audio, but the track was started 60s in: every section
        // moves with it, because the readings are anchored to where the
        // audio came from rather than to where the analysis began.
        let anchored = analysis.anchored_at(60.0);
        let shifted = anchored.loud_sections();
        assert_eq!(shifted.len(), 2);
        assert!(
            (shifted[0].start - 88.0).abs() < 4.0,
            "anchored 60s in, the first chorus should be near 88s, got {:.1}s",
            shifted[0].start
        );
        assert!(
            (anchored.chorus_starting_after(80.0).expect("a chorus").start - 88.0).abs() < 4.0,
            "the accessors must report anchored times too"
        );
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
        // 1.9x is the same groove at double time, so it folds to 0.95.
        let raw: f64 = 1.9;
        let folded = fold_octave(raw);
        assert!(
            (folded - 0.95).abs() < 1e-9,
            "1.9 should fold down to 0.95, got {folded}"
        );
        assert!((folded - 1.0).abs() <= MAX_FOLDED_GAP);
    }

    /// The reason no pair has to be refused: inside the fold's reach the gap
    /// it leaves is never wider than the two decks can share between them.
    #[test]
    fn folding_always_lands_inside_what_the_decks_can_share() {
        // The fold searches two octaves either way, so every ratio from 1/4
        // to 4 is reachable — 60 against 240 BPM spans that whole range, and
        // no two real tracks are wider apart than those.
        let mut worst: f64 = 0.0;
        let mut worst_at: f64 = 0.0;
        for step in 0..=40_000 {
            let raw = 0.25 + f64::from(step) * (3.75 / 40_000.0);
            let gap = (fold_octave(raw) - 1.0).abs();
            assert!(
                gap <= MAX_FOLDED_GAP + 1e-9,
                "{raw} folded to a gap of {gap}, past the {MAX_FOLDED_GAP} the decks can share"
            );
            if gap > worst {
                worst = gap;
                worst_at = raw;
            }
        }
        // The bound has to be tight, or it is not describing anything. It is
        // reached where a ratio sits exactly between two octaves and either
        // way leaves a third: 4/3 and 8/3 both do.
        assert!(
            (worst - MAX_FOLDED_GAP).abs() < 1e-4,
            "the sweep never reached the bound ({worst} against {MAX_FOLDED_GAP})"
        );
        assert!(
            (worst_at - 4.0 / 3.0).abs() < 1e-3 || (worst_at - 8.0 / 3.0).abs() < 1e-3,
            "the worst fold came from {worst_at}, which is not one of the between-octaves ties"
        );
    }

    /// The stretch sits on whichever deck is quietest, which is what makes a
    /// shared sweep sound better than stretching one deck outright: each
    /// deck drifts away from its own tempo only as it fades, and is at its
    /// own tempo at the moment it owns the mix.
    #[test]
    fn each_deck_drifts_only_as_it_fades() {
        let folded = fold_octave(1.3333);
        let sweep = |progress: f64| {
            let up = folded.powf(progress);
            (up, up / folded)
        };

        // The outgoing deck starts at its own tempo and is pulled away as it
        // hands over, so its deviation only grows.
        let mut previous = 0.0;
        for step in 0..=20 {
            let (up, _) = sweep(f64::from(step) / 20.0);
            let deviation = (up - 1.0).abs();
            assert!(
                deviation >= previous - 1e-9,
                "the outgoing deck came back towards its own tempo mid-fade"
            );
            previous = deviation;
        }

        // The incoming deck is the mirror of that: furthest from its own
        // tempo while it is inaudible, and settled on it by the time it owns
        // the mix.
        let mut previous = f64::INFINITY;
        for step in 0..=20 {
            let (_, across) = sweep(f64::from(step) / 20.0);
            let deviation = (across - 1.0).abs();
            assert!(
                deviation <= previous + 1e-9,
                "the incoming deck drifted further off as it grew louder"
            );
            previous = deviation;
        }
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
        assert!((planned.tempo_ratio - 1.0).abs() <= MAX_FOLDED_GAP);
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

    /// The bug that cut the end off a real track: analysis hears a window,
    /// so `loudest_span` reports the loudest part *of that window*. On a
    /// track 281s long whose first 30s were analysed, the chorus it found was
    /// around 235s, and treating that as the track's final chorus threw away
    /// the last 46 seconds.
    ///
    /// The structure is only trustworthy where it was measured, so a window
    /// that stopped early must not decide where the track ends.
    #[test]
    fn an_early_chorus_does_not_cut_the_end_off_a_long_track() {
        // A track 300s long, of which only the opening 60s was analysed —
        // with a clearly loud chorus inside that opening.
        let a = Analysis::of(
            &click_track_with_chorus(128.0, 60.0, 44_100, 16.0, 32.0),
            44_100,
        )
        .unwrap();
        let out = Duration::from_secs(300);
        let planned = plan_exit_matched(&a, None, out, 0.0).expect("a plan");

        // Whatever it picks, the fade must run to the end of the track: the
        // planner has no evidence about anything after the window it heard,
        // so it must fall back to the outro rather than the mid-window chorus.
        // Two bars of slack: one for a slow decode, one because the exit is
        // snapped back to a downbeat at or before the latest safe start.
        let slack = 2.0 * a.bar_seconds();
        let tail = out.as_secs_f64() - (planned.fade_out_at + planned.duration.as_secs_f64());
        assert!(
            tail <= slack + 1e-6,
            "planned an exit at {:.2}s leaving {tail:.2}s of the track unplayed",
            planned.fade_out_at
        );
        // The old behaviour put the exit at the mid-window chorus. The
        // fallback is the outro, which this pins.
        assert!(
            planned.fade_out_at > 280.0,
            "the exit fell back to {:.2}s, not the end of the track",
            planned.fade_out_at
        );
    }

    /// A pair whose raw tempos look hopeless is still mixable: folding brings
    /// them into the band the two decks can share, so the transition is
    /// planned and matched rather than refused.
    #[test]
    fn a_pair_that_looks_hopeless_still_gets_a_transition() {
        let a = Analysis::of(&click_track(90.0, 40.0, 44_100), 44_100).unwrap();
        let b = Analysis::of(&click_track(120.0, 40.0, 44_100), 44_100).unwrap();
        let raw = b.bpm / a.bpm;
        let planned = plan(&a, &b, Duration::from_secs(40)).expect("every pair is mixable");

        // The point of the test: the raw quotient is outside the shareable
        // band, so planning at all means folding happened.
        assert!(
            (raw - 1.0).abs() > MAX_FOLDED_GAP,
            "the pair was not wide enough for this to prove anything: {raw}"
        );
        assert!(
            (planned.tempo_ratio - 1.0).abs() <= MAX_FOLDED_GAP + 1e-9,
            "the ratio left the band the decks can share: {}",
            planned.tempo_ratio
        );
        assert_ne!(
            planned.tempo_ratio, 1.0,
            "the pair has to be matched, not left plain"
        );
        assert!(
            (planned.tempo_ratio - fold_octave(raw)).abs() < 1e-9,
            "the ratio must be the folded quotient of the measured tempos"
        );
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
