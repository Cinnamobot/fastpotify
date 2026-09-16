//! Runs automix's structure detection over real tracks, offline.
//!
//! The detector's thresholds were tuned against synthetic verse/chorus
//! material whose band ratio nearly triples across a boundary. A real master
//! opens far less than that, so whether the constants hold on real music is
//! the one thing the unit tests cannot answer and the one thing the tuning
//! depends on. This decodes a real track from the audio cache, runs the same
//! envelope, band split and section detection the client runs, and prints
//! what each stage saw: the band ratio curve, the loudness curve, the
//! sections each signal finds, and the thresholds they were found at.
//!
//! It changes nothing and writes nothing. Use it to compare a threshold
//! against a track whose structure you know, before changing a constant.
//!
//!   cargo run --example automix_analyse -- <track-uri> [seconds]
//!
//! Seconds defaults to the whole track. Reading a whole track costs one
//! audio key request and one decode, both off the playback path.

use librespot_audio::AudioFile;
use librespot_core::{FileId, Session, SessionConfig, SpotifyId, SpotifyUri, cache::Cache};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};

use fastpotify::automix::Analysis;
use fastpotify::automix_track::{NUM_BANDS, envelope_with_bands};

/// Bytes per second the fetcher sizes its read-ahead by, per format. The
/// decoder only needs a value that is roughly right, and the cache is warm
/// for every track this is pointed at.
fn data_rate(format: AudioFileFormat) -> usize {
    match format {
        AudioFileFormat::OGG_VORBIS_320 | AudioFileFormat::MP3_320 => 40,
        AudioFileFormat::OGG_VORBIS_160 | AudioFileFormat::MP3_160 => 20,
        AudioFileFormat::OGG_VORBIS_96 | AudioFileFormat::MP3_96 => 12,
        _ => 40,
    }
}

/// The first format offered that librespot can decode.
fn pick(item: &AudioItem) -> Option<(AudioFileFormat, FileId)> {
    const ORDER: [AudioFileFormat; 7] = [
        AudioFileFormat::OGG_VORBIS_320,
        AudioFileFormat::OGG_VORBIS_160,
        AudioFileFormat::OGG_VORBIS_96,
        AudioFileFormat::MP3_320,
        AudioFileFormat::MP3_160,
        AudioFileFormat::MP3_96,
        AudioFileFormat::MP3_256,
    ];
    ORDER
        .iter()
        .find_map(|format| item.files.get(format).map(|file| (*format, *file)))
}

/// The 16 bytes of a base62 Spotify id, which is what the file id hashes.
fn raw_id(id: &SpotifyId) -> Option<[u8; 16]> {
    let mut bytes = [0u8; 16];
    let text = id.to_base16().ok()?;
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// Decodes up to `seconds` of a track into interleaved stereo f32.
async fn decode(session: &Session, uri: &SpotifyUri, seconds: f64) -> anyhow::Result<Vec<f32>> {
    let item = AudioItem::get_file(session, uri.clone())
        .await
        .map_err(|error| anyhow::anyhow!("no metadata for {uri:?}: {error}"))?;
    let (format, file) = pick(&item).ok_or_else(|| anyhow::anyhow!("no decodable format"))?;
    let track_id = raw_id(match uri {
        SpotifyUri::Track { id } => id,
        _ => anyhow::bail!("only tracks can be decoded"),
    })
    .ok_or_else(|| anyhow::anyhow!("bad track id"))?;

    let stream = AudioFile::open(session, file, data_rate(format))
        .await
        .map_err(|error| anyhow::anyhow!("cannot open the audio file: {error}"))?;
    let length = session
        .cache()
        .and_then(|cache| cache.file_path(file))
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);

    let key = session
        .audio_key()
        .request(SpotifyId::from_raw(&track_id)?, file)
        .await
        .map_err(|error| anyhow::anyhow!("no audio key: {error}"))?;

    let decrypted = librespot_audio::AudioDecrypt::new(Some(key), stream);
    // Ogg Vorbis carries a custom header packet before the real stream, and
    // it has to be skipped exactly as the player skips it.
    let is_ogg = AudioFiles::is_ogg_vorbis(format);
    let header_end = if is_ogg { 0xa7u64 } else { 0 };
    let subfile = Subfile::new(std::io::BufReader::new(decrypted), header_end, length);

    let mut hint = symphonia::core::probe::Hint::new();
    if let Some(mime) = AudioFiles::mime_type(format) {
        hint.mime_type(mime);
    }
    let mut decoder = librespot_playback::decoder::SymphoniaDecoder::new(subfile, hint)
        .map_err(|error| anyhow::anyhow!("cannot decode: {error}"))?;

    let wanted = (seconds * f64::from(SAMPLE_RATE)) as usize * NUM_CHANNELS as usize;
    let mut samples: Vec<f32> = Vec::with_capacity(wanted.min(20_000_000));
    use librespot_playback::decoder::AudioDecoder;
    while samples.len() < wanted {
        match decoder.next_packet() {
            Ok(Some((_, packet))) => match packet.samples() {
                Ok(packet) => samples.extend(packet.iter().map(|sample| *sample as f32)),
                Err(_) => continue,
            },
            Ok(None) => break,
            Err(error) => {
                eprintln!("decode stopped early: {error}");
                break;
            }
        }
    }
    Ok(samples)
}

