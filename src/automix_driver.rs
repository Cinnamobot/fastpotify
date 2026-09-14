//! Drives automix from the player's event stream.
//!
//! The sink collects the playing track's audio ([`crate::automix_track`]);
//! this decides what to do with it. When the player is about to load the
//! next track, the collected grid is turned into a transition plan and
//! handed to librespot, which fires the overlap on the plan's timing.
//!
//! Only the outgoing track reaches the sink, so the incoming track's grid
//! comes from a short probe of its decoder instead. With both grids the pair
//! is tempo-matched and the overlap starts on the incoming track's downbeat;
//! without the probe the plan still lands the exit on a downbeat of the
//! outgoing track, which is the audible part.

use std::sync::Arc;
use std::time::Duration;

use crate::automix::{self, Analysis};
use crate::automix_track::{Collector, Worker, envelope_with_bands};

/// How long before a transition's own lead-in the plan is armed, so a slow
/// decode or a late probe still has time to change the decision.
///
/// It has to cover the whole round trip that produces the tempo matching: the
/// arm is what triggers the preload, the preload is what fetches the track the
/// probe is read from, and the probe then has to be decoded and analysed
/// before the exit arrives. Measured on a real boundary, that trip takes about
/// five seconds — three from the arm to the probe landing, two more to the
/// grid. At the five seconds this used to be, the grid therefore arrived at
/// the exit rather than before it and no pair was ever matched; the revision
/// below could not run at all.
///
/// Thirty seconds leaves that trip six times over, so the grid is in hand
/// well before the decision has to be final.
const PLAN_SLACK: Duration = Duration::from_secs(30);

/// Plans and arms transitions for one engine.
///
/// One per engine: it holds the worker thread doing the analysis, and the
/// most recent result so a track boundary does not have to wait for it.
pub struct Automix {
    collector: Arc<Collector>,
    worker: Worker,
    /// The grid for the track that is playing, once analysis has finished.
    playing: Option<Analysis>,
    /// Seconds into the playing track where the collected audio began, so
    /// the grid it produces can be read in the track's own time.
    collected_from: f64,
    /// Seconds into the preloaded track where its probe began.
    incoming_from: f64,
    /// A second worker for the track being preloaded. It is separate so an
    /// incoming probe cannot overwrite the grid of the track still playing.
    incoming_worker: Worker,
    /// The preloaded track's grid, and where its probe began.
    incoming: Option<(Analysis, f64)>,
    /// The opening of the preloaded track, kept so the overlap's other half
    /// can be rendered from it.
    incoming_audio: Arc<Vec<f32>>,
    /// The transition currently handed to the player, so it is only sent
    /// again when it actually changes.
    armed: Option<automix::Transition>,
    /// How many envelope readings the structure was last read from, so a
    /// refresh only runs when the envelope has actually grown.
    restructured_at: usize,
}

impl std::fmt::Debug for Automix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Automix")
    }
}

impl Automix {
    /// Builds the driver, or `None` when automix is off.
    pub fn new(collector: Option<Arc<Collector>>, sample_rate: u32) -> Option<Self> {
        let collector = collector?;
        Some(Self {
            collector,
            worker: Worker::spawn(sample_rate),
            playing: None,
            collected_from: 0.0,
            incoming_from: 0.0,
            incoming_worker: Worker::spawn(sample_rate),
            incoming: None,
            incoming_audio: Arc::new(Vec::new()),
            armed: None,
            restructured_at: 0,
        })
    }

    /// Called when a new track starts: the previous grid is no longer the
    /// one playing, and the collector starts over.
    pub fn track_changed(&mut self) {
        self.collector.clear();
        self.playing = None;
        self.incoming = None;
        self.incoming_audio = Arc::new(Vec::new());
        self.armed = None;
    }

    /// Takes back an armed plan whose boundary is no longer the one it was
    /// made for, so a track change or a seek does not leave it to fire at
    /// the wrong moment. Returns whether the player has to be told.
    pub fn withdraw_plan(&mut self) -> bool {
        std::mem::take(&mut self.armed).is_some()
    }

