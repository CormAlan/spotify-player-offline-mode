use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use librespot_playback::{
    audio_backend,
    config::AudioFormat,
    convert::Converter,
    decoder::{AudioDecoder, AudioPacket, SymphoniaDecoder},
};
use parking_lot::Mutex;
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, List, ListState, Paragraph, Wrap},
};

use super::store::{Download, Library};

enum Control {
    Play(usize),
    Toggle,
    Next,
    Previous,
    Seek(i64),
    Volume(i16),
    Stop,
}

#[derive(Clone)]
struct Status {
    current: Option<usize>,
    position_ms: u32,
    playing: bool,
    volume: u8,
    error: Option<String>,
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn run(library: &Library, tracks: &[Download], notice: Option<&str>) -> Result<()> {
    anyhow::ensure!(
        !tracks.is_empty(),
        "No downloads yet. While online, run: spotify_player downloads add <Spotify URL>"
    );
    let status = Arc::new(Mutex::new(Status {
        current: None,
        position_ms: 0,
        playing: false,
        volume: crate::config::get_config()
            .app_config
            .device
            .volume
            .min(100),
        error: None,
    }));
    let (sender, receiver) = flume::unbounded();
    let worker_status = status.clone();
    let worker_library = library.clone();
    let worker_tracks = tracks.to_owned();
    let worker = std::thread::Builder::new()
        .name("offline-audio".into())
        .spawn(move || {
            if let Err(err) = audio_loop(&worker_library, &worker_tracks, &receiver, &worker_status)
            {
                let mut state = worker_status.lock();
                state.error = Some(format!("Audio output failed: {err:#}"));
                state.playing = false;
            }
        })?;
    let result = render_loop(library, tracks, &sender, &status, notice);
    let _ = sender.send(Control::Stop);
    let joined = worker.join();
    result?;
    anyhow::ensure!(joined.is_ok(), "offline audio worker failed");
    Ok(())
}

fn audio_loop(
    library: &Library,
    tracks: &[Download],
    receiver: &flume::Receiver<Control>,
    status: &Arc<Mutex<Status>>,
) -> Result<()> {
    let mut decoder: Option<SymphoniaDecoder> = None;
    // Open the device only when the user starts playback.
    let mut sink = None;
    let mut converter = Converter::new(None);
    loop {
        let playing = status.lock().playing;
        let command = if playing {
            match receiver.try_recv() {
                Ok(command) => Some(command),
                Err(flume::TryRecvError::Empty) => None,
                Err(flume::TryRecvError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
        };
        let current = status.lock().current;
        let start = match command {
            Some(Control::Stop) => break,
            Some(Control::Play(index)) => Some(index),
            Some(Control::Next) => Some(current.map_or(0, |i| (i + 1) % tracks.len())),
            Some(Control::Previous) => Some(current.map_or(0, |i| i.saturating_sub(1))),
            Some(Control::Toggle) if decoder.is_none() => Some(current.unwrap_or(0)),
            Some(Control::Toggle) => {
                status.lock().playing = !playing;
                None
            }
            Some(Control::Volume(offset)) => {
                let mut state = status.lock();
                state.volume = (i16::from(state.volume) + offset).clamp(0, 100) as u8;
                None
            }
            Some(Control::Seek(offset)) => {
                if let (Some(decoder), Some(index)) = (decoder.as_mut(), current) {
                    let position = (i64::from(status.lock().position_ms) + offset)
                        .clamp(0, i64::from(tracks[index].duration_ms.saturating_sub(1)))
                        as u32;
                    match decoder.seek(position) {
                        Ok(actual) => status.lock().position_ms = actual,
                        Err(err) => status.lock().error = Some(format!("Seek failed: {err:#}")),
                    }
                }
                None
            }
            None => None,
        };
        if let Some(index) = start.filter(|&i| i < tracks.len()) {
            match library.decoder(&tracks[index]) {
                Ok(new_decoder) => {
                    decoder = Some(new_decoder);
                    let mut state = status.lock();
                    state.current = Some(index);
                    state.position_ms = 0;
                    state.playing = true;
                    state.error = None;
                }
                Err(err) => {
                    let mut state = status.lock();
                    state.current = Some(index);
                    state.playing = false;
                    state.error = Some(format!("Cannot play {}: {err:#}", tracks[index].name));
                    decoder = None;
                }
            }
        }
        let state = status.lock().clone();
        if !state.playing {
            continue;
        }
        if sink.is_none() {
            let builder = audio_backend::find(None).context("no audio output backend available")?;
            let mut output = builder(None, AudioFormat::F32);
            output.start()?;
            sink = Some(output);
        }
        let packet = decoder.as_mut().context("no local decoder")?.next_packet();
        match packet {
            Ok(Some((position, mut packet))) => {
                status.lock().position_ms = position.position_ms;
                if let AudioPacket::Samples(samples) = &mut packet {
                    let scale = f64::from(state.volume) / 100.0;
                    for sample in samples {
                        *sample *= scale;
                    }
                }
                sink.as_mut()
                    .context("no audio output")?
                    .write(packet, &mut converter)?;
            }
            Ok(None) => {
                let next = state.current.map_or(0, |i| i + 1);
                if next < tracks.len() {
                    match library.decoder(&tracks[next]) {
                        Ok(next_decoder) => {
                            decoder = Some(next_decoder);
                            let mut state = status.lock();
                            state.current = Some(next);
                            state.position_ms = 0;
                        }
                        Err(err) => {
                            status.lock().error =
                                Some(format!("Next download is unavailable: {err:#}"));
                            status.lock().playing = false;
                            decoder = None;
                        }
                    }
                } else {
                    status.lock().playing = false;
                    decoder = None;
                }
            }
            Err(err) => {
                status.lock().error = Some(format!("Audio decoding failed: {err:#}"));
                status.lock().playing = false;
                decoder = None;
            }
        }
    }
    if let Some(mut sink) = sink {
        sink.stop()?;
    }
    Ok(())
}

fn render_loop(
    library: &Library,
    tracks: &[Download],
    sender: &flume::Sender<Control>,
    status: &Arc<Mutex<Status>>,
    notice: Option<&str>,
) -> Result<()> {
    let _guard = TerminalGuard;
    let mut terminal = crate::ui::init_terminal()?;
    let mut selected = 0;
    let mut list_state = ListState::default().with_selected(Some(0));
    #[cfg(feature = "image")]
    let picker = ratatui_image::picker::Picker::halfblocks();
    #[cfg(feature = "image")]
    let mut cover = None;
    #[cfg(feature = "image")]
    let mut cover_key = None;
    #[cfg(not(feature = "image"))]
    let _ = library;
    loop {
        let state = status.lock().clone();
        let display_index = state.current.unwrap_or(selected);
        let track = &tracks[display_index];
        terminal.draw(|frame| {
            let rows = Layout::vertical([Constraint::Length(9), Constraint::Min(2), Constraint::Length(3)]).split(frame.area());
            let columns = Layout::horizontal([Constraint::Length(18), Constraint::Min(10)]).split(rows[0]);
            let now = format!("{}\n{}\n{}\n{}  {:02}:{:02} / {:02}:{:02}   Volume {}%\n{}",
                track.name, track.artists.join(", "), track.album,
                if state.playing { "Playing" } else { "Paused" },
                state.position_ms / 60000, state.position_ms / 1000 % 60,
                track.duration_ms / 60000, track.duration_ms / 1000 % 60, state.volume,
                state.error.as_deref().or(notice).unwrap_or("Local downloads — no network connection"));
            frame.render_widget(Paragraph::new(now).block(Block::bordered().title(" Offline playback ")).wrap(Wrap { trim: true }), columns[1]);
            #[cfg(feature = "image")]
            {
                let key = (display_index, columns[0]);
                if cover_key != Some(key) {
                    cover_key = Some(key);
                    cover = track.cover_url.as_ref()
                        .and_then(|url| std::fs::read(library.cover_path(url)).ok())
                        .and_then(|bytes| image::load_from_memory(&bytes).ok())
                        .and_then(|image| crate::ui::cover_image::CoverImage::new(&picker, &image, columns[0]).ok());
                }
                if let Some(cover) = cover.as_mut() { cover.render(frame, columns[0]); }
                else { frame.render_widget(Paragraph::new("No downloaded\ncover image"), columns[0]); }
            }
            #[cfg(not(feature = "image"))]
            frame.render_widget(Paragraph::new("Artwork display\nrequires the\nimage feature"), columns[0]);
            let items = tracks.iter().enumerate().map(|(i, t)| format!("{} {} — {}", if state.current == Some(i) { "▶" } else { " " }, t.name, t.artists.join(", "))).collect::<Vec<_>>();
            frame.render_stateful_widget(List::new(items).block(Block::bordered().title(format!(" {} downloaded tracks ", tracks.len())))
                .highlight_style(Style::default().bg(Color::DarkGray)).highlight_symbol("> "), rows[1], &mut list_state);
            frame.render_widget(Paragraph::new("↑/↓ or j/k select • Enter play • Space pause • n/p skip • ←/→ seek 5s • +/- volume • q quit")
                .block(Block::bordered()).wrap(Wrap { trim: true }), rows[2]);
        })?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                let control = match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(tracks.len() - 1);
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Enter => Some(Control::Play(selected)),
                    KeyCode::Char(' ') => Some(Control::Toggle),
                    KeyCode::Char('n') => Some(Control::Next),
                    KeyCode::Char('p') => Some(Control::Previous),
                    KeyCode::Left => Some(Control::Seek(-5000)),
                    KeyCode::Right => Some(Control::Seek(5000)),
                    KeyCode::Char('+' | '=') => Some(Control::Volume(5)),
                    KeyCode::Char('-') => Some(Control::Volume(-5)),
                    _ => None,
                };
                list_state.select(Some(selected));
                if let Some(control) = control {
                    let _ = sender.send(control);
                }
            }
        }
    }
    Ok(())
}
