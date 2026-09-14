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

/// How often the energy envelope takes a reading, in seconds. Fine enough to
/// resolve a bar at the fastest tempo worth mixing, coarse enough that an
/// hour of audio costs a few hundred kilobytes.
pub const ENERGY_HOP_SECONDS: f64 = 0.05;

/// Longest stretch the envelope covers. A stream that never ends must not
/// grow the buffer without bound.
const ENERGY_MAX_SECONDS: f64 = 3600.0;

/// Bands the structure analysis splits the signal into.
///
/// A chorus is not just louder than a verse: it is *differently* balanced,
/// with more kick and more cymbals while the midrange makes room for the
/// vocal. Total energy cannot tell the two apart, which is why an earlier
/// attempt at finding choruses from a single envelope kept returning the
/// verse-and-chorus pair as one repeating unit. Three bands are enough to
/// see the difference and cheap enough to run on the audio thread.
pub const NUM_BANDS: usize = 3;

/// Split points between the bands, in Hz.
pub const BAND_EDGES: [f64; NUM_BANDS - 1] = [250.0, 4000.0];

/// A one-pole lowpass, used to split the signal into bands.
///
/// A single pole is deliberate: the point is a coarse balance between
/// bands over seconds of audio, not a clean crossover, and a one-pole costs
/// two multiplies per sample so it can run inside the sink's push.
#[derive(Debug, Clone, Copy, Default)]
struct OnePole {
    state: f64,
    coefficient: f64,
}

impl OnePole {
    fn new(cutoff_hz: f64, sample_rate: f64) -> Self {
        let coefficient = 1.0 - (-std::f64::consts::TAU * cutoff_hz / sample_rate).exp();
        Self {
            state: 0.0,
            coefficient: coefficient.clamp(0.0, 1.0),
        }
    }

    #[inline]
    fn run(&mut self, input: f64) -> f64 {
        self.state += self.coefficient * (input - self.state);
        self.state
    }
}

/// Frames the collector keeps before it stops appending.
fn keep_frames(sample_rate: u32) -> usize {
    (ANALYSED_SECONDS * f64::from(sample_rate)) as usize
}

/// A whole-track energy envelope, one RMS reading per [`ENERGY_HOP_SECONDS`].
///
/// Finding where a track's sections are needs the whole track, but keeping
/// every sample of one would cost hundreds of megabytes. This carries the
/// same structural information for a few hundred kilobytes; the tempo it is
/// read against comes from the bounded sample window kept alongside it.
#[derive(Debug)]
struct Envelope {
    /// One RMS reading per completed hop, in track order.
    values: Vec<f32>,
    /// Sum of squares for the hop in progress.
    partial: f64,
    /// Samples counted into `partial`.
    partial_samples: usize,
    /// Samples per hop, across every channel.
    hop_samples: usize,
    /// Most readings to keep.
    max_values: usize,
    /// Per-band sums of squares for the hop in progress.
    band_partial: [f64; NUM_BANDS],
    /// One reading per band per completed hop, in track order. Each band's
    /// readings are under the same index as `values`, so a hop's balance is
    /// readable across all of them.
    bands: [Vec<f32>; NUM_BANDS],
    /// The filters that split the signal, one per band, run per channel
    /// folded into a single mono path.
    split: [OnePole; NUM_BANDS - 1],
}

impl Envelope {
    fn new(sample_rate: u32) -> Self {
        let hop_samples = (ENERGY_HOP_SECONDS * f64::from(sample_rate)) as usize
            * crate::vis::CHANNELS as usize;
        let rate = f64::from(sample_rate);
        let mut split = [OnePole::default(); NUM_BANDS - 1];
        for (filter, edge) in split.iter_mut().zip(BAND_EDGES) {
            *filter = OnePole::new(edge, rate);
        }
        let max_values = (ENERGY_MAX_SECONDS / ENERGY_HOP_SECONDS) as usize;
        Self {
            values: Vec::new(),
            partial: 0.0,
            partial_samples: 0,
            hop_samples: hop_samples.max(1),
            max_values,
            band_partial: [0.0; NUM_BANDS],
            bands: std::array::from_fn(|_| Vec::with_capacity(max_values)),
            split,
        }
    }

