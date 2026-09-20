use std::{
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::PathBuf,
};

use anyhow::{Context, Result};
use librespot_audio::AudioDecrypt;
use librespot_core::audio_key::AudioKey;
use librespot_playback::decoder::{AudioDecoder, SymphoniaDecoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::core::probe::Hint;

const MAGIC: &[u8; 8] = b"SPDL0001";
const MAX_HEADER: usize = 65536;
const MAX_AUDIO: u64 = 128 * 1024 * 1024;

fn checksum(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

// No Debug implementation: the audio key must not leak through logging.
#[derive(Clone, Serialize, Deserialize)]
pub struct Download {
    pub version: u32,
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover_url: Option<String>,
    pub duration_ms: u32,
    pub key: [u8; 16],
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn create(&self) -> Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.root)?;
        Ok(())
    }

    pub fn cover_path(&self, url: &str) -> PathBuf {
        self.root
            .join(format!("cover-{}.img", checksum(url.as_bytes())))
    }

    pub fn save_cover(&self, url: &str, bytes: &[u8]) -> Result<()> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 5 * 1024 * 1024,
            "invalid cover image size"
        );
        #[cfg(feature = "image")]
        image::load_from_memory(bytes).context("invalid album cover image")?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.cover_path(url))?;
        Ok(())
    }

    pub fn cover_is_valid(&self, url: &str) -> bool {
        let Ok(bytes) = fs::read(self.cover_path(url)) else {
            return false;
        };
        if bytes.is_empty() {
            return false;
        }
        #[cfg(feature = "image")]
        {
            image::load_from_memory(&bytes).is_ok()
        }
        #[cfg(not(feature = "image"))]
        {
            true
        }
    }

    pub fn track(&self, uri: &str) -> Result<Download> {
        read_header(&mut File::open(self.path(uri)?)?)
    }

    fn path(&self, uri: &str) -> Result<PathBuf> {
        let normalized = super::parse_source(uri)?;
        anyhow::ensure!(
            normalized.starts_with("spotify:track:"),
            "expected a track URI"
        );
        let id = normalized.rsplit(':').next().context("missing track ID")?;
        Ok(self.root.join(format!("{id}.spdl")))
    }

    fn collection_path(&self, uri: &str) -> PathBuf {
        self.root.join(format!("{}.json", checksum(uri.as_bytes())))
    }

    pub fn save_collection(&self, uri: &str, tracks: &[String]) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, tracks)?;
        file.as_file().sync_all()?;
        file.persist(self.collection_path(uri))?;
        Ok(())
    }

    pub fn select(&self, source: Option<&str>) -> Result<Vec<Download>> {
        let Some(source) = source else {
            return self.tracks();
        };
        let uris = if source.starts_with("spotify:track:") {
            vec![source.to_owned()]
        } else {
            serde_json::from_reader(
                File::open(self.collection_path(source))
                    .context("this album or playlist has not been downloaded")?,
            )?
        };
        let mut tracks = Vec::new();
        for uri in uris {
            let path = self.path(&uri)?;
            if path.exists() {
                tracks.push(read_header(&mut File::open(path)?)?);
            }
        }
        anyhow::ensure!(
            !tracks.is_empty(),
            "no downloaded tracks for this selection"
        );
        Ok(tracks)
    }

    pub fn tracks(&self) -> Result<Vec<Download>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut tracks = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "spdl") {
                match File::open(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|mut f| read_header(&mut f))
                {
                    Ok(track) => {
                        anyhow::ensure!(
                            self.path(&track.uri)? == path,
                            "download filename does not match its track URI"
                        );
                        tracks.push(track);
                    }
                    Err(err) => anyhow::bail!(
                        "invalid download {}: {err:#}; remove or download it again with --force",
                        path.display()
                    ),
                }
            }
        }
        tracks.sort_by(|a, b| a.name.cmp(&b.name).then(a.uri.cmp(&b.uri)));
        Ok(tracks)
    }

    pub fn contains_valid(&self, uri: &str) -> bool {
        self.path(uri)
            .and_then(|p| read_header(&mut File::open(p)?))
            .and_then(|t| self.decoder(&t))
            .is_ok()
    }

    pub fn remove(&self, uri: &str) -> Result<()> {
        fs::remove_file(self.path(uri)?).context("remove downloaded track")
    }

    pub fn save(&self, mut track: Download, audio: impl Read) -> Result<()> {
        anyhow::ensure!(
            track.bytes > 167 && track.bytes <= MAX_AUDIO,
            "unsupported download size"
        );
        let mut encrypted = Vec::new();
        audio.take(MAX_AUDIO + 1).read_to_end(&mut encrypted)?;
        anyhow::ensure!(
            encrypted.len() as u64 == track.bytes,
            "incomplete download: expected {} bytes, received {}",
            track.bytes,
            encrypted.len()
        );
        track.sha256 = checksum(&encrypted);
        let mut decoder = decode(&track, &encrypted)?;
        anyhow::ensure!(
            decoder.next_packet()?.is_some(),
            "download has no playable audio"
        );
        let header = serde_json::to_vec(&track)?;
        anyhow::ensure!(header.len() <= MAX_HEADER, "download metadata too large");
        // A single atomic rename commits metadata, key and audio together. Temp files are
        // owner-only and automatically removed on failures; existing downloads survive.
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(MAGIC)?;
        file.write_all(&(header.len() as u32).to_le_bytes())?;
        file.write_all(&header)?;
        file.write_all(&encrypted)?;
        file.as_file().sync_all()?;
        file.persist(self.path(&track.uri)?)?;
        Ok(())
    }

    pub fn decoder(&self, track: &Download) -> Result<SymphoniaDecoder> {
        let mut file = File::open(self.path(&track.uri)?)?;
        let saved = read_header(&mut file)?;
        anyhow::ensure!(saved.uri == track.uri, "download track mismatch");
        let mut encrypted = Vec::new();
        file.take(MAX_AUDIO + 1).read_to_end(&mut encrypted)?;
        anyhow::ensure!(encrypted.len() as u64 == saved.bytes, "truncated download");
        anyhow::ensure!(
            checksum(&encrypted) == saved.sha256,
            "download checksum mismatch"
        );
        decode(&saved, &encrypted)
    }

    pub fn verify_cover(&self, track: &Download) -> Result<()> {
        if let Some(url) = &track.cover_url {
            let bytes = fs::read(self.cover_path(url))
                .context("album cover is missing; rerun downloads add")?;
            anyhow::ensure!(!bytes.is_empty(), "album cover is empty");
            #[cfg(feature = "image")]
            image::load_from_memory(&bytes).context("album cover is corrupt")?;
        }
        Ok(())
    }

    pub fn verify(&self, track: &Download) -> Result<usize> {
        let mut decoder = self.decoder(track)?;
        let mut packets = 0;
        while decoder.next_packet()?.is_some() {
            packets += 1;
        }
        anyhow::ensure!(packets > 0, "download has no audio packets");
        self.verify_cover(track)?;
        Ok(packets)
    }
}

