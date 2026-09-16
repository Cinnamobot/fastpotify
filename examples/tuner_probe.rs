//! Prints the ground truth the official client mixes from, per track.
//!
//! The desktop client does not derive a transition locally: it asks for
//! Spotify's own tuner data, which carries the beats and the fade cuepoints
//! computed on the service. Where that data exists it is the reference any
//! local detector should be measured against, and where it does not, a local
//! detector is the only option — which is the case this project is built for.
//!
//! The payload is dumped raw because its schema is not published. What is
//! known is that the response is a length-delimited blob that arrives
//! non-empty for ordinary tracks, so the interesting question is whether the
//! fade cuepoints are in it and where.
//!
//!   cargo run --example tuner_probe -- spotify:track:...
//!   cargo run --example tuner_probe -- --batch < tracks.txt

use fastpotify::automix_cuepoints::Cuepoints;
use futures_util::FutureExt;
use librespot_core::{Session, SessionConfig, SpotifyUri, cache::Cache};
use librespot_protocol::extension_kind::ExtensionKind;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let batch = args.iter().any(|arg| arg == "--batch");
    let tracks: Vec<String> = if batch {
        std::io::read_to_string(std::io::stdin())?
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    } else {
        vec![
            args.get(1)
                .cloned()
                .unwrap_or_else(|| "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into()),
        ]
    };

    let dirs = fastpotify::paths::AppDirs::discover();
    let cache = Cache::new(None, None, Some(dirs.audio_cache_dir().as_path()), None)?
        .with_memory_credentials();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let store = fastpotify::credentials::Store::new(dirs);
        let loaded = store
            .lease(fastpotify::credentials::Slot::Playback)
            .load()
            .await?;
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
            match session
                .spclient()
                .get_metadata(ExtensionKind::LIST_TUNER_AUDIO_ANALYSIS, &uri)
                .await
            {
                Ok(data) => {
                    println!("{track}  analysis bytes={}", data.len());
                    if batch {
                        let name = track.replace(':', "_");
                        std::fs::write(format!("tuner-{name}.bin"), &data)?;
                    } else {
                        dump(&data);
                    }
                }
                Err(error) => println!("{track}  analysis ERR {error}"),
            }
            // The server's own automix cuepoints: where the official client
            // brings a track in and takes it out, with the tempo it uses.
            for (label, kind) in [
                ("LIST_TUNER_CUEPOINTS", ExtensionKind::LIST_TUNER_CUEPOINTS),
                ("CUEPOINTS", ExtensionKind::CUEPOINTS),
                ("AUTOMIX_MODE", ExtensionKind::AUTOMIX_MODE),
            ] {
                let request = session.spclient().get_metadata(kind, &uri);
                match std::panic::AssertUnwindSafe(request).catch_unwind().await {
                    Ok(Ok(data)) => {
                        println!("{track}  {label} bytes={}", data.len());
                        if label == "CUEPOINTS" {
                            let name = track.replace(':', "_");
                            std::fs::write(format!("cue-{name}.bin"), &data)?;
                            match Cuepoints::parse(&data) {
                                Some(c) => println!(
                                    "    parsed: fade_in {:.2}s  fade_out {:.2}s  {:.3} BPM",
                                    c.fade_in_at, c.fade_out_at, c.bpm
                                ),
                                None => println!("    parsed: none"),
                            }
                            if !batch {
                                dump(&data);
                            }
                        }
                    }
                    Ok(Err(error)) => println!("{track}  {label} ERR {error}"),
                    Err(_) => println!("{track}  {label} PANIC"),
                }
            }
        }
        anyhow::Ok(())
    })?;
    Ok(())
}

/// Prints the payload as field numbers, so a schema can be inferred from a
/// response whose layout is unpublished.
fn dump(data: &[u8]) {
    fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        while *i < b.len() {
            let byte = b[*i];
            *i += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
        None
    }

    let mut index = 0usize;
    let mut shown = 0usize;
    while index < data.len() && shown < 40 {
        let start = index;
        let Some(key) = varint(data, &mut index) else {
            break;
        };
        let field = key >> 3;
        let wire = key & 7;
        let (desc, end) = match wire {
            0 => match varint(data, &mut index) {
                Some(value) => (format!("varint {value}"), index),
                None => break,
            },
            1 => (
                format!("fixed64 {:?}", &data[index..(index + 8).min(data.len())]),
                index + 8,
            ),
            5 => (
                format!("fixed32 {:?}", &data[index..(index + 4).min(data.len())]),
                index + 4,
            ),
            2 => match varint(data, &mut index) {
                Some(len) => {
                    let end = (index + len as usize).min(data.len());
                    let body = &data[index..end];
                    let head = body
                        .iter()
                        .take(24)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    (format!("len={len} [{head}]"), end)
                }
                None => break,
            },
            other => (format!("wire {other}"), index),
        };
        println!("  @{start:6} field {field:3} {desc}");
        index = end;
        shown += 1;
    }
    println!("  ({} bytes total)", data.len());
}
