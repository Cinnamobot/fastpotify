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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use librespot_core::spotify_uri::SpotifyUri;

use crate::automix::{self, Analysis};
use crate::automix_cuepoints::Cuepoints;
use crate::automix_track::{Collector, Worker, envelope_with_bands};

/// What automix is holding for the track that is playing, for the interface
/// to draw.
///
/// Published from the engine and read by the interface, which run on
/// different threads and never coordinate directly: [`Automix`] owns the
/// planning and this is the one-way copy of what it decided. Every field is
/// what the planner actually used, so a bar drawn from it shows the decision
/// rather than a re-derivation that could disagree with it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AutomixView {
    /// Seconds into the playing track where its fade starts, when a plan is
    /// armed.
    pub fade_out_at: Option<f64>,
    /// Seconds into the incoming track where its fade starts.
    pub fade_in_at: Option<f64>,
    /// The overlap's length in seconds.
    pub overlap: Option<f64>,
    /// The pair's tempo ratio, as the plan was armed with.
    pub tempo_ratio: Option<f64>,
    /// Whether the plan came from the server's cuepoints rather than from the
    /// local analysis, which is what says which of the two placed the edges.
    pub from_cuepoints: bool,
    /// The playing track's own cuepoints, once the lookup landed.
    pub playing_cuepoints: Option<Cuepoints>,
    /// The incoming track's own cuepoints, once the lookup landed.
    pub incoming_cuepoints: Option<Cuepoints>,
}

/// The interface's copy of what automix is holding.
pub type SharedAutomixView = Arc<Mutex<AutomixView>>;

/// Builds the shared view an engine and the interface both hold.
pub fn shared_view() -> SharedAutomixView {
    Arc::new(Mutex::new(AutomixView::default()))
}

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
    /// The track that is playing, by name, so a cue arriving for the incoming
    /// role can be refused when it is really this one's.
    playing_track: Option<SpotifyUri>,
    /// The playing track's own automix cuepoints, once fetched.
    ///
    /// Its fade-out cue is where the overlap starts. `None` for a track the
    /// service has nothing for, and for one whose lookup has not landed yet;
    /// the local analysis covers both cases.
    playing_cuepoints: Option<Cuepoints>,
    /// The track this engine believes plays next, and its cuepoints.
    ///
    /// The two are one value on purpose. The name is what a plan carries so
    /// the player can judge whether it still applies — a manual skip asks "is
    /// this plan for the track I am about to play", and only the destination
    /// can answer it. A cue held against a *different* name would answer that
    /// question for the wrong track, and the player would seek the incoming
    /// track to an offset that means nothing in it. Keeping them together
    /// makes that unrepresentable rather than merely unlikely.
    next: Option<(SpotifyUri, Option<Cuepoints>)>,
    /// Whether the last plan came from the server's cuepoints rather than
    /// from the local analysis. Recorded by [`Self::plan`] so the interface
    /// can tell the two apart.
    plan_came_from_cuepoints: bool,
    /// What the interface draws. Written here whenever the plan or the
    /// cuepoints change, so the bar shows the decision that was made.
    view: SharedAutomixView,
}

impl std::fmt::Debug for Automix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Automix")
    }
}