    /// Folds interleaved samples into the envelope. Allocation-free, so the
    /// sink's thread can call it for every packet.
    fn push(&mut self, interleaved: &[f64]) {
        let channels = crate::vis::CHANNELS as usize;
        let mut rest = interleaved;
        while !rest.is_empty() && self.values.len() < self.max_values {
            let take = (self.hop_samples - self.partial_samples).min(rest.len());
            for sample in &rest[..take] {
                self.partial += sample * sample;
            }
            // The bands are split from the same samples, folded to mono so
            // the filters carry one state each rather than one per channel.
            let frames = take / channels;
            for frame in 0..frames {
                let base = frame * channels;
                let mut mono = 0.0;
                for channel in 0..channels {
                    mono += rest[base + channel];
                }
                mono /= channels as f64;
                // Band 0 is everything below the first edge, and so on: each
                // filter's output is removed from what the next one sees.
                let mut remaining = mono;
                for (index, filter) in self.split.iter_mut().enumerate() {
                    let below = filter.run(remaining);
                    self.band_partial[index] += below * below;
                    remaining -= below;
                }
                self.band_partial[NUM_BANDS - 1] += remaining * remaining;
            }
            self.partial_samples += take;
            rest = &rest[take..];
            if self.partial_samples == self.hop_samples {
                let mean = self.partial / self.hop_samples as f64;
                self.values.push(mean.sqrt() as f32);
                // Each band's figure is scaled to the whole hop, mono, so
                // the bands compare with one another rather than with the
                // stereo total.
                let frames = self.hop_samples / channels;
                let scale = 1.0 / frames.max(1) as f64;
                for band in 0..NUM_BANDS {
                    let energy = self.band_partial[band] * scale;
                    self.bands[band].push(energy.sqrt() as f32);
                }
                self.partial = 0.0;
                self.partial_samples = 0;
                self.band_partial = [0.0; NUM_BANDS];
            }
        }
    }

    /// The readings taken so far, oldest first.
    fn rms(&self) -> Vec<f64> {
        self.values.iter().map(|value| f64::from(*value)).collect()
    }

    /// Per-band readings, each the same length as [`Self::rms`].
    fn band_rms(&self) -> [Vec<f64>; NUM_BANDS] {
        std::array::from_fn(|band| {
            self.bands[band]
                .iter()
                .map(|value| f64::from(*value))
                .collect()
        })
    }

    fn clear(&mut self) {
        self.values.clear();
        self.partial = 0.0;
        self.partial_samples = 0;
        self.band_partial = [0.0; NUM_BANDS];
        for band in &mut self.bands {
            band.clear();
        }
        for filter in &mut self.split {
            *filter = OnePole::default();
        }
    }
}

/// Builds an energy envelope from interleaved samples in one pass.
///
/// The probe's audio arrives whole rather than streamed through the sink, so
/// it needs the same envelope without a [`Collector`] to accumulate it.
pub fn envelope_of(interleaved: &[f32]) -> Vec<f64> {
    let hop = (ENERGY_HOP_SECONDS * f64::from(crate::vis::SAMPLE_RATE)) as usize
        * crate::vis::CHANNELS as usize;
    let hop = hop.max(1);
    interleaved
        .chunks(hop)
        .filter(|span| span.len() * 2 >= hop)
        .map(|span| {
            let energy: f64 = span
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum();
            (energy / span.len() as f64).sqrt()
        })
        .collect()
}

/// The total envelope together with its per-band readings.
///
/// The probe's audio arrives whole, so it goes through an [`Envelope`] the
/// same way the playing track does; that keeps one implementation of the
/// band split rather than two that could drift apart.
pub fn envelope_with_bands(interleaved: &[f32]) -> (Vec<f64>, [Vec<f64>; NUM_BANDS]) {
    let sample_rate = crate::vis::SAMPLE_RATE;
    let mut envelope = Envelope::new(sample_rate);
    // The envelope works in interleaved f64, as the sink hands it over.
    let widened: Vec<f64> = interleaved.iter().map(|sample| f64::from(*sample)).collect();
    envelope.push(&widened);
    (envelope.rms(), envelope.band_rms())
}