    /// Called after a seek. The track is unchanged, so a finished grid still
    /// describes it, but the collected audio now has a jump in it and must
    /// not be tracked as if it were continuous.
    pub fn seeked(&mut self) {
        self.collector.clear();
        self.collected_from = 0.0;
    }

    /// The transition to hand the player for the coming boundary, or `None`
    /// when nothing changed.
    ///
    /// The player holds the best plan known for the coming boundary and is
    /// told again only when that plan changes. Re-sending the same one would
    /// be pointless, and sending `None` would take the boundary's crossfade
    /// away entirely: reaching the exit must not disturb the plan, because
    /// the exit *is* the moment the transition starts. [`Self::plan`] returns
    /// `None` once there is no room left, which is why that case is left
    /// alone rather than forwarded.
    ///
    /// The plan changes for two reasons. Arming has to happen early — the
    /// player decides when to preload the next track from the plan in hand,
    /// so a plan that waited for the probe would never trigger the preload
    /// that produces it — so the first plan carries no tempo matching and is
    /// replaced once the probe has been analysed. The other is the outgoing
    /// track's own structure: the exit comes from the loud sections found so
    /// far, and a later reading can move it.
    pub fn take_plan_change(
        &mut self,
        elapsed: Duration,
        out_duration: Duration,
    ) -> Option<Option<automix::Transition>> {
        let mut planned = self.plan(elapsed, out_duration)?;
        // Hold the first plan as soon as there is one: it is what tells the
        // player the next track has to be fetched early enough to analyse.
        let lead_in = out_duration
            .saturating_sub(Duration::from_secs_f64(planned.fade_out_at))
            .max(planned.duration);
        let due = out_duration.saturating_sub(elapsed) <= lead_in + PLAN_SLACK;
        if !due {
            return None;
        }
        match &self.armed {
            // Already holding this exact plan: nothing to say, and nothing to
            // render. This is the common path — it runs several times a
            // second — so the render below must not be reachable from here.
            Some(armed) if *armed == planned => return None,
            Some(armed) => {
                if armed.tempo_ratio != planned.tempo_ratio {
                    log::debug!(
                        "automix: revising the transition to {:.4}x now the incoming grid is in",
                        planned.tempo_ratio
                    );
                } else if (armed.fade_out_at - planned.fade_out_at).abs() > 0.01 {
                    log::debug!(
                        "automix: moving the exit to {:.2}s; the track's structure reads differently now",
                        planned.fade_out_at
                    );
                }
            }
            None => {}
        }
        planned.curve = self.render_curve_for(&planned);
        self.armed = Some(planned.clone());
        Some(Some(planned))
    }

    /// The plan the player is holding, so the host can tell whether one
    /// still needs taking back when the track changes.
    pub fn armed(&self) -> Option<&automix::Transition> {
        self.armed.as_ref()
    }

    /// Receives the opening of the track being preloaded, so the transition
    /// into it can be planned before it starts.
    ///
    /// The incoming track never reaches the sink while it is only preloaded,
    /// so this probe is the only chance to measure it. Analysis runs on the
    /// same worker, and the result is kept until the boundary.
    pub fn incoming(&mut self, probe: &crate::automix::Probe) {
        if probe.samples.is_empty() {
            return;
        }
        log::debug!(
            "automix: probing the incoming track from {:.1}s ({} samples)",
            probe.position_seconds,
            probe.samples.len()
        );
        self.incoming = None;
        self.incoming_from = probe.position_seconds;
        // Kept, not just measured: the overlap's other half has to be
        // rendered from the track's own audio, and this probe is the only
        // copy of it that exists before the track starts.
        self.incoming_audio = Arc::new(probe.samples.clone());
        // The probe's own audio is all there is of the incoming track, so
        // its envelope is measured from it directly rather than collected
        // from the sink the way the playing track's is.
        let (envelope, bands) = envelope_with_bands(&probe.samples);
        self.incoming_worker
            .analyse(probe.samples.clone(), envelope, bands);
    }

