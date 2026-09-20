mod player;
mod store;

use std::{io::Write, time::Duration};

use anyhow::{bail, Context, Result};
use clap::{Arg, ArgAction, ArgMatches, Command};
use librespot_audio::AudioFile;
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem, UniqueFields};
use rspotify::model::Id;

use crate::{auth, client::AppClient, config, state};
use store::{Download, Library};

pub fn command() -> Command {
    Command::new("downloads")
        .about("Download Spotify tracks and play them without a network connection")
        .subcommand_required(true)
        .subcommand(
            Command::new("add")
                .about("Download a track, album or playlist (requires an online Premium session)")
                .arg(Arg::new("source").required(true).num_args(1..))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Replace existing downloads"),
                ),
        )
        .subcommand(Command::new("list").about("List downloaded tracks as JSON"))
        .subcommand(
            Command::new("play")
                .about("Open the offline player; no Spotify connection is made")
                .arg(
                    Arg::new("source")
                        .help("Previously downloaded track, album or playlist URL/URI"),
                ),
        )
        .subcommand(
            Command::new("remove")
                .about("Remove one downloaded track")
                .arg(Arg::new("source").required(true)),
        )
        .subcommand(
            Command::new("verify").about("Check download integrity without connecting to Spotify"),
        )
}

pub fn library() -> Result<Library> {
    let root = if let Some(path) = &config::get_config().app_config.download_folder {
        path.clone()
    } else {
        dirs_next::data_dir()
            .context("cannot determine data directory")?
            .join("spotify-player")
            .join("downloads")
    };
    Ok(Library::new(root))
}

pub fn has_downloads() -> bool {
    library()
        .and_then(|l| l.tracks())
        .is_ok_and(|tracks| !tracks.is_empty())
}

pub fn play(source: Option<&str>, notice: Option<&str>) -> Result<()> {
    let library = library()?;
    let tracks = library.select(source.map(parse_source).transpose()?.as_deref())?;
    player::run(&library, &tracks, notice)
}

pub fn handle(args: &ArgMatches) -> Result<()> {
    let library = library()?;
    match args.subcommand().context("downloads subcommand required")? {
        ("add", args) => {
            let sources = args
                .get_many::<String>("source")
                .context("source required")?
                .map(|s| parse_source(s))
                .collect::<Result<Vec<_>>>()?;
            tokio::runtime::Runtime::new()?.block_on(add(&library, sources, args.get_flag("force")))
        }
        ("list", _) => {
            // Keys deliberately never appear in command output or logs.
            let tracks = library.tracks()?.into_iter().map(|t| serde_json::json!({
                "uri": t.uri, "name": t.name, "artists": t.artists,
                "album": t.album, "duration_ms": t.duration_ms, "bytes": t.bytes,
                "cover_downloaded": t.cover_url.as_ref().is_some_and(|url| library.cover_path(url).is_file()),
            })).collect::<Vec<_>>();
            serde_json::to_writer_pretty(std::io::stdout().lock(), &tracks)?;
            Ok(())
        }
        ("play", args) => play(args.get_one::<String>("source").map(String::as_str), None),
        ("remove", args) => library.remove(&parse_source(
            args.get_one::<String>("source")
                .context("source required")?,
        )?),
        ("verify", _) => {
            let mut failures = 0;
            for track in library.tracks()? {
                match library.verify(&track) {
                    Ok(packets) => writeln!(
                        std::io::stdout().lock(),
                        "OK {} — {} ({packets} decoded audio packets; artwork verified)",
                        track.uri,
                        track.name
                    )?,
                    Err(err) => {
                        failures += 1;
                        writeln!(std::io::stderr().lock(), "FAILED {}: {err:#}", track.uri)?;
                    }
                }
            }
            anyhow::ensure!(
                failures == 0,
                "{failures} downloads failed verification; download them again with --force"
            );
            Ok(())
        }
        _ => bail!("unknown downloads command"),
    }
}