/// Accumulates the playing track's samples for analysis.
pub struct Collector {
    samples: Mutex<Vec<f32>>,
    limit: usize,
    envelope: Mutex<Envelope>,
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
            envelope: Mutex::new(Envelope::new(sample_rate)),
        })
    }

    /// Appends interleaved frames, narrowing `f64` to `f32` on the way in.
    /// Called from the sink's thread for every decoded packet, so it takes
    /// the samples directly rather than through a copy.
    pub fn push(&self, interleaved: &[f64]) {
        // The envelope sees the whole track; the sample window does not.
        self.envelope
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(interleaved);
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if samples.len() >= self.limit {
            return;
        }
        let room = self.limit - samples.len();
        let take = room.min(interleaved.len());
        samples.extend(interleaved[..take].iter().map(|sample| *sample as f32));
    }

    /// The whole-track energy envelope, one reading per hop.
    pub fn envelope_rms(&self) -> Vec<f64> {
        self.envelope
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .rms()
    }

    /// Per-band readings over the whole track so far, each the same length
    /// as [`Self::envelope_rms`].
    pub fn envelope_bands(&self) -> [Vec<f64>; NUM_BANDS] {
        self.envelope
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .band_rms()
    }

    /// How many energy readings have been taken. Cheap enough to call on
    /// every position update, unlike the envelope itself.
    pub fn envelope_len(&self) -> usize {
        self.envelope
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values
            .len()
    }

    /// Whether nothing has been collected yet.
    pub fn is_empty(&self) -> bool {
        self.samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_empty()
    }

    /// Drops what was collected, for the next track.
    pub fn clear(&self) {
        self.samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        self.envelope
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
    request: Mutex<Option<Sender<Job>>>,
    result: Arc<Mutex<Option<Analysis>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Work handed to the analysis thread.
enum Job {
    /// Full analysis of a track's opening: the beat grid comes from these
    /// samples, and the structure from the envelope and bands covering them.
    Analyse {
        samples: Vec<f32>,
        envelope: Vec<f64>,
        bands: [Vec<f64>; NUM_BANDS],
    },
    /// Re-read the structure from longer readings, keeping the grid that was
    /// already tracked. Much cheaper than another beat-tracking pass, which
    /// is what makes it affordable to do as a track plays.
    Restructure {
        envelope: Vec<f64>,
        bands: [Vec<f64>; NUM_BANDS],
    },
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Worker")
    }
}

impl Worker {
    pub fn spawn(sample_rate: u32) -> Self {
        let (tx, rx): (Sender<Job>, Receiver<Job>) = std::sync::mpsc::channel();
        let result = Arc::new(Mutex::new(None));
        let published = Arc::clone(&result);
        let join = std::thread::Builder::new()
            .name("automix-analysis".into())
            .spawn(move || {
                // Only the newest request of each kind matters. A later
                // envelope is a superset of an earlier one, so the last
                // restructure queued wins; a full analysis supersedes it.
                while let Ok(mut job) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        job = match (job, newer) {
                            (Job::Restructure { .. }, newer @ Job::Restructure { .. }) => newer,
                            (_, newer @ Job::Analyse { .. }) => newer,
                            (job, _) => job,
                        };
                    }
                    let analysed = match job {
                        Job::Analyse {
                            samples,
                            envelope,
                            bands,
                        } => Analysis::of_with_envelope(&samples, sample_rate, &envelope)
                            .map(|mut analysis| {
                                analysis.refresh_bands(&bands);
                                analysis
                            }),
                        Job::Restructure { envelope, bands } => {
                            let mut current: Option<Analysis> = published
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .clone();
                            current.as_mut().map(|analysis| {
                                analysis.refresh_structure(&envelope);
                                analysis.refresh_bands(&bands);
                                analysis.clone()
                            })
                        }
                    };
                    if analysed.is_some() {
                        *published
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner()) = analysed;
                    }
                }
            })
            .ok();
        Self {
            request: Mutex::new(Some(tx)),
            result,
            join,
        }
    }

    /// Queues a track's opening for full analysis. Cheap: it moves buffers.
    pub fn analyse(&self, samples: Vec<f32>, envelope: Vec<f64>, bands: [Vec<f64>; NUM_BANDS]) {
        self.send(Job::Analyse {
            samples,
            envelope,
            bands,
        });
    }

    /// Queues fresh readings so the structure is re-read against the longer
    /// ones, without re-tracking the beat.
    pub fn restructure(&self, envelope: Vec<f64>, bands: [Vec<f64>; NUM_BANDS]) {
        self.send(Job::Restructure { envelope, bands });
    }

    fn send(&self, job: Job) {
        let guard = self
            .request
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(job);
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
        let block = vec![0.1f64; 44_100 * crate::vis::CHANNELS as usize];
        for _ in 0..(ANALYSED_SECONDS as usize + 5) {
            collector.push(&block);
        }
        let held = collector.snapshot().len();
        assert_eq!(held, keep_frames(44_100) * crate::vis::CHANNELS as usize);
    }

    #[test]
    fn clearing_a_collector_frees_it_for_the_next_track() {
        let collector = Collector::new(44_100);
        collector.push(&vec![0.1f64; 4096]);
        assert!(!collector.snapshot().is_empty());
        collector.clear();
        assert!(collector.snapshot().is_empty());
    }

    #[test]
    fn a_collector_is_not_ready_until_half_a_minute_has_been_heard() {
        let rate = 44_100;
        let collector = Collector::new(rate);
        let one_second = vec![0.1f64; rate as usize * crate::vis::CHANNELS as usize];
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

        let envelope = envelope_of(&samples);
        worker.analyse(samples, envelope, Default::default());
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
        worker.analyse(
            vec![0.0f32; 44_100 * 10 * crate::vis::CHANNELS as usize],
            Vec::new(),
            Default::default(),
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(worker.latest().is_none());
    }
}