    /// The preloaded track's grid, read in its own time, once it is ready.
    pub fn incoming_analysis(&mut self) -> Option<&Analysis> {
        if self.incoming.is_none()
            && let Some(analysis) = self.incoming_worker.latest()
        {
            log::debug!("automix: incoming grid ready at {:.1} BPM", analysis.bpm);
            self.incoming = Some((analysis, self.incoming_from));
        }
        self.incoming.as_ref().map(|(analysis, _)| analysis)
    }

    /// Called as the track plays, to keep the grid current.
    ///
    /// `position` is where the play head is now; the first call after a
    /// track change or a seek fixes where the collected audio began, which
    /// is what the finished grid is anchored to.
    ///
    /// Analysis runs on the worker thread, so this only queues a snapshot
    /// once enough audio has been collected to be worth it.
    pub fn tick(&mut self, sample_rate: u32, position: Duration) {
        if self.collector.is_empty() {
            self.collected_from = position.as_secs_f64();
        }
        // A published analysis is always picked up, not only the first: the
        // structure is re-read as the track plays, and those results replace
        // the one that was kept.
        let published = self.worker.latest();
        let fresh = match (&self.playing, &published) {
            (None, Some(_)) => true,
            (Some(old), Some(new)) => new.bar_loudness().len() > old.bar_loudness().len(),
            _ => false,
        };
        if fresh {
            let analysis = published.expect("checked by `fresh`");
            if self.playing.is_none() {
                log::debug!(
                    "automix: beat grid ready at {:.1} BPM, anchored at {:.1}s",
                    analysis.bpm,
                    self.collected_from
                );
            } else {
                log::debug!(
                    "automix: structure re-read, now covering {:.0}s of the track",
                    analysis.analysed_until().unwrap_or(0.0)
                );
                let sections = analysis.loud_sections();
                if !sections.is_empty() {
                    log::debug!(
                        "automix: {} loud section(s) found: {}",
                        sections.len(),
                        sections
                            .iter()
                            .map(|section| format!("{:.0}-{:.0}s", section.start, section.end))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            self.playing = Some(analysis);
            return;
        }
        if self.playing.is_none() {
            if !self.collector.is_ready(sample_rate) {
                return;
            }
            let envelope = self.collector.envelope_rms();
            self.restructured_at = envelope.len();
            log::debug!(
                "automix: handing collected audio (from {:.1}s) to the analyser",
                self.collected_from
            );
            self.worker.analyse(
                self.collector.snapshot(),
                envelope,
                self.collector.envelope_bands(),
            );
            return;
        }

        // The grid is settled, but where the track's sections are only
        // becomes clear as more of it is heard — and leaving before the last
        // chorus is the thing this has to avoid. Re-reading the structure
        // against the longer envelope costs no beat tracking.
        let reached = self.collector.envelope_len();
        if !self.structure_is_stale(reached) {
            return;
        }
        self.restructured_at = reached;
        self.worker.restructure(self.collector.envelope_rms(), self.collector.envelope_bands());
    }

    /// Whether the published analysis describes fewer bars than the
    /// envelope has completed, and one has not already been asked for.
    ///
    /// The published bar count is compared against the envelope rather than
    /// tracked separately, so a refresh is requested whenever the readings
    /// have genuinely run ahead — and `restructured_at` stops a request from
    /// being re-sent for an envelope already handed over.
    fn structure_is_stale(&self, readings: usize) -> bool {
        if readings <= self.restructured_at {
            return false;
        }
        let Some(playing) = self.playing.as_ref() else {
            return false;
        };
        let hops_per_bar =
            (playing.bar_seconds() / crate::automix_track::ENERGY_HOP_SECONDS).round();
        if !(hops_per_bar >= 1.0) {
            return false;
        }
        // Whole bars the envelope has completed, less the one still filling.
        let whole_bars = readings as f64 / hops_per_bar - 1.0;
        (playing.bar_loudness().len() as f64) + 1.0 < whole_bars
    }

    /// The overlap to use for the coming boundary, if one can be planned.
    ///
    /// `elapsed` is how far into the playing track the player is, and
    /// `out_duration` is the whole track's length.
    ///
    /// The returned transition carries the incoming track's half of the tempo
    /// sweep, rendered from the probe's own audio. It is rendered here rather
    /// than played live because the live path cannot feed a swept deck
    /// without either starving it or reading the track ahead of what has been
    /// heard; see [`automix::render_curve`].
    pub fn plan(&mut self, elapsed: Duration, out_duration: Duration) -> Option<automix::Transition> {
        // Resolve the incoming grid here, so a probe that finished while the
        // outgoing track was still playing is picked up before it is needed.
        let incoming = self.incoming_analysis().cloned();
        let playing = self.playing.as_ref()?;
        let anchored = playing.clone().anchored_at(self.collected_from);
        // The probe is anchored where its audio was taken from, the same way
        // the playing grid is: without it the incoming bars are read as if
        // the track began at the probe.
        let incoming = incoming.map(|to| to.anchored_at(self.incoming_from));
        automix::plan_exit_matched(
            &anchored,
            incoming.as_ref(),
            out_duration,
            elapsed.as_secs_f64(),
        )
    }

    /// Attaches the incoming track's rendered half to a plan, if it needs one.
    ///
    /// Called only when the plan has actually changed. The render is seconds
    /// of audio through the keylock engine, and `take_plan_change` runs on
    /// every position update — several times a second — so doing this on each
    /// call re-rendered the same overlap over and over and starved the sink.
    /// The plan's own three numbers decide whether anything needs rendering,
    /// which is why they are compared without the curve.
    fn render_curve_for(&self, planned: &automix::Transition) -> Option<Arc<librespot_playback::player::IncomingCurve>> {
        if planned.tempo_ratio == 1.0 {
            return None;
        }
        let channels = crate::vis::CHANNELS as usize;
        let rate = crate::vis::SAMPLE_RATE;
        let from_ms = (planned.fade_in_at.max(0.0) * 1000.0) as usize;
        let from = from_ms * rate as usize / 1000 * channels;
        if from >= self.incoming_audio.len() {
            // The overlap starts past everything the probe heard. The opening
            // is a fixed window and a boundary can be planned ahead of a
            // chorus well beyond it, so this is expected rather than a fault:
            // the tail carries the whole stretch instead.
            return None;
        }
        let frames = (planned.duration.as_secs_f64() * f64::from(rate)) as usize;
        let rendered = automix::render_curve(
            &self.incoming_audio[from..],
            planned.tempo_ratio,
            frames,
            channels,
            rate,
        );
        if rendered.is_empty() {
            log::debug!("automix: the curve did not render, so the tail carries the whole stretch");
            return None;
        }
        let consumed = automix::curve_frames_consumed(planned.tempo_ratio, frames);
        log::debug!(
            "automix: rendered {:.2}s of the incoming track for the overlap, covering {:.2}s of it",
            frames as f64 / f64::from(rate),
            consumed as f64 / f64::from(rate)
        );
        Some(Arc::new(librespot_playback::player::IncomingCurve {
            samples: Arc::new(rendered),
            consumed_ms: (consumed as u64 * 1_000 / u64::from(rate)) as u32,
            ratio: planned.tempo_ratio,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click track at 128 BPM, interleaved.
    fn clicks(seconds: f64, rate: u32) -> Vec<f32> {
        clicks_at(128.0, seconds, rate)
    }

    /// The same, at a chosen tempo.
    fn clicks_at(bpm: f64, seconds: f64, rate: u32) -> Vec<f32> {
        let channels = crate::vis::CHANNELS as usize;
        let mut samples = vec![0.0f32; (seconds * f64::from(rate)) as usize * channels];
        let beat = 60.0 / bpm;
        let mut t = 0.0;
        while t < seconds {
            let start = (t * f64::from(rate)) as usize * channels;
            for i in 0..(rate as usize / 100) {
                let index = start + i * channels;
                if index + channels - 1 < samples.len() {
                    let decay = (-(i as f32) / 60.0).exp();
                    for channel in 0..channels {
                        samples[index + channel] = decay;
                    }
                }
            }
            t += beat;
        }
        samples
    }

    /// Waits for the worker to publish, with a bound.
    fn settle(automix: &mut Automix, rate: u32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            automix.tick(rate, Duration::from_secs(0));
            if automix.playing.is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    #[test]
    fn a_track_change_forgets_the_previous_grid() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(20.0, 44_100), 44_100).expect("analysable click track"),
        );
        assert!(automix.playing.is_some());
        automix.track_changed();
        assert!(automix.playing.is_none(), "the old grid must not survive");
        assert!(!automix.collector.is_ready(44_100), "collection starts over");
    }

    #[test]
    fn a_planned_transition_lands_on_the_playing_track() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        let audio: Vec<f64> = clicks(35.0, 44_100).iter().map(|s| f64::from(*s)).collect();
        collector.push(&audio);
        assert!(settle(&mut automix, 44_100), "the grid is published");

        let planned = automix
            .plan(Duration::from_secs(0), Duration::from_secs(240))
            .expect("a long track has room for a transition");
        assert!(planned.duration >= Duration::from_millis(1_500));
        assert!(planned.duration <= automix::MAX_TRANSITION);
        assert_eq!(planned.tempo_ratio, 1.0, "one grid cannot stretch");
    }

    #[test]
    fn a_transition_already_passed_is_not_armed() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        let audio: Vec<f64> = clicks(35.0, 44_100).iter().map(|s| f64::from(*s)).collect();
        collector.push(&audio);
        assert!(settle(&mut automix, 44_100));

        // Pretend the track is far shorter than the plan's exit point.
        assert!(
            automix
                .plan(Duration::from_secs(0), Duration::from_secs(3))
                .is_none(),
            "a track with no room must not arm a transition"
        );
    }

    /// Without a grid for the playing track there is nothing to plan from, so
    /// nothing is armed and the player keeps its own plain crossfade. This is
    /// the path taken when analysis is unavailable, and it must not arm a
    /// transition with made-up timing.
    #[test]
    fn no_grid_arms_nothing() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        assert!(automix.playing.is_none(), "nothing analysed yet");
        assert!(
            automix
                .plan(Duration::from_secs(0), Duration::from_secs(240))
                .is_none(),
            "an unanalysed track must not produce a plan"
        );
    }

    /// The grid on its own is no longer a reason to refuse a pair: folding
    /// and the shared sweep make every tempo mixable, so even 128 against
    /// 176 BPM — a gap far past what one deck could stretch — arms a
    /// transition rather than leaving the boundary to a plain cut.
    #[test]
    fn a_wide_tempo_gap_still_arms_a_matched_transition() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"),
        );
        automix.incoming = Some((
            Analysis::of(&clicks_at(176.0, 20.0, 44_100), 44_100).expect("analysable"),
            0.0,
        ));
        let planned = automix
            .plan(Duration::from_secs(0), Duration::from_secs(240))
            .expect("a wide gap is still mixable");
        assert_ne!(
            planned.tempo_ratio, 1.0,
            "the pair has to be matched, not left plain"
        );
        assert!(
            (planned.tempo_ratio - 1.0).abs() <= automix::MAX_FOLDED_GAP + 1e-9,
            "the ratio left the band the decks can share: {}",
            planned.tempo_ratio
        );
    }