/// A window of `Subfile` the decoder can read, mirroring the player's own.
struct Subfile {
    stream: std::io::BufReader<librespot_audio::AudioDecrypt<AudioFile>>,
    offset: u64,
    length: u64,
}

impl Subfile {
    fn new(
        stream: std::io::BufReader<librespot_audio::AudioDecrypt<AudioFile>>,
        offset: u64,
        length: u64,
    ) -> Self {
        Self {
            stream,
            offset,
            length,
        }
    }
}

impl std::io::Read for Subfile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl std::io::Seek for Subfile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let pos = match pos {
            std::io::SeekFrom::Start(offset) => std::io::SeekFrom::Start(offset + self.offset),
            other => other,
        };
        let new = self.stream.seek(pos)?;
        Ok(new.saturating_sub(self.offset))
    }
}

impl symphonia::core::io::MediaSource for Subfile {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

fn spark(values: &[f64], blocks: usize) -> String {
    const LEVELS: [char; 9] = ['_', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return String::new();
    }
    let (mut low, mut high) = (f64::INFINITY, f64::NEG_INFINITY);
    for value in values {
        low = low.min(*value);
        high = high.max(*value);
    }
    let span = (high - low).max(f64::MIN_POSITIVE);
    let per = values.len().div_ceil(blocks).max(1);
    values
        .chunks(per)
        .map(|chunk| {
            let mean = chunk.iter().sum::<f64>() / chunk.len() as f64;
            let level = ((mean - low) / span * 8.0) as usize;
            LEVELS[level.min(8)]
        })
        .collect()
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error")).init();
    let args: Vec<String> = std::env::args().collect();
    let batch = args.iter().any(|arg| arg == "--batch");
    let dump = args.iter().any(|arg| arg == "--dump");
    let tracks: Vec<String> = if batch {
        std::io::read_to_string(std::io::stdin())?
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    } else {
        vec![args
            .get(1)
            .cloned()
            .unwrap_or_else(|| "spotify:track:0aaKu1ym6qIuoIOsTH8uij".into())]
    };
    let seconds: f64 = args
        .iter()
        .skip(1)
        .find_map(|value| value.parse().ok())
        .unwrap_or(600.0);

    let dirs = fastpotify::paths::AppDirs::discover();
    let cache = Cache::new(
        None,
        None,
        Some(dirs.audio_cache_dir().as_path()),
        None,
    )?
    .with_memory_credentials();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let store = fastpotify::credentials::Store::new(dirs);
    let mut sampled: Vec<(String, Vec<f32>)> = Vec::new();
    runtime.block_on(async {
        let loaded = store.lease(fastpotify::credentials::Slot::Playback).load().await?;
        let Some(fastpotify::credentials::Grant::Playback(credentials)) = loaded.grant else {
            anyhow::bail!("enable playback in Fastpotify first");
        };
        let session = Session::new(SessionConfig::default(), Some(cache));
        session.connect(credentials, false).await?;
        println!("connected as {}", session.username());
        for track in &tracks {
            let uri = match SpotifyUri::from_uri(track) {
                Ok(uri) => uri,
                Err(error) => {
                    println!("{track}: bad uri: {error}");
                    continue;
                }
            };
            match decode(&session, &uri, seconds).await {
                Ok(samples) if !samples.is_empty() => sampled.push((track.clone(), samples)),
                Ok(_) => println!("{track}: nothing decoded"),
                Err(error) => println!("{track}: {error}"),
            }
        }
        anyhow::Ok(())
    })?;

    if batch {
        let mut reported = 0usize;
        let mut missed = 0usize;
        let mut loudness_missed = 0usize;
        let mut ratios: Vec<f64> = Vec::new();
        for (track, samples) in &sampled {
            let (envelope, bands) = envelope_with_bands(samples);
            let Some(mut analysis) =
                Analysis::of_with_envelope(samples, SAMPLE_RATE, &envelope)
            else {
                println!("{track}: no grid");
                continue;
            };
            analysis.refresh_bands(&bands);
            let ratio: Vec<f64> = (0..bands[1].len().min(bands[2].len()))
                .map(|index| bands[2][index] / bands[1][index].max(f64::MIN_POSITIVE))
                .collect();
            let mut sorted = ratio.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median = sorted[sorted.len() / 2];
            let peak = sorted[sorted.len() - 1];
            let spread = peak / median.max(f64::MIN_POSITIVE);
            let bars = analysis.bar_loudness().to_vec();
            let span = analysis.loudest_span(&bars, 16);
            let sections = analysis.loud_sections();
            ratios.push(spread);
            let found = !sections.is_empty();
            let loud_found = span.is_some();
            // The doc's own metric: the longest stretch that stays above the
            // threshold, which is what decides whether a section exists.
            // `find_loud_sections` keeps runs of MIN_LOUD_SECONDS or more, so
            // a run shorter than that is where the detector lost it.
            let run_seconds = |values: &[f64], median: f64, factor: f64, hop: f64| {
                let cut = median * factor;
                let (mut best, mut run) = (0usize, 0usize);
                for value in values {
                    run = if *value >= cut { run + 1 } else { 0 };
                    best = best.max(run);
                }
                best as f64 * hop
            };
            let smoothed = {
                let window = (2.0 / 0.05f64).round() as usize;
                let half = window / 2;
                (0..ratio.len())
                    .map(|index| {
                        let start = index.saturating_sub(half);
                        let end = (index + half + 1).min(ratio.len());
                        ratio[start..end].iter().sum::<f64>() / (end - start) as f64
                    })
                    .collect::<Vec<f64>>()
            };
            let mut smoothed_sorted = smoothed.clone();
            smoothed_sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let smoothed_median = smoothed_sorted[smoothed_sorted.len() / 2];
            let band_run = run_seconds(&smoothed, smoothed_median, 1.35, 0.05);
            let mut bsorted = bars.clone();
            bsorted.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
            let loud_median = bsorted[bsorted.len() / 2];
            let loud_run = run_seconds(&bars, loud_median, 1.12, analysis.bar_seconds());
            if !found {
                missed += 1;
            }
            if !loud_found {
                loudness_missed += 1;
            }
            reported += 1;
            // A dump of both signals, so either can be lined up against a
            // reference measured outside this program — the server's own
            // tuner curve, or a by-ear reading of where the chorus is.
            if dump {
                let name = track.replace(':', "_");
                let mut out = String::new();
                out.push_str(&format!("# bpm {:.2} bar {:.3} hop 0.05\n", analysis.bpm, analysis.bar_seconds()));
                for (index, value) in smoothed.iter().enumerate() {
                    out.push_str(&format!("band {:.6}\n", value));
                    let _ = index;
                }
                for (index, value) in bars.iter().enumerate() {
                    out.push_str(&format!("loud {:.6}\n", value));
                    let _ = index;
                }
                std::fs::write(format!("curves-{name}.txt"), out)?;
            }
            println!(
                "{track}  {:.0}s  {:.0} BPM  longest run over threshold: band-ratio {band_run:.1}s (needs 6.0)  loudness {loud_run:.1}s  |  sections {}  loud16 {:?}",
                samples.len() as f64 / f64::from(SAMPLE_RATE) / f64::from(NUM_CHANNELS),
                analysis.bpm,
                if found {
                    sections
                        .iter()
                        .map(|s| format!("{:.0}-{:.0}", s.start, s.end))
                        .collect::<Vec<_>>()
                        .join(",")
                } else {
                    "NONE".into()
                },
                span.map(|s| format!("{s:.0}")),
            );
        }
        ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        println!(
            "\n{reported} tracks: band-ratio detector found nothing on {missed}; \
             loudest_span found nothing on {loudness_missed}"
        );
        if !ratios.is_empty() {
            println!(
                "spread p10 {:.2}x  p50 {:.2}x  p90 {:.2}x  max {:.2}x  (LOUD_PROMINENCE is 1.35x)",
                ratios[ratios.len() / 10],
                ratios[ratios.len() / 2],
                ratios[ratios.len() * 9 / 10],
                ratios[ratios.len() - 1]
            );
        }
        return Ok(());
    }

    let (track, samples) = sampled.pop().ok_or_else(|| anyhow::anyhow!("nothing decoded"))?;
    let frames = samples.len() / NUM_CHANNELS as usize;
    println!(
        "decoded {frames} frames ({:.1}s) from {track}",
        frames as f64 / f64::from(SAMPLE_RATE)
    );

    let (envelope, bands) = envelope_with_bands(&samples);
    let mut analysis = Analysis::of_with_envelope(&samples, SAMPLE_RATE, &envelope)
        .ok_or_else(|| anyhow::anyhow!("no beat grid: the tracker was not confident"))?;
    analysis.refresh_bands(&bands);

    println!(
        "\nbeat grid: {:.1} BPM, first beat {:.2}s, bar {:.2}s, confidence ok",
        analysis.bpm,
        analysis.first_beat,
        analysis.bar_seconds()
    );
    println!(
        "analysed {:.1}s of the track in bars",
        analysis.analysed_until().unwrap_or(0.0)
    );

    let ratio: Vec<f64> = (0..bands[1].len().min(bands[2].len()))
        .map(|index| bands[2][index] / bands[1][index].max(f64::MIN_POSITIVE))
        .collect();
    println!("\nband ratio (high/mid) over {:.0}s", ratio.len() as f64 * 0.05);
    println!("  {}", spark(&ratio, 90));
    let mut sorted = ratio.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    println!(
        "  p50 {median:.3}  p90 {:.3}  max {:.3}  -> 1.35x threshold {:.3}",
        sorted[sorted.len() * 9 / 10],
        sorted[sorted.len() - 1],
        median * 1.35
    );

    let bars = analysis.bar_loudness().to_vec();
    if !bars.is_empty() {
        let bar = analysis.bar_seconds();
        println!("\nloudness per bar, {:.2}s each, {} bars:", bar, bars.len());
        println!("  {}", spark(&bars, 90));
        let mut bsorted = bars.clone();
        bsorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let bmedian = bsorted[bsorted.len() / 2];
        println!(
            "  median {bmedian:.5}  min {:.5}  max {:.5}  max/median {:.2}x",
            bsorted[0],
            bsorted[bsorted.len() - 1],
            bsorted[bsorted.len() - 1] / bmedian.max(f64::MIN_POSITIVE)
        );
        println!("\n  loudest 16-bar span: {:?}", analysis.loudest_span(&bars, 16));
    }

    let sections = analysis.loud_sections();
    println!("\nband-ratio sections (>=6s):");
    if sections.is_empty() {
        println!("  none - the pair would fall back to the first downbeat");
    }
    for section in &sections {
        println!("  {:.1}s - {:.1}s ({:.1}s)", section.start, section.end, section.end - section.start);
    }

    for now in [30.0, 60.0, 90.0, 120.0] {
        println!(
            "  first chorus after {now:.0}s: {:?}",
            analysis.chorus_starting_after(now).map(|s| (s.start, s.end))
        );
    }
    println!("\nbands: {} readings each, {} of {NUM_BANDS}", bands[0].len(), NUM_BANDS);
    Ok(())
}
