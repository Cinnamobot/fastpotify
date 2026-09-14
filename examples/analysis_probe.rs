//! Diagnostic: can this account still reach Spotify's audio analysis?
//!
//! The analysis endpoint (beats, bars, sections, tatums) would give the
//! automix planner the structure of a whole track instead of the 20-second
//! window it measures today. It was withdrawn for new apps in Nov 2024, so
//! whether it still answers depends on the app this account is registered
//! against. This asks and prints what comes back; it changes nothing.
//!
//!   cargo run --example analysis_probe -- spotify:track:4uLU6hMCjMI75M1A2tKUQC

use librespot_core::{Session, SessionConfig, cache::Cache};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let track = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into());
    let id = track.rsplit(':').next().unwrap_or_default().to_string();

    let dirs = fastpotify::paths::AppDirs::discover();
    let cache = Cache::new::<&std::path::Path>(None, None, None, None)?.with_memory_credentials();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let store = fastpotify::credentials::Store::new(dirs);
        let loaded = store
            .lease(fastpotify::credentials::Slot::Playback)
            .load()
            .await?;
        if let Some(warning) = loaded.warning {
            eprintln!("{warning}");
        }
        let Some(fastpotify::credentials::Grant::Playback(credentials)) = loaded.grant else {
            anyhow::bail!("Enable playback in Fastpotify first");
        };
        let session = Session::new(SessionConfig::default(), Some(cache));
        session.connect(credentials, false).await?;
        println!("connected as {}", session.username());

        // The internal endpoints the desktop client itself uses. These go
        // over the session's own Mercury connection, not the public Web API,
        // so the November 2024 change to the public API need not apply.
        let uris = [
            format!("hm://audio-analysis/v1/analysis/{id}"),
            format!("hm://audio-analysis/v1/analysis/{id}/sections"),
            format!("hm://audio-features/v1/features/{id}"),
            format!("hm://metadata/4/track/{id}?country=JP&product=0"),
        ];
        for uri in uris {
            print!("{uri} -> ");
            match session.mercury().get(uri.clone()) {
                Ok(future) => match future.await {
                    Ok(response) => {
                        let payload: usize = response.payload.iter().map(Vec::len).sum();
                        println!(
                            "ok: status={} parts={} bytes={payload}",
                            response.status_code,
                            response.payload.len()
                        );
                        // Show enough to tell structure from an error body.
                        if let Some(first) = response.payload.first() {
                            let text = String::from_utf8_lossy(&first[..first.len().min(600)]);
                            println!("    {text}");
                        }
                    }
                    Err(error) => println!("ERR: {error}"),
                },
                Err(error) => println!("ERR: {error}"),
            }
        }

        // `LIST_TUNER_AUDIO_ANALYSIS` is the one audio-analysis extension the
        // protocol still names. It is what a tuner would use to line tracks
        // up, which is exactly what this needs.
        use librespot_core::spotify_uri::SpotifyUri;
        use librespot_protocol::extension_kind::ExtensionKind;
        let uri = SpotifyUri::from_uri(&track)?;
        print!("get_metadata(LIST_TUNER_AUDIO_ANALYSIS, {track}) -> ");
        match session
            .spclient()
            .get_metadata(ExtensionKind::LIST_TUNER_AUDIO_ANALYSIS, &uri)
            .await
        {
            Ok(data) => {
                println!("ok: {} bytes", data.len());
                println!(
                    "    {}",
                    String::from_utf8_lossy(&data[..data.len().min(600)])
                );
            }
            Err(error) => println!("ERR: {error}"),
        }
        anyhow::Ok(())
    })?;
    Ok(())
}