fn read_header(file: &mut File) -> Result<Download> {
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == MAGIC, "not a supported download file");
    let mut length = [0u8; 4];
    file.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    anyhow::ensure!(length <= MAX_HEADER, "download metadata too large");
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    let track: Download = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        track.version == 1 && track.bytes > 167 && track.bytes <= MAX_AUDIO,
        "unsupported download version or size"
    );
    anyhow::ensure!(
        file.metadata()?.len() == 12 + length as u64 + track.bytes,
        "incomplete download"
    );
    super::parse_source(&track.uri)?;
    Ok(track)
}

fn decode(track: &Download, encrypted: &[u8]) -> Result<SymphoniaDecoder> {
    let mut decrypted = Vec::with_capacity(encrypted.len());
    AudioDecrypt::new(Some(AudioKey(track.key)), encrypted).read_to_end(&mut decrypted)?;
    // Spotify's Vorbis files begin with a 167-byte custom metadata packet.
    anyhow::ensure!(
        decrypted.get(167..171) == Some(b"OggS"),
        "download could not be decrypted as Ogg Vorbis"
    );
    let mut hint = Hint::new();
    hint.with_extension("ogg");
    Ok(SymphoniaDecoder::new(
        Cursor::new(decrypted.split_off(167)),
        hint,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_download_round_trip_decodes_and_detects_tampering() {
        use std::io::{Seek, SeekFrom};
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().join("downloads"));
        lib.create().unwrap();
        let mut plain = vec![0; 167];
        plain.extend_from_slice(include_bytes!("testdata/silence.ogg"));
        let key = [42; 16];
        let mut encrypted = Vec::new();
        AudioDecrypt::new(Some(AudioKey(key)), plain.as_slice())
            .read_to_end(&mut encrypted)
            .unwrap();
        let t = Download {
            version: 1,
            uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
            name: "Silence".into(),
            artists: vec!["Test".into()],
            album: "Fixture".into(),
            cover_url: None,
            duration_ms: 120,
            key,
            bytes: encrypted.len() as u64,
            sha256: String::new(),
        };
        lib.save(t.clone(), encrypted.as_slice()).unwrap();
        assert!(lib.contains_valid(&t.uri));
        let mut decoder = lib.decoder(&t).unwrap();
        assert!(decoder.next_packet().unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(lib.path(&t.uri).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&lib.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(lib.path(&t.uri).unwrap())
            .unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(&[encrypted[encrypted.len() - 1] ^ 1])
            .unwrap();
        assert!(lib
            .decoder(&t)
            .err()
            .unwrap()
            .to_string()
            .contains("checksum"));
    }

    #[test]
    #[cfg(feature = "image")]
    fn cover_survives_reload_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().into());
        let image = image::DynamicImage::new_rgb8(8, 8);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let url = "https://i.scdn.co/image/test";
        lib.save_cover(url, bytes.get_ref()).unwrap();
        let mut t = fixture(&lib, &[0; 200]);
        t.cover_url = Some(url.into());
        let reopened = Library::new(dir.path().into());
        reopened.verify_cover(&t).unwrap();
        assert!(reopened.save_cover(url, b"broken image").is_err());
        reopened.verify_cover(&t).unwrap();
    }

    fn fixture(library: &Library, payload: &[u8]) -> Download {
        let t = Download {
            version: 1,
            uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
            name: "Test".into(),
            artists: vec![],
            album: String::new(),
            cover_url: None,
            duration_ms: 1000,
            key: [0; 16],
            bytes: payload.len() as u64,
            sha256: checksum(payload),
        };
        let header = serde_json::to_vec(&t).unwrap();
        let mut f = File::create(library.path(&t.uri).unwrap()).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&(header.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&header).unwrap();
        f.write_all(payload).unwrap();
        t
    }

    #[test]
    fn rejects_partial_and_corrupt_files_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().into());
        let t = fixture(&lib, &[0; 200]);
        assert_eq!(lib.tracks().unwrap().len(), 1);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(lib.path(&t.uri).unwrap())
            .unwrap();
        f.write_all(&[1]).unwrap();
        assert!(lib.tracks().is_err());
        let t = fixture(&lib, &[1; 200]);
        assert!(lib.decoder(&t).is_err());
        assert!(!lib.contains_valid(&t.uri));
    }

    #[test]
    fn failed_replacement_preserves_existing_download() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().into());
        let t = fixture(&lib, &[0; 200]);
        let before = fs::read(lib.path(&t.uri).unwrap()).unwrap();
        assert!(lib.save(t.clone(), Cursor::new(vec![0; 100])).is_err());
        assert_eq!(before, fs::read(lib.path(&t.uri).unwrap()).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn collection_preserves_order_and_missing_tracks_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new(dir.path().into());
        let t = fixture(&lib, &[0; 200]);
        let album = "spotify:album:4uLU6hMCjMI75M1A2tKUQC";
        lib.save_collection(
            album,
            &[
                t.uri.clone(),
                "spotify:track:0000000000000000000000".into(),
                t.uri.clone(),
            ],
        )
        .unwrap();
        assert_eq!(lib.select(Some(album)).unwrap().len(), 2);
        assert!(lib.path("../../secret").is_err());
    }
}