    /// The plan's own numbers decide whether anything needs rendering. The
    /// check runs several times a second and a render is seconds of audio
    /// through the keylock engine, so rendering on every call re-rendered the
    /// same overlap over and over and starved the sink — which a live run
    /// showed as dropped audio.
    #[test]
    fn a_plan_is_the_same_plan_with_its_curve_attached() {
        let ratio = 1.2652;
        let plan = automix::Transition {
            fade_out_at: 120.0,
            fade_in_at: 4.0,
            duration: Duration::from_secs(8),
            tempo_ratio: ratio,
            curve: None,
        };
        // The rendered audio must not be part of the comparison: walking a
        // buffer to learn what three numbers already say would put the cost
        // back on every tick.
        let rendered = automix::Transition {
            curve: Some(Arc::new(librespot_playback::player::IncomingCurve {
                samples: Arc::new(vec![0.0; 8]),
                consumed_ms: 7_000,
                ratio,
            })),
            ..plan.clone()
        };
        assert_eq!(
            plan, rendered,
            "attaching the curve must not make it a different plan"
        );

        let moved = automix::Transition {
            fade_out_at: 121.0,
            ..plan.clone()
        };
        assert_ne!(plan, moved, "a moved exit is a different plan");
        let restretched = automix::Transition {
            tempo_ratio: 1.1,
            ..plan.clone()
        };
        assert_ne!(plan, restretched, "a different ratio is a different plan");
    }