fn parse_source(source: &str) -> Result<String> {
    let source = source.trim();
    let uri = if let Some(path) = source.strip_prefix("https://open.spotify.com/") {
        let path = path.split(['?', '#']).next().unwrap_or(path);
        let parts = path.trim_end_matches('/').split('/').collect::<Vec<_>>();
        let pair = if parts.first().is_some_and(|p| p.starts_with("intl-")) {
            &parts[1..]
        } else {
            &parts[..]
        };
        anyhow::ensure!(
            pair.len() == 2,
            "expected a Spotify track, album or playlist URL"
        );
        format!("spotify:{}:{}", pair[0], pair[1])
    } else if source.len() == 22 && source.bytes().all(|b| b.is_ascii_alphanumeric()) {
        format!("spotify:track:{source}")
    } else {
        source.to_owned()
    };
    let parts = uri.split(':').collect::<Vec<_>>();
    anyhow::ensure!(
        parts.len() == 3
            && parts[0] == "spotify"
            && matches!(parts[1], "track" | "album" | "playlist")
            && parts[2].len() == 22
            && parts[2].bytes().all(|b| b.is_ascii_alphanumeric()),
        "expected a Spotify track ID or track/album/playlist URI or URL"
    );
    Ok(uri)
}

async fn add(library: &Library, sources: Vec<String>, force: bool) -> Result<()> {
    library.create()?;
    // Track downloads use librespot directly, avoiding Web API quota entirely.
    let auth = auth::AuthConfig::new(config::get_config())?;
    let session = auth.session();
    let creds = auth::get_creds(&auth, true, true)?;
    tokio::time::timeout(Duration::from_secs(30), session.connect(creds, true))
        .await
        .context("Spotify connection timed out")??;
    let mut api = None;
    let mut failures = 0;
    for source in sources {
        let uris = if source.starts_with("spotify:track:") {
            vec![source.clone()]
        } else {
            if api.is_none() {
                api = Some(AppClient::new().await?);
            }
            let client = api.as_ref().context("API client unavailable")?;
            let context = if source.starts_with("spotify:album:") {
                client
                    .album_context(rspotify::model::AlbumId::from_uri(&source)?)
                    .await?
            } else {
                client
                    .playlist_context(rspotify::model::PlaylistId::from_uri(&source)?)
                    .await?
            };
            let (state::Context::Album { tracks, .. } | state::Context::Playlist { tracks, .. }) =
                context
            else {
                unreachable!()
            };
            tracks.into_iter().map(|t| t.id.uri()).collect()
        };
        let total = uris.len();
        for (index, uri) in uris.iter().enumerate() {
            if !force && library.contains_valid(uri) {
                let track = library.track(uri)?;
                if let Err(err) = download_cover(library, track.cover_url.as_deref()).await {
                    failures += 1;
                    writeln!(
                        std::io::stderr().lock(),
                        "Artwork missing for {uri}: {err:#}"
                    )?;
                }
                writeln!(
                    std::io::stderr().lock(),
                    "[{}/{total}] Already downloaded: {uri}",
                    index + 1
                )?;
                continue;
            }
            writeln!(
                std::io::stderr().lock(),
                "[{}/{total}] Downloading {uri}",
                index + 1
            )?;
            if let Err(err) = download(&session, library, uri).await {
                failures += 1;
                writeln!(std::io::stderr().lock(), "Failed {uri}: {err:#}")?;
            }
            // Keep bulk downloads sequential and avoid bursts of audio-key requests.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !source.starts_with("spotify:track:") {
            library.save_collection(&source, &uris)?;
        }
    }
    session.shutdown();
    anyhow::ensure!(failures == 0, "{failures} downloads failed; completed tracks were retained. Rerun to retry missing tracks.");
    Ok(())
}

