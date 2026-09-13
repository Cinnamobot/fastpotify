//! Drives automix from the player's event stream.
//!
//! The sink collects the playing track's audio ([`crate::automix_track`]);
//! this decides what to do with it. When the player is about to load the
//! next track, the collected grid is turned into a transition plan and
//! handed to librespot, which fires the overlap on the plan's timing.
//!
//! Only the outgoing track reaches the sink: librespot preloads the next
//! track's decoder without writing it anywhere, so its grid is unknown and
//! the plan is made with [`plan_exit`]. That still lands the crossfade on a
//! downbeat, which is the audible part.

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
        })
    }

    /// Called when a new track starts: the previous grid is no longer the
    /// one playing, and the collector starts over.
    pub fn track_changed(&mut self) {
        self.collector.clear();
        self.playing = None;
    }

    /// Called as the track plays, to keep the grid current.
    ///
    /// Analysis runs on the worker thread, so this only queues a snapshot
    /// once enough audio has been collected to be worth it.
    pub fn tick(&mut self, sample_rate: u32) {
        if self.playing.is_some() || !self.collector.is_ready(sample_rate) {
            return;
        }
        if let Some(analysis) = self.worker.latest() {
            self.playing = Some(analysis);
            return;
        }
        self.worker.analyse(self.collector.snapshot());
    }

    /// The overlap to use for the coming boundary, if one can be planned.
    ///
    /// `elapsed` is how far into the playing track the player is, and
    /// `out_duration` is the whole track's length.
    pub fn plan(&self, elapsed: Duration, out_duration: Duration) -> Option<automix::Transition> {
        let playing = self.playing.as_ref()?;
        automix::plan_exit(playing, out_duration)
            .filter(|transition| transition.fade_out_at >= elapsed.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click track at 128 BPM, interleaved.
    fn clicks(seconds: f64, rate: u32) -> Vec<f32> {
        let channels = crate::vis::CHANNELS as usize;
        let mut samples = vec![0.0f32; (seconds * f64::from(rate)) as usize * channels];
        let beat = 60.0 / 128.0;
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
            automix.tick(rate);
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
        collector.push(&clicks(35.0, 44_100));
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
        collector.push(&clicks(35.0, 44_100));
        assert!(settle(&mut automix, 44_100));

        // Pretend the track is far shorter than the plan's exit point.
        assert!(
            automix
                .plan(Duration::from_secs(0), Duration::from_secs(3))
                .is_none(),
            "a track with no room must not arm a transition"
        );
    }

    #[test]
    fn automix_is_off_without_a_collector() {
        assert!(Automix::new(None, 44_100).is_none());
    }
}
