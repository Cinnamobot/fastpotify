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
use crate::automix_track::{Collector, Worker};

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
        })
    }

    /// Called when a new track starts: the previous grid is no longer the
    /// one playing, and the collector starts over.
    pub fn track_changed(&mut self) {
        self.collector.clear();
        self.playing = None;
        self.incoming = None;
    }

    /// Called after a seek. The track is unchanged, so a finished grid still
    /// describes it, but the collected audio now has a jump in it and must
    /// not be tracked as if it were continuous.
    pub fn seeked(&mut self) {
        self.collector.clear();
        self.collected_from = 0.0;
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
        self.incoming_worker.analyse(probe.samples.clone());
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
        if self.playing.is_some() || !self.collector.is_ready(sample_rate) {
            return;
        }
        if let Some(analysis) = self.worker.latest() {
            log::debug!(
                "automix: beat grid ready at {:.1} BPM, anchored at {:.1}s",
                analysis.bpm,
                self.collected_from
            );
            self.playing = Some(analysis);
            return;
        }
        log::debug!(
            "automix: handing collected audio (from {:.1}s) to the analyser",
            self.collected_from
        );
        self.worker.analyse(self.collector.snapshot());
    }

    /// The overlap to use for the coming boundary, if one can be planned.
    ///
    /// `elapsed` is how far into the playing track the player is, and
    /// `out_duration` is the whole track's length.
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
        automix::plan_exit_matched(&anchored, incoming.as_ref(), out_duration, elapsed.as_secs_f64())
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

    /// A pair too far apart in tempo is refused rather than smeared, so the
    /// boundary falls back to the plain crossfade even though both grids are
    /// known.
    #[test]
    fn a_hopeless_pair_falls_back_to_the_plain_crossfade() {
        let collector = Collector::new(44_100);
        let mut automix = Automix::new(Some(Arc::clone(&collector)), 44_100).expect("on");
        automix.playing = Some(
            Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"),
        );
        // 128 against 176 BPM is far past the stretch limit.
        automix.incoming = Some((
            Analysis::of(&clicks_at(176.0, 20.0, 44_100), 44_100).expect("analysable"),
            0.0,
        ));
        assert!(
            automix
                .plan(Duration::from_secs(0), Duration::from_secs(240))
                .is_none(),
            "an unmixable pair must not be armed"
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
