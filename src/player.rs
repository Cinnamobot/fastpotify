//! Local Spotify Connect playback through librespot.
//!
//! The engine owns one librespot session, player, mixer, and Spirc (the
//! Connect state machine). Player events are folded into a [`LocalState`]
//! snapshot that is pushed to the interface whenever something changed;
//! commands from the interface go straight to Spirc, which keeps Spotify's
//! cluster state in sync so phones and other clients see what this device
//! is doing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::{
    SpotifyUri,
    authentication::Credentials,
    cache::Cache,
    config::{DeviceType, SessionConfig},
    error::ErrorKind,
    session::Session,
    spotify_id::SpotifyId,
};
use librespot_metadata::{
    Album as MetadataAlbum, Metadata,
    album::AlbumType,
    audio::{AudioItem, UniqueFields},
};
use librespot_playback::{
    audio_backend::{self, Sink},
    config::{AudioFormat, Bitrate, NormalisationType, PlayerConfig, VolumeCtrl},
    mixer::{self, Mixer, MixerConfig, NoOpVolume, VolumeGetter},
    player::{Player, PlayerEvent},
};
use sha1::{Digest, Sha1};

use crate::automix_cuepoints::{Cuepoints, Missing};

use crate::api::models::ArtistRef;
use crate::sink::{AudioControl, ErrorHook, RodioSink};
use crate::vis::{AudioTap, Tapped};

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub device_name: String,
    pub bitrate_kbps: u16,
    pub normalisation: bool,
    pub autoplay: bool,
    pub gapless: bool,
    /// Overlap the end of one track with the start of the next. `Duration::ZERO`
    /// keeps tracks strictly sequential.
    pub crossfade: Duration,
    pub backend: Option<String>,
    pub audio_device: Option<String>,
    pub initial_volume: u16,
    pub volume_dir: PathBuf,
    pub audio_cache_dir: Option<PathBuf>,
    pub audio_cache_limit: Option<u64>,
    /// Output buffer length in milliseconds.
    pub buffer_ms: u32,
    pub tap: Arc<AudioTap>,
    /// The equalizer's settings, shared with the window that sets them.
    pub eq: crate::eq::SharedEq,
    /// Collects the playing track for automix's beat analysis, when automix
    /// is on. `None` leaves the audio path untouched.
    pub analysis: Option<Arc<crate::automix_track::Collector>>,
    /// What automix is holding, for the interface to draw. Shared rather than
    /// queried because the engine that decides and the interface that shows
    /// it are separate threads with no channel between them for this.
    pub automix_view: crate::automix_driver::SharedAutomixView,
}

impl EngineConfig {
    /// A stable Connect device id derived from the name, so Spotify keeps
    /// recognising this computer across restarts.
    pub fn device_id(&self) -> String {
        hex(&Sha1::digest(self.device_name.as_bytes()))
    }

    pub fn open_cache(&self) -> Result<Cache> {
        Cache::new(
            None,
            Some(self.volume_dir.as_path()),
            self.audio_cache_dir.as_deref(),
            self.audio_cache_limit,
        )
        .map(Cache::with_memory_credentials)
        .context("unable to open the playback cache")
    }

    fn bitrate(&self) -> Bitrate {
        match self.bitrate_kbps {
            96 => Bitrate::Bitrate96,
            160 => Bitrate::Bitrate160,
            _ => Bitrate::Bitrate320,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    Context,
    Track,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Context,
            Self::Context => Self::Track,
            Self::Track => Self::Off,
        }
    }