async fn download(session: &Session, library: &Library, uri: &str) -> Result<()> {
    let original_uri = SpotifyUri::from_uri(uri)?;
    let mut item = AudioItem::get_file(session, original_uri).await?;
    if item.availability.is_err() {
        let alternatives = item
            .alternatives
            .clone()
            .context("track unavailable in your market")?;
        for alternative in alternatives.0 {
            let candidate = AudioItem::get_file(session, alternative).await?;
            if candidate.availability.is_ok() {
                item = candidate;
                break;
            }
        }
    }
    anyhow::ensure!(
        item.availability.is_ok(),
        "track unavailable in your market"
    );
    let bitrate = config::get_config().app_config.device.bitrate;
    let formats = match bitrate {
        96 => [
            (AudioFileFormat::OGG_VORBIS_96, 96),
            (AudioFileFormat::OGG_VORBIS_160, 160),
            (AudioFileFormat::OGG_VORBIS_320, 320),
        ],
        160 => [
            (AudioFileFormat::OGG_VORBIS_160, 160),
            (AudioFileFormat::OGG_VORBIS_96, 96),
            (AudioFileFormat::OGG_VORBIS_320, 320),
        ],
        _ => [
            (AudioFileFormat::OGG_VORBIS_320, 320),
            (AudioFileFormat::OGG_VORBIS_160, 160),
            (AudioFileFormat::OGG_VORBIS_96, 96),
        ],
    };
    let (file_id, rate) = formats
        .iter()
        .find_map(|(f, rate)| item.files.get(f).map(|id| (*id, *rate)))
        .context("no supported Ogg Vorbis audio file for this track")?;
    let track_id = SpotifyId::try_from(&item.track_id)?;
    let key = session.audio_key().request(track_id, file_id).await?;
    let audio = AudioFile::open(session, file_id, rate * 1000 / 8).await?;
    let controller = audio.get_stream_loader_controller()?;
    let bytes = controller.len() as u64;
    let cover_url = item
        .covers
        .iter()
        .filter(|c| c.width <= 640)
        .max_by_key(|c| c.width)
        .or_else(|| item.covers.first())
        .map(|c| c.url.clone());
    let (artists, album) = match item.unique_fields {
        UniqueFields::Track { artists, album, .. } => {
            (artists.0.into_iter().map(|a| a.name).collect(), album)
        }
        _ => (Vec::new(), String::new()),
    };
    let track = Download {
        version: 1,
        uri: uri.to_owned(),
        name: item.name,
        artists,
        album,
        cover_url: cover_url.clone(),
        duration_ms: item.duration_ms,
        key: key.0,
        bytes,
        sha256: String::new(),
    };
    let local_library = library.clone();
    let mut job = tokio::task::spawn_blocking(move || local_library.save(track, audio));
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(300), &mut job).await {
        result??;
    } else {
        controller.close();
        bail!("download timed out after five minutes")
    }
    download_cover(library, cover_url.as_deref()).await
}

async fn download_cover(library: &Library, url: Option<&str>) -> Result<()> {
    let Some(url) = url else {
        return Ok(());
    };
    if library.cover_is_valid(url) {
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= 5 * 1024 * 1024,
            "album cover exceeds 5 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    library.save_cover(url, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_spotify_urls_and_ids_but_rejects_paths() {
        let id = "4uLU6hMCjMI75M1A2tKUQC";
        assert_eq!(parse_source(id).unwrap(), format!("spotify:track:{id}"));
        assert_eq!(
            parse_source(&format!("https://open.spotify.com/intl-sv/album/{id}?si=x")).unwrap(),
            format!("spotify:album:{id}")
        );
        for input in [
            "../../secret",
            "spotify:track:../../secret",
            "https://evil.test/track/4uLU6hMCjMI75M1A2tKUQC",
            "spotify:episode:4uLU6hMCjMI75M1A2tKUQC",
        ] {
            assert!(parse_source(input).is_err(), "{input}");
        }
    }
}