impl Automix {
    /// Builds the driver, or `None` when automix is off.
    pub fn new(
        collector: Option<Arc<Collector>>,
        sample_rate: u32,
        view: SharedAutomixView,
    ) -> Option<Self> {
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
            next: None,
            playing_track: None,
            playing_cuepoints: None,
            plan_came_from_cuepoints: false,
            view,
        })
    }

    /// Called when a new track starts: the previous grid is no longer the
    /// one playing, and the collector starts over.
    ///
    /// Every piece of the previous track's analysis goes with it. Leaving the
    /// workers' results in place was the bug this closes: the collector is
    /// emptied, so the next track's envelope starts at zero, but `latest()`
    /// still held the previous track's grid and the next tick adopted it —
    /// `(None, Some(_))` in [`Self::tick`] — as the track that had just
    /// started. The watermark went the same way, and since the new track's
    /// envelope begins below it, the structure was never re-read either.
    pub fn track_changed(&mut self) {
        self.collector.clear();
        self.worker.reset();
        self.incoming_worker.reset();
        self.playing = None;
        self.incoming = None;
        self.incoming_audio = Arc::new(Vec::new());
        self.armed = None;
        self.next = None;
        self.playing_track = None;
        self.playing_cuepoints = None;
        self.restructured_at = 0;
        self.publish_current();
    }

    /// Names the track this engine believes plays next.
    ///
    /// The name arrives before the cue does, and from more than one place:
    /// the queue names what follows as soon as a track starts, and the
    /// preload names the same track later. A name that matches one already
    /// held keeps whatever cue is already stored for it, so the earlier
    /// source's lookups are not thrown away by the later one repeating
    /// itself.
    pub fn set_incoming_track(&mut self, track: SpotifyUri) {
        match &self.next {
            // Already named: the preload repeating what the queue said.
            // Nothing changes, and this is called on every event, so it
            // must stay silent rather than fill the log once a second.
            Some((known, _)) if *known == track => {}
            _ => {
                log::info!("automix: the incoming track is {track}");
                self.next = Some((track, None));
            }
        }
    }

    /// Hands over the cuepoints for the track named as the incoming one.
    ///
    /// Ignored when it is not that track's: the answer is worthless without
    /// the name it belongs to, and keeping it would let a plan carry a cue
    /// for one track against another.
    pub fn set_incoming_cuepoints(&mut self, track: &SpotifyUri, cuepoints: Option<Cuepoints>) {
        // A track is never its own successor. The caller looks its answers up
        // by track and caches them, so when a track starts playing the answer
        // it already holds for *itself* — fetched while it was the incoming
        // one — is still on hand, and filing it under the incoming role put
        // the track on both sides of a transition. The plan that produced was
        // a crossfade of the track into itself, and the arrival it drew was
        // that track's own fade-in: a mark at 8s that jumped to 92s a second
        // later, once the real next track was known. Nothing downstream can
        // tell that plan from a real one, so it is refused here.
        if self.playing_track.as_ref() == Some(track) {
            log::debug!("automix: refusing cuepoints for {track}; it is the track playing");
            return;
        }
        match &self.next {
            Some((known, _)) if known == track => {
                self.next = Some((known.clone(), cuepoints));
                self.publish_current();
            }
            _ => log::debug!(
                "automix: dropping cuepoints for {track}; the incoming track is {:?}",
                self.next.as_ref().map(|(known, _)| known)
            ),
        }
    }

    /// The track the armed plan is a transition into, if one is current.
    pub fn incoming_track(&self) -> Option<&SpotifyUri> {
        self.next.as_ref().map(|(track, _)| track)
    }

    /// The cuepoints held for the incoming track, if its lookup has landed.
    pub fn incoming_cuepoints(&self) -> Option<Cuepoints> {
        self.next.as_ref().and_then(|(_, cue)| *cue)
    }

    /// Hands over the playing track's own cuepoints, once the lookup landed.
    ///
    /// Set by the caller rather than fetched here so the network round trip
    /// stays off the event loop that drives [`Self::tick`], which runs several
    /// times a second. The name comes with it because the cue is worthless
    /// without it: [`Self::set_incoming_cuepoints`] refuses an answer for the
    /// track that is playing, and that is only knowable if this one is named.
    pub fn set_playing_cuepoints(&mut self, track: &SpotifyUri, cuepoints: Option<Cuepoints>) {
        self.playing_track = Some(track.clone());
        self.playing_cuepoints = cuepoints;
        self.publish_current();
    }

    /// Takes back an armed plan whose boundary is no longer the one it was
    /// made for, so a track change or a seek does not leave it to fire at
    /// the wrong moment. Returns whether the player has to be told.
    pub fn withdraw_plan(&mut self) -> bool {
        std::mem::take(&mut self.armed).is_some()
    }

    /// Called after a seek. The grid and the collected audio are dropped and
    /// the track is measured again from where the play head now is.
    ///
    /// Keeping the grid was wrong, and measurably so. Its beats are stored as
    /// offsets into the audio the tracker heard, and `plan` re-anchors them at
    /// `collected_from` — so after a seek the whole lattice is translated by
    /// the seek distance instead of staying where the music's beats are.
    /// Measured on a 128 BPM click track, a seek to 37.3s started the lattice
    /// 37.3s further on, which against a 1.875s bar is a rotation of 0.89 of
    /// a bar: the exit would open 1.67s off the beat it was snapped to. Only a
    /// seek of a whole number of bars would have landed right by accident.
    ///
    /// A wrong grid is worse than no grid here, because the whole point of
    /// the grid is to put the exit on a downbeat, and one that is rotated has
    /// lost the only property it was kept for while still looking like it was
    /// used. The cost is real and bounded: the local path arms nothing until
    /// half a minute of the new position has been collected, and it falls on
    /// the tracks the service has no cuepoints for — the ones that are already
    /// the fallback. The cue path does not read the grid at all.
    pub fn seeked(&mut self) {
        self.collector.clear();
        self.collected_from = 0.0;
        self.worker.reset();
        self.playing = None;
        self.restructured_at = 0;
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
    /// A plan that exists is handed over immediately, however far away its
    /// boundary is. When to *act* on a plan is the player's judgement, not
    /// this one's: it preloads when the plan's own lead-in comes within
    /// reach, and that is the only moment at which being early has a cost.
    /// Withholding the plan until then was what left a manual skip with
    /// nothing to read — a skip can be pressed at any moment, and one pressed
    /// early found no plan and so could not start the incoming track at its
    /// own cue.
    ///
    /// Handing it over early is also cheap. The curve render, which is the
    /// only expensive part, returns before doing any work until the probe's
    /// audio has arrived and the pair is actually stretched.
    ///
    /// The plan changes for two reasons. With the server's cuepoints the
    /// first plan is already the final one, since both the tempo and both
    /// edges come from the service. On the local path the first plan carries
    /// an unmatched tempo and is replaced once the probe has been analysed,
    /// and either path can move its exit as the outgoing track's structure
    /// reads differently.
    pub fn take_plan_change(
        &mut self,
        elapsed: Duration,
        out_duration: Duration,
    ) -> Option<Option<automix::Transition>> {
        let mut planned = self.plan(elapsed, out_duration)?;
        match &self.armed {
            // Already holding this exact plan. This is the common path — it
            // runs several times a second — so the render below must not be
            // reachable from here unless the plan is still owed something.
            Some(armed) if *armed == planned => {
                // A plan is handed over as soon as it exists, which is before
                // the probe's audio has arrived, so its first render can
                // legitimately produce nothing. The plan's numbers never
                // change again on the cuepoint path — both edges and the tempo
                // all come from the service — so waiting for the numbers to
                // move would mean the curve was never rendered at all and the
                // overlap quietly fell back to the tail carrying the whole
                // stretch. Retrying while a curve is owed is what attaches it.
                //
                // The retry has to stop when the answer can no longer change.
                // The probe is a fixed window taken before the track starts,
                // so a plan whose overlap begins past its end can never be
                // rendered: re-rendering answers the same way every time, and
                // each answer re-sends the plan to the player. Measured over
                // one session that was 10,773 sends for 136 tracks, the worst
                // a single plan repeated 297 times.
                if planned.tempo_ratio == 1.0
                    || armed.curve.is_some()
                    || self.start_is_past_the_probe(&planned)
                {
                    return None;
                }
            }
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
        self.publish(Some(&planned), self.plan_came_from_cuepoints);
        self.armed = Some(planned.clone());
        Some(Some(planned))
    }

    /// Records what the interface should draw.
    ///
    /// Called wherever the plan or the cuepoints change, which is the only
    /// way the bar can show the decision the engine actually made rather than
    /// a second guess at it. `from_cuepoints` says which of the two sources
    /// placed the edges, so a bar can show a fallback differently from a
    /// server-placed plan — they look the same in the numbers otherwise.
    /// Republishes what is held right now: the armed plan and both cues.
    ///
    /// Called when the cuepoints change. Passing `None` here instead wiped the
    /// plan's own fields, so a cue that landed after a plan was armed erased it
    /// from the interface — which read as "no plan" when the plan was there
    /// all along.
    fn publish_current(&mut self) {
        let armed = self.armed.clone();
        let from_cuepoints = self.plan_came_from_cuepoints;
        self.publish(armed.as_ref(), from_cuepoints);
    }

    fn publish(&mut self, planned: Option<&automix::Transition>, from_cuepoints: bool) {
        let Ok(mut view) = self.view.lock() else {
            return;
        };
        view.fade_out_at = planned.map(|plan| plan.fade_out_at);
        view.fade_in_at = planned.map(|plan| plan.fade_in_at);
        view.overlap = planned.map(|plan| plan.duration.as_secs_f64());
        view.tempo_ratio = planned.map(|plan| plan.tempo_ratio);
        view.from_cuepoints = from_cuepoints && planned.is_some();
        view.playing_cuepoints = self.playing_cuepoints;
        view.incoming_cuepoints = self.next.as_ref().and_then(|(_, cue)| *cue);
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
        // The probe that just arrived supersedes any earlier one, including a
        // run still in flight for a track the queue has since replaced. The
        // reset is what makes that run unable to publish over this one, so the
        // grid is only ever read against the position it was measured from.
        self.incoming_worker.reset();
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
        self.worker.restructure(
            self.collector.envelope_rms(),
            self.collector.envelope_bands(),
        );
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
        if hops_per_bar < 1.0 {
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
    pub fn plan(
        &mut self,
        elapsed: Duration,
        out_duration: Duration,
    ) -> Option<automix::Transition> {
        // The server's own answer first, wherever it exists: it is what the
        // official client mixes from, and the local analysis is measured
        // against it rather than the other way round. Both halves are needed,
        // because the exit comes from the outgoing track's own cue and the
        // overlap's length from the incoming one's lead-in.
        if let (Some(from), Some((_, Some(to)))) = (self.playing_cuepoints, &self.next)
            && let Some(planned) =
                automix::plan_from_cuepoints(&from, to, out_duration, elapsed.as_secs_f64())
        {
            self.plan_came_from_cuepoints = true;
            return Some(planned);
        }
        self.plan_came_from_cuepoints = false;
        if let (Some(from), Some((track, Some(to)))) = (&self.playing_cuepoints, &self.next) {
            let room = out_duration.as_secs_f64() - from.fade_out_at;
            log::debug!(
                "automix: no cuepoint plan for {track}: exit {:.2}s of {:.2}s leaves {room:.2}s \
                 (need 1.5..12.0s), pair {} -> {} BPM, elapsed {:.2}s",
                from.fade_out_at,
                out_duration.as_secs_f64(),
                from.bpm,
                to.bpm,
                elapsed.as_secs_f64(),
            );
        }

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

    /// Whether the overlap would start past everything the probe heard.
    ///
    /// The probe is a fixed window of the incoming track's opening, so a plan
    /// that begins beyond it has no audio to render from and never will. This
    /// is the same test [`Self::render_curve_for`] makes, kept beside it so a
    /// plan can be recognised as unrenderable *before* the render is retried
    /// for it again.
    fn start_is_past_the_probe(&self, planned: &automix::Transition) -> bool {
        let channels = crate::vis::CHANNELS as usize;
        let rate = crate::vis::SAMPLE_RATE;
        let from_ms = (planned.fade_in_at.max(0.0) * 1000.0) as usize;
        let from = from_ms * rate as usize / 1000 * channels;
        from >= self.incoming_audio.len()
    }

    /// Attaches the incoming track's rendered half to a plan, if it needs one.
    ///
    /// Called only when the plan has actually changed. The render is seconds
    /// of audio through the keylock engine, and `take_plan_change` runs on
    /// every position update — several times a second — so doing this on each
    /// call re-rendered the same overlap over and over and starved the sink.
    /// The plan's own three numbers decide whether anything needs rendering,
    /// which is why they are compared without the curve.
    fn render_curve_for(
        &mut self,
        planned: &automix::Transition,
    ) -> Option<Arc<librespot_playback::player::IncomingCurve>> {
        if planned.tempo_ratio == 1.0 {
            return None;
        }
        let channels = crate::vis::CHANNELS as usize;
        let rate = crate::vis::SAMPLE_RATE;
        let from_ms = (planned.fade_in_at.max(0.0) * 1000.0) as usize;
        let frames = (planned.duration.as_secs_f64() * f64::from(rate)) as usize;
        let from = from_ms * rate as usize / 1000 * channels;
        if self.start_is_past_the_probe(planned) {
            // The overlap starts past everything the probe heard. The opening
            // is a fixed window and a boundary can be planned ahead of a
            // chorus well beyond it, so this is expected rather than a fault:
            // the tail carries the whole stretch instead.
            return None;
        }
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

    /// The track the tests pretend is queued next.
    fn incoming_track() -> SpotifyUri {
        SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").expect("a uri")
    }

    /// The track the tests pretend is playing, named so a cue arriving for it
    /// is not mistaken for the incoming one's.
    fn playing_track() -> SpotifyUri {
        SpotifyUri::from_uri("spotify:track:1iVvOJIrgaF3LjW0YI5cLt").expect("a uri")
    }

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

    /// The bug this covers: the local section detector found nothing at all
    /// from a 90-second probe, so every transition into a track started it
    /// from its first sample. With the server's cues in hand the incoming
    /// track is started where its own fade-in cue says, which is what the
    /// planner must actually reach the transition with.
    #[test]
    fn the_servers_cuepoints_place_the_plan_over_the_local_analysis() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        // A playing grid whose local analysis would put the exit elsewhere,
        // so a plan that matches the cues cannot have come from it.
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));
        automix.set_playing_cuepoints(
            &playing_track(),
            Some(Cuepoints {
                fade_in_at: 2.0,
                fade_out_at: 30.0,
                bpm: 128.0,
            }),
        );
        automix.set_incoming_track(incoming_track());
        automix.set_incoming_cuepoints(
            &incoming_track(),
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );

        let planned = automix
            .plan(Duration::from_secs(5), Duration::from_secs(40))
            .expect("the cues give a plan");

        assert!(
            (planned.fade_out_at - 30.0).abs() < 1e-9,
            "the exit is the outgoing track's own cue, got {}",
            planned.fade_out_at
        );
        assert!(
            (planned.fade_in_at - 11.5).abs() < 1e-9,
            "the arrival is the incoming track's own cue, got {}",
            planned.fade_in_at
        );
    }

    /// A track the service has no cues for keeps working: the plan falls
    /// through to the local analysis rather than the transition being lost.
    #[test]
    fn a_track_without_cuepoints_still_plans_locally() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));
        // Only one side answered, which is what a partially covered pair
        // looks like, and what a pair the service knows nothing about looks
        // like is the same fall-through.
        automix.set_playing_cuepoints(&playing_track(), None);
        automix.set_incoming_track(incoming_track());
        automix.set_incoming_cuepoints(
            &incoming_track(),
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );

        let planned = automix.plan(Duration::from_secs(5), Duration::from_secs(40));
        assert!(
            planned.is_some(),
            "the local analysis must still produce a plan"
        );
        assert!(
            planned.unwrap().fade_in_at != 11.5,
            "a local plan cannot be using the cue it was not given"
        );
    }

    /// A track change must drop the cues along with everything else: they
    /// belong to the track that just left, and carrying them over would place
    /// the next transition at the previous track's cue.
    #[test]
    fn a_track_change_forgets_the_cuepoints() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.set_playing_cuepoints(
            &playing_track(),
            Some(Cuepoints {
                fade_in_at: 2.0,
                fade_out_at: 30.0,
                bpm: 128.0,
            }),
        );
        automix.set_incoming_track(incoming_track());
        automix.set_incoming_cuepoints(
            &incoming_track(),
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );
        automix.track_changed();
        assert!(automix.playing_cuepoints.is_none());
        assert!(automix.incoming_cuepoints().is_none());
    }

    /// A plan has to name the track it is a transition into, because a manual
    /// skip asks the player whether the plan still applies to the track it is
    /// about to start. The driver is what knows that name, so it must be the
    /// one that hands it over.
    #[test]
    fn the_driver_names_the_track_a_plan_leads_into() {
        use SpotifyUri;
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        assert!(
            automix.incoming_track().is_none(),
            "nothing is preloaded, so there is nothing to name"
        );
        let next = SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").expect("a uri");
        automix.set_incoming_track(next.clone());
        assert_eq!(automix.incoming_track(), Some(&next));
        // A track change clears it along with everything else about the pair.
        automix.track_changed();
        assert!(automix.incoming_track().is_none());
    }

    /// The bug this covers: the name and the cue arrive from different
    /// events, and the queue can move between them. A cue stored against a
    /// name that is no longer the incoming track would be handed to a plan,
    /// and the player would then seek that track to an offset belonging to
    /// another track — nowhere near its music.
    #[test]
    fn a_cue_for_a_track_that_is_not_incoming_is_dropped() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        let queued = SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").expect("a uri");
        let stale = SpotifyUri::from_uri("spotify:track:0aaKu1ym6qIuoIOsTH8uij").expect("a uri");

        automix.set_incoming_track(queued.clone());
        // A cue for the track that was queued before the queue moved.
        automix.set_incoming_cuepoints(
            &stale,
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );
        assert_eq!(
            automix.incoming_cuepoints(),
            None,
            "a cue for another track must not be kept against this one"
        );
        // The right track's cue is kept.
        automix.set_incoming_cuepoints(
            &queued,
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );
        assert!(
            automix.incoming_cuepoints().is_some(),
            "the named track's own cue is kept"
        );
    }

    /// The queue names the next track at the start of a track, and the
    /// preload names the same one much later. The second naming must not
    /// discard the cue the first one already fetched — that is the whole
    /// point of naming it early.
    #[test]
    fn renaming_the_same_track_keeps_the_cue_already_fetched() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        let next = incoming_track();
        automix.set_incoming_track(next.clone());
        automix.set_incoming_cuepoints(
            &next,
            Some(Cuepoints {
                fade_in_at: 11.5,
                fade_out_at: 25.0,
                bpm: 128.0,
            }),
        );
        // The preload naming the same track again.
        automix.set_incoming_track(next.clone());
        assert!(
            (automix
                .incoming_cuepoints()
                .expect("the cue survives")
                .fade_in_at
                - 11.5)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn a_track_change_forgets_the_previous_grid() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(20.0, 44_100), 44_100).expect("analysable click track"));
        assert!(automix.playing.is_some());
        automix.track_changed();
        assert!(automix.playing.is_none(), "the old grid must not survive");
        assert!(
            !automix.collector.is_ready(44_100),
            "collection starts over"
        );
    }

    #[test]
    fn a_planned_transition_lands_on_the_playing_track() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
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
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
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
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
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
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));
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

    /// A plan is handed over as soon as it exists, and then left alone.
    ///
    /// Handing it over early is what lets a manual skip read the incoming
    /// track's own cue — a skip can be pressed at any moment, including at
    /// the very start of a track, and one pressed early used to find no plan
    /// at all. Once the plan is current it must not be sent again, and must
    /// not be withdrawn as the boundary arrives: the exit *is* the moment
    /// the transition starts.
    #[test]
    fn a_plan_is_handed_over_at_once_and_only_once() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));

        // A plan is handed over as soon as there is one, however far out. It
        // is the player that decides when to act on it, and it does so from
        // the plan's own lead-in; holding it back here left a manual skip
        // with nothing to read.
        assert!(
            matches!(
                automix.take_plan_change(Duration::from_secs(0), Duration::from_secs(240)),
                Some(Some(_))
            ),
            "a plan that exists is handed over at once"
        );
        assert!(
            automix
                .take_plan_change(Duration::from_secs(0), Duration::from_secs(240))
                .is_none(),
            "an unchanged plan must not be re-sent"
        );

        // Reaching the exit must not take the plan away. The exit is the
        // moment the transition starts, so withdrawing it here would turn
        // the boundary into a plain cut — which is what used to happen. The
        // plan's *content* may still move as the play head goes past the
        // section the exit was taken from, which is a revision and not a
        // withdrawal, so it is the withdrawal that is asserted against.
        let near = Duration::from_millis(240_000 - 8_000);
        let at_exit = Duration::from_millis(240_000 - 1_000);
        for elapsed in [near, at_exit] {
            if let Some(change) = automix.take_plan_change(elapsed, Duration::from_secs(240)) {
                assert!(
                    change.is_some(),
                    "the plan was withdrawn at {elapsed:?}, which turns the boundary into a cut"
                );
            }
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
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));
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
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        automix.playing =
            Some(Analysis::of(&clicks(35.0, 44_100), 44_100).expect("analysable click track"));

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
        assert!(
            automix.incoming_analysis().is_some(),
            "the probe produced a grid"
        );

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

    /// The bug this covers: a track becoming its own successor. The caller
    /// caches cuepoints by track, so the moment a track starts playing the
    /// answer it already holds for *itself* — fetched while it was the
    /// incoming one — is still on hand. Filing that under the incoming role
    /// put the track on both sides of a transition, and the arrival the plan
    /// drew was that track's own fade-in: a mark that appeared at 8s and
    /// jumped to 92s a second later, once the real next track was known.
    #[test]
    fn a_track_is_not_taken_as_its_own_successor() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        let cue = Cuepoints {
            fade_in_at: 8.0,
            fade_out_at: 120.0,
            bpm: 128.0,
        };

        // The track playing, whose cue the caller already holds.
        automix.set_playing_cuepoints(&playing_track(), Some(cue));
        // The same track offered for the incoming role, which is what a stale
        // cache entry does the instant the track starts.
        automix.set_incoming_track(playing_track());
        automix.set_incoming_cuepoints(&playing_track(), Some(cue));

        let planned = automix.plan(Duration::from_secs(5), Duration::from_secs(300));
        assert!(
            planned.is_none(),
            "a transition from the playing track into itself is not a transition, \
             but one was planned: {planned:?}"
        );
    }

    /// The bug this covers: the local plan's exit was placed on the wrong
    /// track. `track_changed` dropped the driver's copy of the grid but not
    /// the worker's, so the next tick re-adopted the finished analysis of the
    /// track that had just ended. The watermark went with it, and since the
    /// new track's envelope starts below it, the structure was never re-read
    /// either — every track after the first was planned against the first
    /// one's grid.
    #[test]
    fn a_track_change_does_not_keep_the_previous_tracks_grid() {
        let rate = 44_100u32;
        let collector = Collector::new(rate);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), rate, shared_view()).expect("on");

        push_clicks(&collector, &clicks_at(128.0, 100.0, rate));
        assert!(settle(&mut automix, rate), "the first track is analysed");
        let first = automix.playing.as_ref().expect("analysed");
        assert!(
            (first.bpm - 128.0).abs() < 4.0,
            "tracked {} for a 128 BPM click",
            first.bpm
        );
        assert!(
            automix.restructured_at > 0,
            "the first track's structure was read"
        );

        automix.track_changed();
        assert!(
            automix.restructured_at == 0,
            "the watermark counts readings of a window that was just emptied"
        );
        assert!(
            automix.playing.is_none(),
            "and nothing of the previous track is left to plan from"
        );

        // The new track is a different tempo. Nothing of its audio has been
        // collected yet, so there is nothing to have measured a tempo from:
        // whatever `tick` adopts here would be the previous track's.
        automix.tick(rate, Duration::from_secs(0));
        assert!(
            automix.playing.is_none(),
            "a track change left a grid in place before any of the new track \
             was heard"
        );

        // And once the new track has been heard, its own tempo is what is
        // tracked — not the one the previous track was analysed at.
        push_clicks(&collector, &clicks_at(90.0, 100.0, rate));
        assert!(settle(&mut automix, rate), "the second track is analysed");
        let second = automix.playing.as_ref().expect("analysed");
        assert!(
            (second.bpm - 90.0).abs() < 4.0,
            "track two reports {} BPM: it is reading the previous track's grid",
            second.bpm
        );
    }

    /// Collects interleaved samples the way the sink does.
    fn push_clicks(collector: &Arc<Collector>, samples: &[f32]) {
        let widened: Vec<f64> = samples.iter().map(|s| f64::from(*s)).collect();
        collector.push(&widened);
    }

    /// The bug this covers: a plan whose overlap starts past the probe's
    /// window could never be rendered, so the retry that exists to attach a
    /// curve answered `None` forever — and every answer re-handed the plan to
    /// the player. Measured over one session: 10,773 sends for 136 tracks,
    /// the worst single plan repeated 297 times.
    #[test]
    fn a_plan_that_cannot_be_rendered_is_not_re_offered_forever() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");
        let playing = Cuepoints {
            fade_in_at: 2.0,
            fade_out_at: 30.0,
            bpm: 128.0,
        };
        automix.set_playing_cuepoints(&playing_track(), Some(playing));
        automix.set_incoming_track(incoming_track());
        // The incoming track's own cue starts the overlap far past the 90s
        // probe, which is what a track with a long intro looks like.
        // A different tempo, so the pair is stretched and a curve is owed.
        let incoming = Cuepoints {
            fade_in_at: 200.0,
            fade_out_at: 260.0,
            bpm: 96.0,
        };
        automix.set_incoming_cuepoints(&incoming_track(), Some(incoming));
        // A probe shorter than the point the overlap would begin at.
        automix.incoming(&crate::automix::Probe {
            samples: clicks(10.0, 44_100),
            position_seconds: 0.0,
        });

        // The first offer is legitimate: a plan is handed over as soon as it
        // exists, before any curve is owed.
        let first = automix
            .take_plan_change(Duration::from_secs(5), Duration::from_secs(300))
            .expect("the cues give a plan");
        assert!(first.is_some(), "the plan is offered once");

        // But an unrenderable one is not asked for again, however often the
        // position moves. Ten passes is what a few seconds of playback is.
        for pass in 0..10 {
            let again =
                automix.take_plan_change(Duration::from_secs(5 + pass), Duration::from_secs(300));
            assert!(
                again.is_none(),
                "pass {pass}: the same unrenderable plan was offered again, which \
                 re-sends it to the player on every position update"
            );
        }
    }

    #[test]
    fn automix_is_off_without_a_collector() {
        assert!(Automix::new(None, 44_100, shared_view()).is_none());
    }

    /// The incoming track must start on one of its own downbeats, so both
    /// tracks land their bar together instead of one sliding under the other.
    #[test]
    fn an_incoming_grid_moves_the_start_onto_its_downbeat() {
        let collector = Collector::new(44_100);
        let mut automix =
            Automix::new(Some(Arc::clone(&collector)), 44_100, shared_view()).expect("on");

        // The outgoing track, measured from its start.
        let audio: Vec<f64> = clicks(35.0, 44_100).iter().map(|s| f64::from(*s)).collect();
        collector.push(&audio);
        assert!(
            settle(&mut automix, 44_100),
            "the outgoing grid is published"
        );

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