    pub fn api_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Context => "context",
            Self::Track => "track",
        }
    }

    pub fn from_api(name: &str) -> Self {
        match name {
            "context" => Self::Context,
            "track" => Self::Track,
            _ => Self::Off,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalTrack {
    pub uri: String,
    pub title: String,
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub art_url: Option<String>,
    pub art_small_url: Option<String>,
    pub duration_ms: u32,
    pub is_episode: bool,
}

impl LocalTrack {
    pub fn artist_names(&self) -> String {
        crate::api::models::join_names(self.artists.iter().map(|artist| artist.name.as_str()))
    }
}

/// Why the engine could not play what it was given.
///
/// Carried apart from the message, because the two call for different
/// responses and a message is for reading, not for deciding on: the message
/// wording is the interface's, and matching it was how the severity of an
/// audio-key refusal came to be missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackFailure {
    /// Spotify has nothing at this URI — one track's problem, not the
    /// session's.
    Unavailable,
    /// The session refused the audio key itself. The engine stops rather
    /// than skipping, and every later track fails the same way: this is the
    /// session, and only a fresh one clears it.
    AudioKeyRefused,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalState {
    pub playback: Playback,
    pub track: Option<LocalTrack>,
    pub position_ms: u32,
    /// When `position_ms` was observed; `None` while not advancing.
    pub position_at: Option<Instant>,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    /// The librespot engine's Spotify session is alive. Connect device
    /// activity is separate: Spotify may make this device inactive while the
    /// session remains ready to be activated by the next load.
    pub connected: bool,
    pub username: String,
    pub active_client: String,
    pub error: Option<String>,
    /// What kind of failure `error` describes, for callers that have to act
    /// on it rather than show it.
    pub failure: Option<PlaybackFailure>,
    pub seek_sequence: u64,
}

/// What local playback was doing when its session ended, so the engine
/// can pick it up again after reconnecting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interrupted {
    pub uri: String,
    pub position_ms: u32,
    /// Playing or loading, as opposed to paused.
    pub playing: bool,
}

impl LocalState {
    /// The track and position to come back to, if something was on.
    pub fn interrupted(&self) -> Option<Interrupted> {
        let track = self.track.as_ref()?;
        if self.playback == Playback::Stopped {
            return None;
        }
        Some(Interrupted {
            uri: track.uri.clone(),
            position_ms: self.position_now(),
            playing: matches!(self.playback, Playback::Playing | Playback::Loading),
        })
    }

    /// The position now, interpolated from the last report while playing.
    pub fn position_now(&self) -> u32 {
        match (self.playback, self.position_at) {
            (Playback::Playing, Some(at)) => {
                let elapsed = at.elapsed().as_millis() as u32;
                let limit = self
                    .track
                    .as_ref()
                    .map_or(u32::MAX, |track| track.duration_ms.max(self.position_ms));
                self.position_ms.saturating_add(elapsed).min(limit)
            }
            _ => self.position_ms,
        }
    }

    pub fn is_active(&self) -> bool {
        self.track.is_some() && self.playback != Playback::Stopped
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadSpec {
    pub context_uri: Option<String>,
    pub uris: Vec<String>,
    pub offset_uri: Option<String>,
    pub offset_index: Option<u32>,
    pub position_ms: u32,
    pub play: bool,
    pub shuffle: Option<bool>,
    /// Play what Spotify would follow `context_uri` with, its autoplay
    /// station, rather than the context itself.
    pub autoplay: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerCommand {
    Toggle,
    Next,
    Previous,
    /// Remove manually queued tracks and keep context tracks.
    ClearQueue,
    /// Queue a track or episode after the ones already queued.
    AddToQueue(String),
    Seek(u32),
    /// The volume to keep: applied at once and told to Spotify Connect.
    Volume(u16),
    /// The slider mid-drag: applied at once, nothing sent. Every Connect
    /// update costs a round trip to Spotify, and librespot makes them one
    /// after another, so dragging through fifty values lagged by seconds.
    VolumePreview(u16),
    Shuffle(bool),
    Repeat(RepeatMode),
    Load(LoadSpec),
    Activate,
}

#[allow(clippy::large_enum_variant)]
pub enum EngineEvent {
    State(LocalState),
    SessionEnded,
}

pub type Notify = Arc<dyn Fn(EngineEvent) + Send + Sync>;

pub struct Engine {
    player: Arc<Player>,
    spirc: Arc<Spirc>,
    session: Session,
    mixer: Arc<dyn Mixer>,
    device_id: String,
    state: Arc<Mutex<LocalState>>,
    /// What was playing when the session ended on its own.
    interrupted: Arc<Mutex<Option<Interrupted>>>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    audio: Arc<AudioControl>,
    /// How long a track change overlaps. Zero means no crossfade, which is
    /// what decides whether a skip may be played as a mix or has to cut.
    crossfade: Duration,
}

impl Engine {
    pub(crate) fn credentials(&self) -> Option<Credentials> {
        self.session.cache().and_then(|cache| cache.credentials())
    }
    /// Connects to Spotify and announces this device on Spotify Connect.
    pub async fn connect(
        config: &EngineConfig,
        credentials: Credentials,
        cache: Cache,
        notify: Notify,
    ) -> Result<Self> {
        let device_id = config.device_id();
        let session_config = SessionConfig {
            device_id: device_id.clone(),
            autoplay: Some(config.autoplay),
            ..SessionConfig::default()
        };
        let normalisation_factor = Arc::new(std::sync::atomic::AtomicU64::new(1.0f64.to_bits()));
        let player_config = PlayerConfig {
            bitrate: config.bitrate(),
            gapless: config.gapless,
            crossfade: config.crossfade,
            normalisation: config.normalisation,
            normalisation_type: NormalisationType::Auto,
            position_update_interval: Some(Duration::from_secs(1)),
            // The fork reports each track's normalisation factor here, so
            // the tap can undo it for the visualisers: they show the music,
            // not the loudness housekeeping.
            normalisation_report: Some(Arc::clone(&normalisation_factor)),
            ..PlayerConfig::default()
        };

        let mixer_builder =
            mixer::find(Some("softvol")).ok_or_else(|| anyhow!("soft volume mixer missing"))?;
        // librespot's default curve spans 60 dB logarithmically, which puts
        // half the slider below -30 dB and every level anyone wants in its
        // top quarter. The cubic curve reaches -16 dB at the middle and -7 dB
        // at three quarters, spreading the useful range across the slider.
        let mixer = mixer_builder(MixerConfig {
            volume_ctrl: VolumeCtrl::Cubic(VolumeCtrl::DEFAULT_DB_RANGE),
            ..MixerConfig::default()
        })
        .context("unable to create the mixer")?;

        let state = Arc::new(Mutex::new(LocalState {
            volume: config.initial_volume,
            ..LocalState::default()
        }));
        let session = Session::new(session_config, Some(cache));
        let audio = AudioControl::new(config.buffer_ms);
        let (sink_builder, volume) = sink_builder(
            config,
            Arc::clone(&state),
            Arc::clone(&notify),
            &mixer,
            Arc::clone(&normalisation_factor),
            Arc::clone(&audio),
        );
        let player = Player::new(player_config, session.clone(), volume, sink_builder);
        let events = player.get_player_event_channel();
        tokio::spawn(run_events(
            events,
            Arc::clone(&state),
            Arc::clone(&notify),
            Arc::clone(&audio),
            crate::automix_driver::Automix::new(
                config.analysis.clone(),
                crate::vis::SAMPLE_RATE,
                config.automix_view.clone(),
            ),
            Arc::clone(&player),
            session.clone(),
        ));

        let connect_config = ConnectConfig {
            name: config.device_name.clone(),
            device_type: DeviceType::Computer,
            initial_volume: config.initial_volume,
            disable_volume: false,
            volume_steps: 64,
            ..ConnectConfig::default()
        };
        let (spirc, spirc_task) = Spirc::new(
            connect_config,
            session.clone(),
            credentials,
            Arc::clone(&player),
            Arc::clone(&mixer),
        )
        .await
        .context("unable to connect to Spotify")?;

        {
            let mut current = state.lock().unwrap_or_else(|p| p.into_inner());
            current.connected = true;
            current.username = session.username();
            notify(EngineEvent::State(current.clone()));
        }

        let shutting_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interrupted: Arc<Mutex<Option<Interrupted>>> = Arc::default();
        let ended_flag = Arc::clone(&shutting_down);
        let ended_notify = Arc::clone(&notify);
        let ended_state = Arc::clone(&state);
        let ended_interrupted = Arc::clone(&interrupted);
        tokio::spawn(async move {
            spirc_task.await;
            {
                let mut current = ended_state.lock().unwrap_or_else(|p| p.into_inner());
                // Kept before the state is marked stopped, so a reconnect
                // knows what to pick up.
                *ended_interrupted.lock().unwrap_or_else(|p| p.into_inner()) =
                    current.interrupted();
                current.connected = false;
                current.playback = Playback::Stopped;
                current.position_at = None;
                ended_notify(EngineEvent::State(current.clone()));
            }
            if !ended_flag.load(std::sync::atomic::Ordering::SeqCst) {
                ended_notify(EngineEvent::SessionEnded);
            }
        });

        Ok(Self {
            player,
            spirc: Arc::new(spirc),
            session,
            mixer,
            device_id,
            state,
            interrupted,
            shutting_down,
            audio,
            crossfade: config.crossfade,
        })
    }

    /// Playback state to resume after replacing this engine.
    pub fn interrupted(&self) -> Option<Interrupted> {
        let ended = self
            .interrupted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        ended.or_else(|| {
            self.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .interrupted()
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Whether Spotify classifies this album as an EP in its internal metadata.
    pub(crate) async fn album_is_ep(&self, album_uri: &str) -> Result<bool> {
        let uri = SpotifyUri::from_uri(album_uri).context("invalid album URI")?;
        let album = MetadataAlbum::get(&self.session, &uri)
            .await
            .context("album metadata")?;
        Ok(album.album_type == AlbumType::EP)
    }

    /// Spotify's own transcription of a track, as the raw JSON its clients
    /// read; `Ok(None)` when Spotify has none, an error when asking failed.
    pub async fn lyrics_json(&self, track_uri: &str) -> Result<Option<serde_json::Value>> {
        let Some(id) = track_uri
            .rsplit(':')
            .next()
            .and_then(|id| SpotifyId::from_base62(id).ok())
        else {
            return Ok(None);
        };
        match self.session.spclient().get_lyrics(&id).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(error) if error.kind == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(anyhow!("spotify lyrics: {error}")),
        }
    }

    /// Account playlist tree in Spotify order, including folder markers,
    /// and which of its playlists the account may add songs to.
    pub async fn rootlist(&self) -> Result<Rootlist> {
        use protobuf::Message as _;
        let mut uris = Vec::new();
        let mut editable = std::collections::BTreeSet::new();
        let mut from = 0usize;
        loop {
            let bytes = self
                .session
                .spclient()
                .get_rootlist(from, Some(500))
                .await
                .map_err(|error| anyhow!("rootlist: {error}"))?;
            let content =
                librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(
                    &bytes,
                )?;
            let Some(contents) = content.contents.into_option() else {
                break;
            };
            let count = contents.items.len();
            let truncated = contents.truncated();
            editable.extend(editable_uris(&contents));
            uris.extend(contents.items.into_iter().filter_map(|item| item.uri));
            if !truncated || count == 0 {
                break;
            }
            from += count;
        }
        Ok(Rootlist {
            entries: parse_rootlist(&uris),
            editable,
        })
    }

    /// The display name behind a user id, from the profile view Spotify's
    /// clients read; `None` when nothing answers.
    pub async fn user_display_name(&self, user_id: &str) -> Option<String> {
        let bytes = self
            .session
            .spclient()
            .get_user_profile(user_id, Some(0), Some(0))
            .await
            .ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        json.get("name")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    pub fn shutdown(&self) {
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.spirc.shutdown();
        self.player.stop();
    }

    pub fn command(&self, command: PlayerCommand) -> Result<()> {
        let interrupts_audio = command_interrupts_audio(
            &self.state.lock().unwrap_or_else(|p| p.into_inner()),
            &command,
        );
        // A skip that will be crossfaded must not be interrupted: the
        // interrupt fades the queue out and rebuilds the output, which
        // discards the mix that is the whole point of the crossfade and is
        // heard as the track being cut. The crossfade itself covers the
        // handover, so the interrupt is only for the case with no overlap.
        if interrupts_audio && !self.crossfade_covers_skip(&command) {
            self.audio.interrupt();
        }
        let result = self.send_command(command);
        if interrupts_audio && result.is_err() {
            self.audio.stopped();
        }
        result
    }

    /// Whether a skip will be played as a crossfade, in which case the audio
    /// path must be left alone.
    fn crossfade_covers_skip(&self, command: &PlayerCommand) -> bool {
        skip_is_mixed(command, self.crossfade)
    }


    fn send_command(&self, command: PlayerCommand) -> Result<()> {
        let spirc = &self.spirc;
        match command {
            PlayerCommand::Toggle => spirc.play_pause()?,
            PlayerCommand::Next => spirc.next()?,
            PlayerCommand::Previous => spirc.prev()?,
            PlayerCommand::ClearQueue => spirc.clear_queue()?,
            PlayerCommand::AddToQueue(uri) => spirc.add_to_queue(uri)?,
            PlayerCommand::Seek(position_ms) => spirc.set_position_ms(position_ms)?,
            PlayerCommand::Volume(volume) => {
                self.mixer.set_volume(volume);
                spirc.set_volume(volume)?;
            }
            PlayerCommand::VolumePreview(volume) => self.mixer.set_volume(volume),
            PlayerCommand::Shuffle(enabled) => spirc.shuffle(enabled)?,
            PlayerCommand::Repeat(mode) => match mode {
                RepeatMode::Off => {
                    spirc.repeat_track(false)?;
                    spirc.repeat(false)?;
                }
                RepeatMode::Context => {
                    spirc.repeat_track(false)?;
                    spirc.repeat(true)?;
                }
                RepeatMode::Track => {
                    spirc.repeat(false)?;
                    spirc.repeat_track(true)?;
                }
            },
            PlayerCommand::Activate => spirc.activate()?,
            PlayerCommand::Load(spec) => {
                let playing_track = spec
                    .offset_uri
                    .clone()
                    .map(PlayingTrack::Uri)
                    .or_else(|| spec.offset_index.map(PlayingTrack::Index));
                let context_options = if spec.autoplay {
                    Some(LoadContextOptions::Autoplay)
                } else {
                    spec.shuffle.map(|shuffle| {
                        LoadContextOptions::Options(Options {
                            shuffle,
                            ..Options::default()
                        })
                    })
                };
                let options = LoadRequestOptions {
                    start_playing: spec.play,
                    seek_to: spec.position_ms,
                    playing_track,
                    context_options,
                };
                let request = if let Some(context) = spec.context_uri {
                    LoadRequest::from_context_uri(context, options)
                } else if !spec.uris.is_empty() {
                    LoadRequest::from_tracks(spec.uris, options)
                } else {
                    anyhow::bail!("nothing to play");
                };
                spirc.activate()?;
                spirc.load(request)?;
            }
        }
        Ok(())
    }
}

fn command_interrupts_audio(state: &LocalState, command: &PlayerCommand) -> bool {
    state.playback == Playback::Playing
        && matches!(
            command,
            PlayerCommand::Next | PlayerCommand::Previous | PlayerCommand::Load(_)
        )
}

/// Whether a track change is played as an overlap rather than a cut.
///
/// A skipping command is mixed when a crossfade is configured, because the
/// overlap is what covers the handover. A `Load` names its own track and is
/// often a fresh start rather than a mix, so it always cuts.
fn skip_is_mixed(command: &PlayerCommand, crossfade: Duration) -> bool {
    matches!(
        command,
        PlayerCommand::Next | PlayerCommand::Previous
    ) && !crossfade.is_zero()
}

/// Builds the audio sink and chooses where volume is applied.
///
/// The default sink opens the device on playback and reports errors instead
/// of panicking. It applies volume at output so changes affect queued audio.
/// Other librespot backends remain available through Settings.
type SinkAndVolume = (
    Box<dyn FnOnce() -> Box<dyn Sink> + Send>,
    Box<dyn VolumeGetter + Send>,
);

fn sink_builder(
    config: &EngineConfig,
    state: Arc<Mutex<LocalState>>,
    notify: Notify,
    mixer: &Arc<dyn Mixer>,
    normalisation: Arc<std::sync::atomic::AtomicU64>,
    audio: Arc<AudioControl>,
) -> SinkAndVolume {
    let device = config.audio_device.clone();
    let buffer_ms = config.buffer_ms;
    let tap = Arc::clone(&config.tap);
    let eq = Arc::clone(&config.eq);
    let analysis = config.analysis.clone();
    let report: ErrorHook = Arc::new(move |message: String| {
        let snapshot = {
            let mut current = state.lock().unwrap_or_else(|p| p.into_inner());
            current.error = Some(message);
            current.clone()
        };
        notify(EngineEvent::State(snapshot));
    });
    if let Some(name) = config
        .backend
        .as_deref()
        .filter(|name| *name != crate::sink::NAME)
    {
        match audio_backend::find(Some(name.to_string())) {
            Some(builder) => {
                // Apply volume after the tap so visualizers are independent of
                // volume, including at zero.
                let applied = mixer.get_soft_volume();
                let normalisation = Arc::clone(&normalisation);
                let analysis = analysis.clone();
                return (
                    Box::new(move || {
                        let sink = builder(device, AudioFormat::S16);
                        Box::new(Tapped::new(
                            sink,
                            tap,
                            applied,
                            true,
                            eq,
                            normalisation,
                            analysis,
                        )) as Box<dyn Sink>
                    }),
                    Box::new(NoOpVolume),
                );
            }
            None => log::warn!("audio backend {name:?} is unavailable; using the default"),
        }
    }
    let volume = mixer.get_soft_volume();
    // The output applies volume to queued audio. The wrapper reads the same
    // value to calculate the pre-volume limiter ceiling.
    let ceiling = mixer.get_soft_volume();
    (
        Box::new(move || {
            let sink = Box::new(RodioSink::new(device, report, volume, buffer_ms, audio));
            Box::new(Tapped::new(
                sink,
                tap,
                ceiling,
                false,
                eq,
                normalisation,
                analysis,
            )) as Box<dyn Sink>
        }),
        Box::new(NoOpVolume),
    )
}

async fn run_events(
    mut events: tokio::sync::mpsc::UnboundedReceiver<PlayerEvent>,
    state: Arc<Mutex<LocalState>>,
    notify: Notify,
    audio: Arc<AudioControl>,
    automix: Option<crate::automix_driver::Automix>,
    player: Arc<Player>,
    session: Session,
) {
    let mut play_request_id = None;
    let mut automix = automix;
    // The server's automix cuepoints, in flight and already held. Looked up
    // per track rather than per pair, because the same track's answer serves
    // both ends of every transition it takes part in.
    let mut cuepoints = CuepointFetches::default();
    // Automix needs the track to have played a while before its grid exists,
    // so the check rides the same per-second position updates the interface
    // already receives rather than a timer of its own.
    while let Some(event) = events.recv().await {
        if let PlayerEvent::PlayRequestIdChanged {
            play_request_id: next,
        } = &event
        {
            play_request_id = Some(*next);
            continue;
        }
        if let (Some(current), Some(incoming)) = (play_request_id, event.get_play_request_id())
            && current != incoming
        {
            // Dropped as belonging to another play request. Worth a line: a
            // preload is answered under the request that asked for it, and one
            // dropped here never becomes `Ready`.
            log::debug!("automix: dropping {event:?}, it is for play request {incoming}, not {current}");
            continue;
        }
        // The preload handshake is the one thing automix cannot work without:
        // no probe means no incoming grid, and no `Ready` means the player has
        // nothing to mix in at the boundary. None of these events reach the
        // interface, so without a line here a preload that never happens and
        // one that is merely late look the same from the log.
        match &event {
            PlayerEvent::TimeToPreloadNextTrack { track_id, .. } => {
                log::debug!("preload: the player asked for the next track ({track_id})")
            }
            PlayerEvent::UpcomingTrack { track_id } => {
                log::debug!("preload: the queue names {track_id} as next")
            }
            PlayerEvent::Preloading { track_id } => {
                log::debug!("preload: {track_id} is ready to mix in")
            }
            PlayerEvent::IncomingPreloaded { track_id, probe } => log::debug!(
                "preload: {track_id} probed, {} samples from {:.1}s",
                probe.samples.len(),
                f64::from(probe.position_ms) / 1000.0
            ),
            _ => {}
        }
        match &event {
            PlayerEvent::TrackChanged { .. } => {
                audio.track_changed();
                if let Some(automix) = &mut automix {
                    // The player may still be holding the plan for the
                    // boundary that just passed, and a new track has no use
                    // for it.
                    if automix.withdraw_plan() {
                        player.set_crossfade_plan(None);
                    }
                    automix.track_changed();
                }
            }
            PlayerEvent::Seeked { .. } => {
                audio.track_changed();
                if let Some(automix) = &mut automix {
                    if automix.withdraw_plan() {
                        player.set_crossfade_plan(None);
                    }
                    automix.seeked();
                }
            }
            PlayerEvent::Stopped { .. } => audio.stopped(),
            _ => {}
        }
        if let Some(automix) = &mut automix {
            if let PlayerEvent::IncomingPreloaded { track_id, probe } = &event {
                automix.set_incoming_track(track_id.clone());
                automix.incoming(&crate::automix::Probe {
                    samples: probe.samples.clone(),
                    position_seconds: f64::from(probe.position_ms) / 1000.0,
                });
            }
            drive_automix(automix, &player, &state, &event);
        }
        // The server's own automix cuepoints, as soon as the track they
        // belong to is known. Each lookup is independent of the plan that
        // uses it, so neither waits on the other: the playing track's is
        // needed long before its own boundary, and the preloaded track's
        // before the boundary it arrives at.
        match &event {
            PlayerEvent::TrackChanged { audio_item } => {
                cuepoints.want(CuepointSlot::Playing, &audio_item.track_id);
            }
            // The queue names what follows as soon as a track starts, which
            // is far earlier than the preload decides it. Looking it up here
            // is what gives a manual skip an answer to use: a skip can be
            // pressed at any moment, including at the very start of a track,
            // and one that had to fetch first would either stall or fall back
            // to playing the track from its first sample.
            PlayerEvent::UpcomingTrack { track_id } => {
                cuepoints.want(CuepointSlot::Incoming, track_id);
            }
            PlayerEvent::IncomingPreloaded { track_id, .. } => {
                cuepoints.want(CuepointSlot::Incoming, track_id);
            }
            _ => {}
        }
        if let Some(automix) = automix.as_mut() {
            cuepoints.collect(&session, automix);
        }
        let snapshot = {
            let mut current = state.lock().unwrap_or_else(|p| p.into_inner());
            if apply_event(&mut current, event) {
                Some(current.clone())
            } else {
                None
            }
        };
        if let Some(snapshot) = snapshot {
            notify(EngineEvent::State(snapshot));
        }
    }
}

/// Arms the coming boundary with a planned transition when one is ready.
///
/// Called as events arrive. Planning needs the outgoing track's grid and
/// its length, and arming is only useful once per boundary, so the work is
/// skipped when the player is not near one.
fn drive_automix(
    automix: &mut crate::automix_driver::Automix,
    player: &Arc<Player>,
    state: &Arc<Mutex<LocalState>>,
    event: &PlayerEvent,
) {
    // Only position updates and track starts move this forward; anything
    // else would just repeat the same decision.
    if !matches!(
        event,
        PlayerEvent::PositionChanged { .. }
            | PlayerEvent::PositionCorrection { .. }
            | PlayerEvent::Playing { .. }
    ) {
        return;
    }
    let (elapsed, duration_ms) = {
        let current = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(track) = current.track.as_ref() else {
            return;
        };
        (
            Duration::from_millis(u64::from(current.position_now())),
            track.duration_ms,
        )
    };
    let out_duration = Duration::from_millis(u64::from(duration_ms));
    automix.tick(crate::vis::SAMPLE_RATE, elapsed);
    // `None` means the plan is unchanged; `Some(..)` arms one, once.
    let Some(planned) = automix.take_plan_change(elapsed, out_duration) else {
        return;
    };
    let plan = planned.map(|planned| {
        log::debug!(
            "automix: arming a {:.2}s transition, exiting at {:.2}s, starting the next at {:.2}s, tail at {:.4}x, stretch {}",
            planned.duration.as_secs_f64(),
            planned.fade_out_at,
            planned.fade_in_at,
            planned.tempo_ratio,
            if planned.curve.is_some() {
                "shared by both decks"
            } else if planned.tempo_ratio == 1.0 {
                "none (no incoming grid)"
            } else {
                "carried by the tail alone"
            }
        );
        librespot_playback::player::CrossfadePlan {
            duration: planned.duration,
            fade_out_before_end: out_duration
                .saturating_sub(Duration::from_secs_f64(planned.fade_out_at)),
            fade_in_at: Duration::from_secs_f64(planned.fade_in_at),
            tempo_rate: planned.tempo_ratio,
            curve: planned.curve,
            // Named so the player can tell whether this plan still applies to
            // the track a manual skip is about to start. Without the name, a
            // skip would have no way to know the offset belongs to that
            // track, and seeking a track to another track's offset lands
            // nowhere near the music.
            incoming_track: automix.incoming_track().cloned(),
        }
    });
    player.set_crossfade_plan(plan);
}

/// Which end of the coming pair a lookup is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CuepointSlot {
    /// The track that is playing, whose fade-out cue begins the overlap.
    Playing,
    /// The preloaded track, whose fade-in cue ends it.
    Incoming,
}

/// One role's wish for a track.
///
/// The track is all that is kept: an answer is looked up by track, and the
/// engine is told the same answer again on each pass rather than this
/// remembering what it was told. That is harmless — the engine republishes
/// the plan it holds along with the cue, so the write is idempotent — and it
/// is one less thing that can disagree with the cache the answer came from.
#[derive(Default)]
struct Wanted {
    /// The track this role needs an answer for.
    track: Option<SpotifyUri>,
}

/// The cuepoint lookups this engine has going, and the answers it holds.
///
/// The service answers per *track*, not per role: a track's own fade-in and
/// fade-out cues are the same pair whichever side of a transition it takes
/// part in. So a track is looked up once and its answer serves every role it
/// later takes — which is not what happened before this cache existed. A
/// track was fetched as the incoming one while it was preloaded, and then
/// fetched again the moment it started playing, because the two roles tracked
/// their lookups separately and neither knew the other had already asked.
/// That was two requests for one answer, on every track.
#[derive(Default)]
struct CuepointFetches {
    playing: Wanted,
    incoming: Wanted,
    /// Lookups in flight, by track, so a track is asked for at most once.
    pending: Vec<(
        SpotifyUri,
        tokio::sync::oneshot::Receiver<Result<Cuepoints, Missing>>,
    )>,
    /// Answers already fetched, and the tracks the service had nothing for.
    /// Small: a pair needs two, and a back-skip reaches one more.
    answers: Vec<(SpotifyUri, Option<Cuepoints>)>,
    /// Lookups that did not complete, by track, with when they last failed
    /// and how many times. Kept apart from `answers` so a failure is retried
    /// instead of being remembered as the service having no cuepoints.
    failed: Vec<(SpotifyUri, Instant, u32)>,
}

/// Tracks whose answers are kept. A transition needs the playing track and
/// the incoming one, and a skip back needs the one that just left.
const REMEMBERED_TRACKS: usize = 8;

/// How many times a track's cuepoints are asked for before a failure is
/// treated as the service not having them. A request that never succeeds is
/// not worth asking for on every event; one that failed once is.
const MAX_LOOKUP_TRIES: u32 = 3;

/// How long to wait before asking again, by how many attempts have failed.
///
/// The first retry is quick, because the common failure is a moment's
/// session trouble and the answer is needed before the boundary. Later ones
/// back off so a service that is genuinely refusing is not hammered.
fn retry_after(tries: u32) -> Duration {
    match tries {
        0 | 1 => Duration::from_secs(2),
        2 => Duration::from_secs(10),
        _ => Duration::from_secs(30),
    }
}

impl CuepointFetches {
    /// Records that a role needs an answer for `track`.
    fn want(&mut self, slot: CuepointSlot, track: &SpotifyUri) {
        let wanted = match slot {
            CuepointSlot::Playing => &mut self.playing,
            CuepointSlot::Incoming => &mut self.incoming,
        };
        if wanted.track.as_ref() == Some(track) {
            return;
        }
        log::debug!(
            "automix: {} now needs cuepoints for {track}",
            match slot {
                CuepointSlot::Playing => "the playing track",
                CuepointSlot::Incoming => "the incoming track",
            }
        );
        wanted.track = Some(track.clone());
    }

    /// The answer held for `track`, if one has been settled already.
    ///
    /// `Some(None)` means the service answered and has nothing for it, which
    /// is an answer in its own right and must not be asked for again. A
    /// lookup that *failed* is not held here at all: it says nothing about
    /// the track, so treating it as an answer took the transition away for
    /// the rest of the session.
    fn answer_for(&self, track: &SpotifyUri) -> Option<Option<Cuepoints>> {
        self.answers
            .iter()
            .rev()
            .find(|(known, _)| known == track)
            .map(|(_, answer)| *answer)
    }

    /// Whether `track` is worth asking for: nothing settled for it, none on
    /// its way, and any earlier failure has had time to clear.
    fn needs_lookup(&self, track: &SpotifyUri, now: Instant) -> bool {
        self.answer_for(track).is_none()
            && !self.pending.iter().any(|(asked, _)| asked == track)
            && self
                .failed
                .iter()
                .rev()
                .find(|(known, _, _)| known == track)
                .is_none_or(|(_, at, tries)| {
                    now.duration_since(*at) >= retry_after(*tries) && *tries < MAX_LOOKUP_TRIES
                })
    }

    fn remember(&mut self, track: SpotifyUri, answer: Option<Cuepoints>) {
        self.answers.retain(|(known, _)| known != &track);
        self.answers.push((track, answer));
        if self.answers.len() > REMEMBERED_TRACKS {
            self.answers.remove(0);
        }
    }

    /// Records that a lookup failed, so it can be tried again later rather
    /// than being remembered as "the service has nothing".
    fn failed(&mut self, track: SpotifyUri) {
        let tries = match self.failed.iter_mut().find(|(known, _, _)| *known == track) {
            Some((_, at, tries)) => {
                *at = Instant::now();
                *tries += 1;
                *tries
            }
            None => {
                self.failed.push((track, Instant::now(), 1));
                1
            }
        };
        log::debug!("automix: cuepoint lookup for that track failed {tries} time(s); will retry");
        if self.failed.len() > REMEMBERED_TRACKS {
            self.failed.remove(0);
        }
    }

    /// Files what a lookup came back with.
    ///
    /// This is the decision that matters, so it lives in one place: an answer
    /// is remembered and a failure is not. Remembering a failure would make it
    /// final — the track would never be asked for again — which is the bug
    /// this separates the two for.
    fn record(&mut self, track: SpotifyUri, result: Result<Cuepoints, Missing>) {
        match result {
            Ok(cue) => {
                log::debug!(
                    "automix: cuepoints for {track}: in {:.2}s out {:.2}s {:.2} BPM",
                    cue.fade_in_at,
                    cue.fade_out_at,
                    cue.bpm
                );
                self.remember(track, Some(cue));
            }
            Err(Missing::NoCuepoints) => {
                log::debug!("automix: the service has no cuepoints for {track}");
                self.remember(track, None);
            }
            Err(Missing::Failed) => self.failed(track),
        }
    }

    /// Reads whatever has arrived, tells the engine what each role needs, and
    /// starts the lookups that are still missing.
    ///
    /// Called on every event. Nothing here waits: a request that has not come
    /// back is left for a later pass, and the player emits events several
    /// times a second, so an answer lands well before the boundary it is for.
    fn collect(&mut self, session: &Session, automix: &mut crate::automix_driver::Automix) {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut index = 0;
        while index < self.pending.len() {
            match self.pending[index].1.try_recv() {
                Ok(result) => {
                    let (track, _) = self.pending.remove(index);
                    self.record(track, result);
                }
                Err(TryRecvError::Empty) => index += 1,
                Err(TryRecvError::Closed) => {
                    // The task went away without answering, which is a
                    // failure like any other rather than an answer.
                    let (track, _) = self.pending.remove(index);
                    self.record(track, Err(Missing::Failed));
                }
            }
        }

        let now = Instant::now();
        for slot in [CuepointSlot::Playing, CuepointSlot::Incoming] {
            let wanted = match slot {
                CuepointSlot::Playing => &self.playing,
                CuepointSlot::Incoming => &self.incoming,
            };
            let Some(track) = wanted.track.clone() else {
                continue;
            };
            if let Some(answer) = self.answer_for(&track) {
                // The name goes first, always. The engine keys its cuepoints
                // by the track they belong to, so a cue that arrives before
                // the name it belongs to is dropped — which is exactly what
                // happened when the queue named the next track early: its
                // answer came back while the engine still had no idea which
                // track that was, and the cue was discarded as a mismatch.
                match slot {
                    CuepointSlot::Playing => automix.set_playing_cuepoints(&track, answer),
                    CuepointSlot::Incoming => {
                        automix.set_incoming_track(track.clone());
                        automix.set_incoming_cuepoints(&track, answer)
                    }
                }
                continue;
            }
            if !self.needs_lookup(&track, now) {
                continue;
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let session = session.clone();
            let asked = track.clone();
            // Spawned rather than awaited: this runs on the task that also
            // arms transitions, and a network round trip in the middle of it
            // would put the lookup's own latency between an event and the
            // plan that uses it.
            tokio::spawn(async move {
                let answer = crate::automix_cuepoints::Cuepoints::fetch(&session, &asked).await;
                let _ = sender.send(answer);
            });
            self.pending.push((track, receiver));
        }
    }
}

fn set<T: PartialEq>(target: &mut T, value: T) -> bool {
    if *target == value {
        false
    } else {
        *target = value;
        true
    }
}

/// Clears the failure and its message together.
///
/// The two are one piece of state: a caller acting on `failure` must not see
/// a kind left over from a failure that is already over, so every site that
/// clears the message clears the kind with it. Doing it here rather than at
/// each site is what keeps them from drifting apart.
fn clear_failure(error: &mut Option<String>, failure: &mut Option<PlaybackFailure>) -> bool {
    let changed = set(error, None);
    changed | set(failure, None)
}

fn apply_event(state: &mut LocalState, event: PlayerEvent) -> bool {
    match event {
        PlayerEvent::Stopped { .. } => {
            let mut changed = set(&mut state.playback, Playback::Stopped);
            changed |= set(&mut state.position_ms, 0);
            changed |= set(&mut state.position_at, None);
            changed
        }
        PlayerEvent::Loading { position_ms, .. } => {
            let mut changed = if state.playback == Playback::Stopped {
                set(&mut state.playback, Playback::Loading)
            } else {
                false
            };
            changed |= set(&mut state.position_ms, position_ms);
            changed |= set(&mut state.position_at, None);
            changed |= clear_failure(&mut state.error, &mut state.failure);
            changed
        }
        PlayerEvent::Playing { position_ms, .. } => {
            let mut changed = set(&mut state.playback, Playback::Playing);
            changed |= set(&mut state.position_ms, position_ms);
            state.position_at = Some(Instant::now());
            changed || true
        }
        PlayerEvent::Paused { position_ms, .. } => {
            let mut changed = set(&mut state.playback, Playback::Paused);
            changed |= set(&mut state.position_ms, position_ms);
            changed |= set(&mut state.position_at, None);
            changed
        }
        PlayerEvent::PositionCorrection { position_ms, .. }
        | PlayerEvent::PositionChanged { position_ms, .. } => {
            state.position_ms = position_ms;
            if state.playback == Playback::Playing {
                state.position_at = Some(Instant::now());
            }
            true
        }
        PlayerEvent::Seeked { position_ms, .. } => {
            state.position_ms = position_ms;
            if state.playback == Playback::Playing {
                state.position_at = Some(Instant::now());
            }
            state.seek_sequence = state.seek_sequence.wrapping_add(1);
            true
        }
        PlayerEvent::TrackChanged { audio_item } => {
            let mut changed = set(&mut state.track, Some(local_track(&audio_item)));
            changed |= clear_failure(&mut state.error, &mut state.failure);
            changed
        }
        PlayerEvent::Unavailable { track_id, .. } => {
            state.failure = Some(PlaybackFailure::Unavailable);
            set(
                &mut state.error,
                Some(format!(
                    "This item isn't available: {}",
                    track_id.to_uri().unwrap_or_default()
                )),
            )
        }
        PlayerEvent::AudioKeyUnavailable { .. } => {
            state.failure = Some(PlaybackFailure::AudioKeyRefused);
            set(
                &mut state.error,
                Some("Spotify refused the audio key. Try again later".into()),
            )
        }
        PlayerEvent::VolumeChanged { volume } => set(&mut state.volume, volume),
        PlayerEvent::SessionConnected { user_name, .. } => {
            let mut changed = set(&mut state.connected, true);
            changed |= set(&mut state.username, user_name);
            changed
        }
        // In librespot this event means the Connect device became inactive,
        // usually because another device took over. The engine session is
        // still alive, and `Load` activates it again before starting a track.
        PlayerEvent::SessionDisconnected { .. } => set(&mut state.active_client, String::new()),
        PlayerEvent::SessionClientChanged { client_name, .. } => {
            set(&mut state.active_client, client_name)
        }
        PlayerEvent::ShuffleChanged { shuffle } => set(&mut state.shuffle, shuffle),
        PlayerEvent::RepeatChanged { context, track } => {
            let mode = if track {
                RepeatMode::Track
            } else if context {
                RepeatMode::Context
            } else {
                RepeatMode::Off
            };
            set(&mut state.repeat, mode)
        }
        PlayerEvent::Preloading { .. }
        | PlayerEvent::IncomingPreloaded { .. }
        | PlayerEvent::TimeToPreloadNextTrack { .. }
        | PlayerEvent::EndOfTrack { .. }
        | PlayerEvent::PlayRequestIdChanged { .. }
        | PlayerEvent::UpcomingTrack { .. }
        | PlayerEvent::AutoPlayChanged { .. }
        | PlayerEvent::FilterExplicitContentChanged { .. } => false,
    }
}

fn local_track(item: &AudioItem) -> LocalTrack {
    let (artists, album, is_episode) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists
                .iter()
                .map(|artist| {
                    let uri = artist.id.to_uri().ok();
                    ArtistRef {
                        id: uri
                            .as_deref()
                            .and_then(crate::util::uri_id)
                            .map(str::to_string),
                        name: artist.name.clone(),
                        uri,
                    }
                })
                .collect(),
            album.clone(),
            false,
        ),
        UniqueFields::Episode { show_name, .. } => (
            vec![ArtistRef {
                name: show_name.clone(),
                ..ArtistRef::default()
            }],
            show_name.clone(),
            true,
        ),
        UniqueFields::Local { artists, album, .. } => (
            artists
                .iter()
                .map(|name| ArtistRef {
                    name: name.clone(),
                    ..ArtistRef::default()
                })
                .collect(),
            album.clone().unwrap_or_default(),
            false,
        ),
    };
    let mut covers: Vec<_> = item.covers.iter().collect();
    covers.sort_by_key(|cover| std::cmp::Reverse(cover.width));
    let art_url = covers.first().map(|cover| cover.url.clone());
    let art_small_url = covers
        .iter()
        .rev()
        .find(|cover| cover.width >= 64)
        .or(covers.last())
        .map(|cover| cover.url.clone());
    LocalTrack {
        uri: item.uri.clone(),
        title: item.name.clone(),
        artists,
        album,
        art_url,
        art_small_url,
        duration_ms: item.duration_ms,
        is_episode,
    }
}

/// The account's playlist tree, and what Spotify lets the account do to
/// the playlists in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rootlist {
    /// The rows in Spotify's order, folder markers included.
    pub entries: Vec<RootlistEntry>,
    /// Playlists the account may add songs to, by URI, as Spotify's own
    /// permission service decorates the rootlist. The Web API's
    /// `collaborative` flag stays false for a playlist shared by
    /// invitation, so this is the only word on those.
    pub editable: std::collections::BTreeSet<String>,
}

/// The playlists in one rootlist page the account may add songs to, read
/// from the `capabilities` Spotify puts beside each row.
pub fn editable_uris(
    contents: &librespot_protocol::playlist4_external::ListItems,
) -> impl Iterator<Item = String> + '_ {
    contents
        .items
        .iter()
        .zip(&contents.meta_items)
        .filter(|(_, meta)| meta.capabilities.can_edit_items())
        .filter_map(|(item, _)| item.uri.clone())
}

/// One row of the account's playlist tree.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RootlistEntry {
    /// A playlist, by its URI.
    Playlist(String),
    /// A folder opens; everything until its end sits inside it.
    FolderStart {
        id: String,
        name: String,
    },
    FolderEnd,
}

/// The rootlist's rows from its URIs: playlists pass through, and the
/// `start-group`/`end-group` markers Spotify brackets folders with become
/// folder rows, their names percent-decoded.
pub fn parse_rootlist(uris: &[String]) -> Vec<RootlistEntry> {
    let mut entries = Vec::new();
    let mut depth = 0usize;
    for uri in uris {
        if let Some(rest) = uri.strip_prefix("spotify:start-group:") {
            let (id, name) = match rest.split_once(':') {
                Some((id, name)) => (id.to_string(), decode_folder_name(name)),
                None => (rest.to_string(), String::new()),
            };
            entries.push(RootlistEntry::FolderStart { id, name });
            depth += 1;
        } else if uri.starts_with("spotify:end-group:") {
            if depth > 0 {
                entries.push(RootlistEntry::FolderEnd);
                depth -= 1;
            }
        } else if uri.starts_with("spotify:playlist:") {
            entries.push(RootlistEntry::Playlist(uri.clone()));
        }
    }
    // A folder Spotify never closed still closes here.
    entries.extend(std::iter::repeat_n(RootlistEntry::FolderEnd, depth));
    entries
}

/// Folder names arrive percent-encoded, with `+` for a space.
fn decode_folder_name(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // The digits are read from the bytes this loop is already
            // walking. Taking them by slicing the text instead put the end
            // of the slice two bytes past a `%`, which is inside a character
            // whenever the next one is not ASCII: a panic rather than the
            // parse error the arm below is written for, on exactly the names
            // that arm exists for.
            b'%' if i + 2 < bytes.len() => {
                let digit = |byte: u8| (byte as char).to_digit(16);
                match (digit(bytes[i + 1]), digit(bytes[i + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high << 4 | low) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    /// The bug this covers: a track's cuepoints were looked up once as the
    /// incoming track and again as the playing track, because the two roles
    /// kept their lookups apart and neither knew the other had asked. The
    /// service answers per track, so that was two requests for one answer on
    /// every track.
    #[test]
    fn a_track_is_asked_for_once_however_many_roles_need_it() {
        use super::{CuepointFetches, CuepointSlot};
        use librespot_core::SpotifyUri;

        let now = std::time::Instant::now();
        let track = SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC").expect("a uri");
        let mut fetches = CuepointFetches::default();

        // Nothing is held for it yet, so it must be looked up.
        assert!(fetches.needs_lookup(&track, now));
        // Once its answer is in, the other role's need is already satisfied.
        fetches.remember(track.clone(), None);
        assert!(
            !fetches.needs_lookup(&track, now),
            "an answer is held, so no second request is made"
        );
        assert_eq!(
            fetches.answer_for(&track),
            Some(None),
            "and 'the service has nothing' is itself an answer, not a miss"
        );

        // Both roles can name it without asking again.
        fetches.want(CuepointSlot::Incoming, &track);
        fetches.want(CuepointSlot::Playing, &track);
        assert!(!fetches.needs_lookup(&track, now));
    }

    /// The remembered answers are bounded: a long queue must not grow them
    /// without end, and only the most recent tracks are worth keeping.
    #[test]
    fn remembered_answers_are_bounded_and_recent() {
        use super::{CuepointFetches, REMEMBERED_TRACKS};
        let mut fetches = CuepointFetches::default();
        let tracks: Vec<_> = (0..REMEMBERED_TRACKS + 4)
            .map(|index| format!("spotify:track:{index:022}"))
            .collect();
        for track in &tracks {
            let uri = librespot_core::SpotifyUri::from_uri(track).expect("a uri");
            fetches.remember(uri, None);
        }
        let oldest = librespot_core::SpotifyUri::from_uri(&tracks[0]).expect("a uri");
        let newest =
            librespot_core::SpotifyUri::from_uri(tracks.last().unwrap()).expect("a uri");
        assert!(
            fetches.answer_for(&oldest).is_none(),
            "the oldest answers are dropped once the cache is full"
        );
        assert!(fetches.answer_for(&newest).is_some());
    }

    /// A role naming a different track must be able to ask for it, or a skip
    /// would leave the new track with the previous track's cues.
    #[test]
    fn naming_a_new_track_reopens_the_question() {
        use super::{CuepointFetches, CuepointSlot};
        let first = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
        let second = "spotify:track:0aaKu1ym6qIuoIOsTH8uij";
        let first = librespot_core::SpotifyUri::from_uri(first).expect("a uri");
        let second = librespot_core::SpotifyUri::from_uri(second).expect("a uri");

        let now = std::time::Instant::now();
        let mut fetches = CuepointFetches::default();
        fetches.want(CuepointSlot::Playing, &first);
        fetches.remember(first, None);
        assert!(!fetches.needs_lookup(&fetches.playing.track.clone().expect("named"), now));

        fetches.want(CuepointSlot::Playing, &second);
        assert!(
            fetches.needs_lookup(&second, now),
            "the new track has not been asked for, so it must be"
        );
    }

    /// The bug this covers: a lookup that failed was filed as "the service
    /// has nothing", so the track was never asked for again and its transition
    /// was taken away for the rest of the session. The two are different
    /// answers and only one of them is final.
    ///
    /// Driven through `record`, which is what the collection loop calls, so
    /// this pins the decision rather than the helper underneath it.
    #[test]
    fn a_failed_lookup_is_retried_rather_than_remembered_as_no_answer() {
        use super::{CuepointFetches, MAX_LOOKUP_TRIES, Missing};
        let track = librespot_core::SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC")
            .expect("a uri");
        let mut fetches = CuepointFetches::default();
        let at = std::time::Instant::now();

        fetches.record(track.clone(), Err(Missing::Failed));
        assert_eq!(
            fetches.answer_for(&track),
            None,
            "a failure says nothing about the track, so it is not an answer"
        );
        assert!(
            !fetches.needs_lookup(&track, at),
            "and it must not be asked for again on the same event"
        );
        assert!(
            fetches.needs_lookup(&track, at + std::time::Duration::from_secs(60)),
            "but the track is still worth asking for once the failure has aged"
        );

        // A track the service genuinely has nothing for is settled, and stays
        // settled: asking again would be a request spent on a known answer.
        let settled = librespot_core::SpotifyUri::from_uri("spotify:track:0aaKu1ym6qIuoIOsTH8uij")
            .expect("a uri");
        fetches.record(settled.clone(), Err(Missing::NoCuepoints));
        assert_eq!(
            fetches.answer_for(&settled),
            Some(None),
            "an answer of 'nothing' is an answer"
        );
        assert!(
            !fetches.needs_lookup(&settled, at + std::time::Duration::from_secs(600)),
            "and it is final however long it has been held"
        );

        // An answer that arrives is held, and told to no one more than once
        // per pass is the engine's business; this only checks it is kept.
        let answered =
            librespot_core::SpotifyUri::from_uri("spotify:track:2tak3H7HGKtRsAmEcLc1VO")
                .expect("a uri");
        fetches.record(
            answered.clone(),
            Ok(crate::automix_cuepoints::Cuepoints {
                fade_in_at: 12.0,
                fade_out_at: 180.0,
                bpm: 128.0,
            }),
        );
        assert_eq!(
            fetches.answer_for(&answered).flatten().map(|cue| cue.bpm),
            Some(128.0)
        );

        // A service that keeps refusing is not asked forever.
        for _ in 0..MAX_LOOKUP_TRIES {
            fetches.record(track.clone(), Err(Missing::Failed));
        }
        assert!(
            !fetches.needs_lookup(&track, at + std::time::Duration::from_secs(600)),
            "after the retries are spent it is treated as unresolvable"
        );
    }

    /// A crossfaded skip is not interrupted, and a skip with no overlap is.
    #[test]
    fn a_crossfaded_skip_is_not_interrupted() {
        use super::{PlayerCommand, skip_is_mixed};
        use std::time::Duration;

        assert!(skip_is_mixed(&PlayerCommand::Next, Duration::from_secs(5)));
        assert!(skip_is_mixed(
            &PlayerCommand::Previous,
            Duration::from_secs(5)
        ));
        // With no overlap configured the skip is a cut, so the interrupt is
        // what keeps the old track from playing on over the new one.
        assert!(!skip_is_mixed(&PlayerCommand::Next, Duration::ZERO));
    }

    /// A load names its own track and is often a fresh start rather than a
    /// mix, so it keeps its interrupt even when a crossfade is configured.
    #[test]
    fn a_load_is_not_treated_as_a_mix() {
        use super::{LoadSpec, PlayerCommand, skip_is_mixed};
        use std::time::Duration;

        assert!(!skip_is_mixed(
            &PlayerCommand::Load(LoadSpec::default()),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn playback_metadata_preserves_each_artist_id_and_name() {
        use librespot_metadata::artist::{ArtistWithRole, ArtistsWithRole};

        let credits = [
            (
                "spotify:artist:0000000000000000000001",
                "Tyler, the Creator",
            ),
            ("spotify:artist:0000000000000000000002", "Guest"),
        ];
        let item = AudioItem {
            track_id: uri(),
            uri: uri().to_uri().unwrap(),
            files: Default::default(),
            name: "Song".into(),
            covers: vec![],
            language: vec![],
            duration_ms: 200_000,
            is_explicit: false,
            availability: Ok(()),
            alternatives: None,
            unique_fields: UniqueFields::Track {
                artists: ArtistsWithRole(
                    credits
                        .iter()
                        .map(|(uri, name)| ArtistWithRole {
                            id: librespot_core::SpotifyUri::from_uri(uri).unwrap(),
                            name: (*name).into(),
                            role: Default::default(),
                        })
                        .collect(),
                ),
                album: "Album".into(),
                album_artists: vec![],
                popularity: 0,
                number: 1,
                disc_number: 1,
            },
        };

        let track = local_track(&item);
        assert_eq!(track.artist_names(), "Tyler, the Creator, Guest");
        assert_eq!(track.artists.len(), 2);
        for (artist, (uri, name)) in track.artists.iter().zip(credits) {
            assert_eq!(artist.id.as_deref(), crate::util::uri_id(uri));
            assert_eq!(artist.uri.as_deref(), Some(uri));
            assert_eq!(artist.name, name);
        }
    }

    #[test]
    fn the_rootlist_markers_become_folders() {
        let uris: Vec<String> = [
            "spotify:playlist:aaa",
            "spotify:start-group:f1:Late%20Night+Mix",
            "spotify:playlist:bbb",
            "spotify:playlist:ccc",
            "spotify:end-group:f1",
            "spotify:playlist:ddd",
            "spotify:start-group:f2:Open",
            "spotify:playlist:eee",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let rows = parse_rootlist(&uris);
        assert_eq!(
            rows[0],
            RootlistEntry::Playlist("spotify:playlist:aaa".into())
        );
        assert_eq!(
            rows[1],
            RootlistEntry::FolderStart {
                id: "f1".into(),
                name: "Late Night Mix".into()
            }
        );
        assert_eq!(rows[4], RootlistEntry::FolderEnd);
        // The unclosed folder still closes.
        assert_eq!(rows.last(), Some(&RootlistEntry::FolderEnd));
        assert_eq!(rows.len(), 9);
    }

    #[test]
    fn a_folder_name_with_a_bare_percent_keeps_its_percent() {
        // The decoder already has an answer for a `%` that begins no escape:
        // it keeps the `%` and moves on. That answer could not be reached
        // when the next character was multi-byte, because the two digits
        // were taken by slicing the `&str` and the second byte of a slice
        // that lands inside a character is a panic, not a parse error.
        let uris: Vec<String> = ["spotify:start-group:f1:100%25 \u{c548}\u{b155}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_rootlist(&uris)[0],
            RootlistEntry::FolderStart {
                id: "f1".into(),
                name: "100% \u{c548}\u{b155}".into()
            }
        );

        // The same shape with nothing to decode at all.
        let raw: Vec<String> = ["spotify:start-group:f2:100% \u{c548}\u{b155}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_rootlist(&raw)[0],
            RootlistEntry::FolderStart {
                id: "f2".into(),
                name: "100% \u{c548}\u{b155}".into()
            }
        );
    }

    /// A playlist shared by invitation is editable by Spotify's word in the
    /// rootlist, never by the Web API's collaborative flag.
    #[test]
    fn the_rootlist_says_which_playlists_take_songs() {
        use librespot_protocol::playlist_permission::Capabilities;
        use librespot_protocol::playlist4_external::{Item, ListItems, MetaItem};

        // #given
        let mut contents = ListItems::new();
        for (uri, can_edit) in [
            ("spotify:playlist:mine", Some(true)),
            ("spotify:playlist:theirs", Some(false)),
            ("spotify:playlist:shared", Some(true)),
            ("spotify:playlist:undecorated", None),
        ] {
            let mut item = Item::new();
            item.set_uri(uri.to_string());
            contents.items.push(item);
            let mut meta = MetaItem::new();
            if let Some(can_edit) = can_edit {
                let mut capabilities = Capabilities::new();
                capabilities.set_can_edit_items(can_edit);
                meta.capabilities = protobuf::MessageField::some(capabilities);
            }
            contents.meta_items.push(meta);
        }

        // #when
        let editable: Vec<String> = editable_uris(&contents).collect();

        // #then
        assert_eq!(
            editable,
            ["spotify:playlist:mine", "spotify:playlist:shared"]
        );
    }

    use super::*;
    use librespot_core::SpotifyUri;

    fn uri() -> SpotifyUri {
        SpotifyUri::from_uri("spotify:track:14XWXWv5FoCbFzLksawpEe").unwrap()
    }

    #[test]
    fn position_interpolates_only_while_playing() {
        let mut state = LocalState {
            playback: Playback::Paused,
            position_ms: 5_000,
            position_at: Some(Instant::now() - Duration::from_secs(2)),
            ..LocalState::default()
        };
        assert_eq!(state.position_now(), 5_000);
        state.playback = Playback::Playing;
        assert!(state.position_now() >= 7_000);
    }

    #[test]
    fn loading_keeps_a_playing_state_visible() {
        let mut state = LocalState {
            playback: Playback::Playing,
            ..LocalState::default()
        };
        apply_event(
            &mut state,
            PlayerEvent::Loading {
                play_request_id: 1,
                track_id: uri(),
                position_ms: 0,
            },
        );
        assert_eq!(state.playback, Playback::Playing);
    }

    #[test]
    fn replacing_a_playing_track_interrupts_queued_audio() {
        let playing = LocalState {
            playback: Playback::Playing,
            ..LocalState::default()
        };
        let stopped = LocalState::default();
        let load = PlayerCommand::Load(LoadSpec::default());

        assert!(command_interrupts_audio(&playing, &PlayerCommand::Next));
        assert!(command_interrupts_audio(&playing, &PlayerCommand::Previous));
        assert!(command_interrupts_audio(&playing, &load));
        assert!(!command_interrupts_audio(&stopped, &PlayerCommand::Next));
        assert!(!command_interrupts_audio(
            &playing,
            &PlayerCommand::Seek(10)
        ));
    }

    /// Spotify making this Connect device inactive must not be mistaken for
    /// the engine session ending. A later playlist load can activate the same
    /// Spirc instance; marking it disconnected makes the UI hold that load
    /// forever while waiting for a reconnect that will never happen.
    #[test]
    fn an_inactive_connect_device_keeps_its_engine_session() {
        let mut state = LocalState {
            connected: true,
            active_client: "Fastpotify".into(),
            ..LocalState::default()
        };

        assert!(apply_event(
            &mut state,
            PlayerEvent::SessionDisconnected {
                connection_id: "connection".into(),
                user_name: "listener".into(),
            },
        ));

        assert!(state.connected, "the Spotify session is still usable");
        assert!(state.active_client.is_empty());
    }

    #[test]
    fn a_rejected_audio_key_has_its_own_error() {
        let mut state = LocalState::default();

        assert!(apply_event(
            &mut state,
            PlayerEvent::AudioKeyUnavailable {
                play_request_id: 1,
                track_id: uri(),
            },
        ));
        assert_eq!(
            state.error.as_deref(),
            Some("Spotify refused the audio key. Try again later")
        );
    }

    #[test]
    fn repeat_cycles_and_maps() {
        assert_eq!(RepeatMode::Off.next(), RepeatMode::Context);
        assert_eq!(RepeatMode::Track.next(), RepeatMode::Off);
        assert_eq!(RepeatMode::from_api("track"), RepeatMode::Track);
        assert_eq!(RepeatMode::Context.api_name(), "context");
    }

    #[test]
    fn device_id_is_stable_hex() {
        let config = EngineConfig {
            buffer_ms: crate::sink::DEFAULT_BUFFER_MS,
            tap: AudioTap::new(),
            eq: crate::eq::shared(),
            analysis: None,
            automix_view: crate::automix_driver::shared_view(),
            device_name: "Fastpotify".into(),
            bitrate_kbps: 320,
            normalisation: false,
            autoplay: true,
            gapless: true,
            crossfade: Duration::ZERO,
            backend: None,
            audio_device: None,
            initial_volume: 1,
            volume_dir: PathBuf::new(),
            audio_cache_dir: None,
            audio_cache_limit: None,
        };
        let id = config.device_id();
        assert_eq!(id.len(), 40);
        assert_eq!(id, config.device_id());
    }

    /// A track that was playing or paused is remembered with its position;
    /// nothing is once playback has stopped.
    #[test]
    fn an_interrupted_track_is_remembered_with_its_position() {
        let mut state = LocalState {
            track: Some(LocalTrack {
                uri: "spotify:track:x".into(),
                duration_ms: 200_000,
                ..LocalTrack::default()
            }),
            playback: Playback::Playing,
            position_ms: 10_000,
            position_at: Some(Instant::now()),
            ..LocalState::default()
        };
        let resume = state.interrupted().expect("playing");
        assert_eq!(resume.uri, "spotify:track:x");
        assert!(resume.playing);
        assert!(resume.position_ms >= 10_000);
        state.playback = Playback::Paused;
        assert!(!state.interrupted().expect("paused").playing);
        state.playback = Playback::Stopped;
        assert!(state.interrupted().is_none());
        state.playback = Playback::Playing;
        state.track = None;
        assert!(state.interrupted().is_none());
    }
}
