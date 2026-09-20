# Offline downloads

This fork adds explicit downloads and an independent local playback path. It does
not import downloads made by Spotify's official app.

## Build and use

```sh
cargo build --release --locked --features image
./target/release/spotify_player downloads add 'SPOTIFY_TRACK_ALBUM_OR_PLAYLIST_URL'
./target/release/spotify_player --offline
```

Existing configuration and cached Spotify credentials can be used. `-c` and `-C`
select alternative config and credential/cache folders. `download_folder` selects
the persistent downloads location. These global options go before the subcommand.

`downloads verify` checks each audio checksum, decodes the complete track and checks
its cover image. It works without a Spotify session. `downloads list` outputs only
public metadata, never playback keys. `downloads remove` deletes one track;
shared artwork is retained. A partially downloaded playlist plays its available
tracks in playlist order; rerun `downloads add` to retry failed downloads.

## Design

- A versioned `.spdl` file contains a bounded JSON header and the complete encrypted
  Vorbis stream. The header includes track metadata, the audio key, payload length
  and SHA-256 digest. The original requested track URI is retained for market
  relinking. Only tracks available to the authenticated account are downloaded.
- Temporary files use owner-only permissions. A successful length, decryption and
  decoder check precedes the atomic rename. A failed replacement preserves the old
  download. Files are capped at 128 MiB; metadata is capped at 64 KiB.
- Album covers are downloaded separately and deduplicated by their source URL.
  Requests have a 30-second timeout and a 5 MiB response limit. Builds with the
  `image` feature validate the image before saving it. Cover failure leaves the
  audio intact and reports the download as incomplete; rerunning repairs artwork.
- The local player reads and decrypts one track into memory, validates its checksum,
  removes Spotify's custom 167-byte Ogg header and uses the existing librespot
  decoder and audio output backend. It never creates a Spotify session or HTTP
  client. Expect memory use above the size of the compressed track being played.
- Network access is confined to `downloads add` and normal online startup. Track
  downloads do not need the Web API. Album/playlist resolution still uses it and
  inherits upstream rate-limit handling. Downloaded collections retain track order.
- Normal startup can fall back to the local library after connection failure. The
  offline UI is entered before normal UI/input threads are started, preventing
  competing terminal readers. Explicit `--offline` always bypasses online startup.

## Current limits

- The offline player is a separate view with a local queue; no mid-song handoff
  from Spotify streaming, Connect synchronization, remote CLI control or MPRIS.
- Ogg Vorbis tracks at 96/160/320 kbps are supported. Podcast downloads and lossless
  formats are not implemented. The requested bitrate follows `[device].bitrate`.
- No automatic disk quota or eviction; downloads remain until explicitly removed.
- A track marked unavailable at download time is rejected unless an available
  market alternative exists. New downloads still depend on Spotify's service.
- Startup fallback requires a nonempty readable downloads library. Initial login
  prompts may still require interaction; use `--offline` to avoid authentication.

## Validation

Tests cover source parsing/path rejection, encrypted audio round trips with generated
silence, corruption/truncation detection, atomic replacement failure, private file
permissions, collection ordering and persistent artwork. The synthetic audio fixture
was generated with:

```sh
ffmpeg -f lavfi -i 'anullsrc=r=44100:cl=stereo' -t 0.12 -c:a libvorbis silence.ogg
```

For an end-to-end network isolation check on Linux, after downloading a track:

```sh
# Requires permission to create a network namespace.
unshare --net ./target/release/spotify_player downloads verify
unshare --net ./target/release/spotify_player --offline
```

Run these in a real terminal (or a PTY harness that answers cursor-position queries)
to test the UI. Offline playback itself requires an accessible local audio device.