    /// The bug this covers: a plan was armed long before the boundary, at a
    /// time when the incoming track was not yet preloaded and so had no
    /// grid. The plan then carried a tempo ratio of 1.0 — no matching at all
    /// — and nothing ever replaced it, because arming happened once and the
    /// probe landed afterwards.
    ///
    /// The decision has to be made late enough that the probe's answer is in,
    /// and a plan that is no longer wanted has to be withdrawn.
    #[test]
    fn nothing_is_armed_before_the_boundary_is_imminent() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"),
        );

        // Far from the boundary: nothing is armed, so the player's own
        // crossfade governs and no stale ratio can be left behind.
        assert!(
            automix
                .take_plan_change(Duration::from_secs(0), Duration::from_secs(240))
                .is_none(),
            "nothing should have changed this far out"
        );

        // Close to the boundary it arms, and reports the change once.
        let near = Duration::from_millis(240_000 - 8_000);
        let change = automix.take_plan_change(near, Duration::from_secs(240));
        assert!(
            matches!(change, Some(Some(_))),
            "a transition should arm near the boundary, got {change:?}"
        );
        assert!(
            automix
                .take_plan_change(near, Duration::from_secs(240))
                .is_none(),
            "an unchanged plan must not be re-sent"
        );

        // Reaching the exit must not take the plan away. The exit is the
        // moment the transition starts, so withdrawing it here would turn
        // the boundary into a plain cut — which is what used to happen.
        let at_exit = Duration::from_millis(240_000 - 1_000);
        for elapsed in [near, at_exit] {
            assert!(
                automix
                    .take_plan_change(elapsed, Duration::from_secs(240))
                    .is_none(),
                "an armed plan was disturbed at {elapsed:?}"
            );
        }
        assert!(
            automix.armed().is_some(),
            "the plan must still be armed as the boundary arrives"
        );
    }

    /// A plan must be taken back when its boundary is gone — a track change
    /// or a seek — or the player keeps firing a transition nobody chose.
    #[test]
    fn a_plan_is_taken_back_when_the_track_moves_on() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"),
        );
        let near = Duration::from_millis(240_000 - 8_000);
        assert!(matches!(
            automix.take_plan_change(near, Duration::from_secs(240)),
            Some(Some(_))
        ));
        assert!(automix.armed().is_some());

        // The track changes, so the armed boundary is no longer this track's.
        assert!(automix.withdraw_plan(), "the player has to be told");
        assert!(automix.armed().is_none());
        assert!(
            !automix.withdraw_plan(),
            "there is nothing left to withdraw"
        );
    }

    /// The bug that kept the tempo rate pinned at 1.0: the player decides
    /// when to preload the next track from the plan it is holding, so a plan
    /// armed only once the probe had landed could never trigger the preload
    /// that produces the probe. The order was circular, and the resolution
    /// is to arm provisionally and revise.
    ///
    /// This pins both halves: a plan with no tempo matching is handed over
    /// first, and it is replaced when a matched one becomes available.
    #[test]
    fn a_provisional_plan_is_revised_once_the_incoming_grid_arrives() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"),
        );

        // Walk in from far out to the boundary and take the first plan that
        // is handed over. The exact lead-in depends on where the exit lands,
        // so pinning one instant would just encode that arithmetic.
        let out = Duration::from_secs(240);
        let mut first = None;
        for remaining in (6..40).rev() {
            let elapsed = out.saturating_sub(Duration::from_secs(remaining));
            if let Some(change) = automix.take_plan_change(elapsed, out) {
                first = Some((remaining, change));
                break;
            }
        }
        let Some((armed_at, Some(first))) = first else {
            panic!("a provisional plan must be handed over before the boundary, got {first:?}");
        };
        assert_eq!(
            first.tempo_ratio, 1.0,
            "with one grid there is nothing to match"
        );
        assert!(
            armed_at >= 10,
            "armed with only {armed_at}s left, too late for the player to preload"
        );

        // The probe lands, at a different tempo.
        automix.incoming(&crate::automix::Probe {
            samples: clicks_at(135.0, 20.0, 44_100),
            position_seconds: 0.0,
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline && automix.incoming_analysis().is_none() {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(automix.incoming_analysis().is_some(), "the probe produced a grid");

        // Now a matched plan must replace the provisional one. Without this
        // the transition fires at 1.0x and nothing is stretched.
        let at = out.saturating_sub(Duration::from_secs(armed_at));
        let revised = automix.take_plan_change(at, out);
        let Some(Some(revised)) = revised else {
            panic!("the matched plan must replace the provisional one, got {revised:?}");
        };
        assert_ne!(
            revised.tempo_ratio, 1.0,
            "the revised plan carries the pair's tempo ratio"
        );

        // And it settles: no further churn on every position update.
        assert!(
            automix.take_plan_change(at, out).is_none(),
            "a settled plan must not be re-sent"
        );
    }

    #[test]
    fn automix_is_off_without_a_collector() {
        assert!(Automix::new(None, 44_100).is_none());
    }

    /// The incoming track must start on one of its own downbeats, so both
    /// tracks land their bar together instead of one sliding under the other.
    #[test]
    fn an_incoming_grid_moves_the_start_onto_its_downbeat() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");

        // The outgoing track, measured from its start.
        let audio: Vec<f64> = clicks(35.0, 44_100).iter().map(|s| f64::from(*s)).collect();
        collector.push(&audio);
        assert!(settle(&mut automix, 44_100), "the outgoing grid is published");

        // A probe of the incoming track, whose grid has its own phase.
        let probe_samples = clicks(20.0, 44_100);
        automix.incoming(&crate::automix::Probe {
            samples: probe_samples,
            position_seconds: 0.0,
        });

        // Wait for the probe's grid first: `plan` returns as soon as the
        // outgoing track has one, which is before the probe is analysed.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline && automix.incoming_analysis().is_none() {
            std::thread::sleep(Duration::from_millis(25));
        }
        let incoming = automix
            .incoming_analysis()
            .cloned()
            .expect("the probe produced a grid");
        let planned = automix
            .plan(Duration::from_secs(0), Duration::from_secs(240))
            .expect("a transition is planned");
        // The start is on the incoming track's bar lattice: a whole number
        // of bars from its first downbeat.
        let phase = incoming.downbeat_at_or_after(0.0).unwrap();
        let bar = 60.0 / incoming.bpm * 4.0;
        let offset = (planned.fade_in_at - phase) / bar;
        assert!(
            (offset - offset.round()).abs() < 1e-6,
            "start {} is not a whole bar from the incoming phase {phase}",
            planned.fade_in_at
        );
    }
}
