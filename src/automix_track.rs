//! Beat analysis for the track that is playing, so automix can plan a
//! transition into the next one.
//!
//! The official client gets this from Spotify's servers: `spclient` serves
//! per-track beats and cuepoints, but only for playlists the service has
//! been asked to mix, which is why a user's own playlist falls back to a
//! plain crossfade. Analysing locally costs no requests and works for every
//! track.
//!
//! Work happens off the audio path. The sink hands each decoded frame to
//! [`Collector::push`], which only appends to a bounded buffer; the
//! beat tracker runs on a worker thread once the track has been collected.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::automix::Analysis;

/// How much of a track to analyse. Long enough for a stable tempo, short
/// enough to finish well inside a track's runtime.
pub const ANALYSED_SECONDS: f64 = 60.0;

/// Frames the collector keeps before it stops appending.
fn keep_frames(sample_rate: u32) -> usize {
    (ANALYSED_SECONDS * f64::from(sample_rate)) as usize
}

/// Accumulates the playing track's samples for analysis.
pub struct Collector {
    samples: Mutex<Vec<f32>>,
    limit: usize,
}

impl std::fmt::Debug for Collector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Collector")
    }
}

impl Collector {
    pub fn new(sample_rate: u32) -> Arc<Self> {
        Arc::new(Self {
            samples: Mutex::new(Vec::new()),
            limit: keep_frames(sample_rate) * crate::vis::CHANNELS as usize,
        })
    }

    /// Appends interleaved frames. Called from the sink's thread, so it only
    /// copies and stops once the buffer is full.
    pub fn push(&self, interleaved: &[f32]) {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if samples.len() >= self.limit {
            return;
        }
        let room = self.limit - samples.len();
        let take = room.min(interleaved.len());
        samples.extend_from_slice(&interleaved[..take]);
    }

    /// Drops what was collected, for the next track.
    pub fn clear(&self) {
        self.samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    /// A copy of what has been collected so far, to hand to the worker.
    pub fn snapshot(&self) -> Vec<f32> {
        self.samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Whether enough was collected to be worth analysing.
    pub fn is_ready(&self, sample_rate: u32) -> bool {
        let wanted = (ANALYSED_SECONDS / 2.0 * f64::from(sample_rate)) as usize
            * crate::vis::CHANNELS as usize;
        self.samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
            >= wanted
    }
}

/// Runs the tracker for one track off the audio path.
///
/// Dropping the handle stops the worker. The most recent request wins: a
/// track skipped before its analysis finished does not publish a result.
pub struct Worker {
    request: Mutex<Option<Sender<Vec<f32>>>>,
    result: Arc<Mutex<Option<Analysis>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Worker")
    }
}

impl Worker {
    pub fn spawn(sample_rate: u32) -> Self {
        let (tx, rx): (Sender<Vec<f32>>, Receiver<Vec<f32>>) = std::sync::mpsc::channel();
        let result = Arc::new(Mutex::new(None));
        let published = Arc::clone(&result);
        let join = std::thread::Builder::new()
            .name("automix-analysis".into())
            .spawn(move || {
                // Only the newest request matters.
                while let Ok(mut samples) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        samples = newer;
                    }
                    let analysed = Analysis::of(&samples, sample_rate);
                    *published
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner()) = analysed;
                }
            })
            .ok();
        Self {
            request: Mutex::new(Some(tx)),
            result,
            join,
        }
    }

    /// Queues samples for analysis. Cheap: it moves a buffer and returns.
    pub fn analyse(&self, samples: Vec<f32>) {
        let guard = self
            .request
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(samples);
        }
    }

    /// The most recent finished analysis, if any.
    pub fn latest(&self) -> Option<Analysis> {
        self.result
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel ends the loop, so the join cannot hang.
        self.request
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collector_stops_at_its_limit() {
        let collector = Collector::new(44_100);
        let block = vec![0.1f32; 44_100 * crate::vis::CHANNELS as usize];
        for _ in 0..(ANALYSED_SECONDS as usize + 5) {
            collector.push(&block);
        }
        let held = collector.snapshot().len();
        assert_eq!(held, keep_frames(44_100) * crate::vis::CHANNELS as usize);
    }

    #[test]
    fn clearing_a_collector_frees_it_for_the_next_track() {
        let collector = Collector::new(44_100);
        collector.push(&vec![0.1f32; 4096]);
        assert!(!collector.snapshot().is_empty());
        collector.clear();
        assert!(collector.snapshot().is_empty());
    }

    #[test]
    fn a_collector_is_not_ready_until_half_a_minute_has_been_heard() {
        let rate = 44_100;
        let collector = Collector::new(rate);
        let one_second = vec![0.1f32; rate as usize * crate::vis::CHANNELS as usize];
        for _ in 0..20 {
            collector.push(&one_second);
        }
        assert!(!collector.is_ready(rate), "20 seconds is too little");
        for _ in 0..15 {
            collector.push(&one_second);
        }
        assert!(collector.is_ready(rate), "35 seconds is enough");
    }

    #[test]
    fn a_worker_publishes_the_analysis_of_what_it_was_given() {
        let worker = Worker::spawn(44_100);
        assert!(worker.latest().is_none(), "nothing analysed yet");

        // A click track at 128 BPM, long enough to lock onto.
        let rate = 44_100u32;
        let channels = crate::vis::CHANNELS as usize;
        let mut samples = vec![0.0f32; rate as usize * 20 * channels];
        let beat = 60.0 / 128.0;
        let mut t = 0.0;
        while t < 20.0 {
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

        worker.analyse(samples);
        // The worker is a thread; give it a bounded moment to publish.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut published = None;
        while std::time::Instant::now() < deadline {
            if let Some(analysis) = worker.latest() {
                published = Some(analysis);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let analysis = published.expect("the click track is analysable");
        assert!(
            (analysis.bpm - 128.0).abs() < 4.0,
            "tracked {} for a 128 BPM click",
            analysis.bpm
        );
    }

    #[test]
    fn a_worker_with_silence_publishes_nothing() {
        let worker = Worker::spawn(44_100);
        worker.analyse(vec![
            0.0f32;
            44_100 * 10 * crate::vis::CHANNELS as usize
        ]);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(worker.latest().is_none());
    }
}
