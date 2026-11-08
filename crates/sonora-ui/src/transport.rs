use super::*;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

#[derive(Clone)]
pub(super) struct PlaybackHandle {
    pub engine: Arc<Engine>,
    sender: Sender<Command>,
    latest_play: Arc<AtomicU64>,
    weak: Weak<AppWindow>,
    lookup: Arc<Mutex<TrackLookup>>,
}

enum Command {
    Play(Vec<TrackId>, u64),
    Toggle,
    Previous,
    Next,
    Shutdown,
}

pub(super) struct PlaybackWorker {
    pub playback: PlaybackHandle,
    thread: Option<JoinHandle<()>>,
}

impl PlaybackWorker {
    pub fn new(
        weak: Weak<AppWindow>,
        engine: Arc<Engine>,
        lookup: Arc<Mutex<TrackLookup>>,
    ) -> anyhow::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let latest_play = Arc::new(AtomicU64::new(0));
        let playback = PlaybackHandle {
            engine: engine.clone(),
            sender,
            latest_play: latest_play.clone(),
            weak: weak.clone(),
            lookup: lookup.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("sonora-transport".into())
            .spawn(move || {
                for command in receiver {
                    let selection = match &command {
                        Command::Play(_, generation) => Some(*generation),
                        _ => None,
                    };
                    let result = match command {
                        Command::Play(tracks, generation) => {
                            if generation != latest_play.load(Ordering::SeqCst) {
                                continue;
                            }
                            engine.play_tracks(&tracks).map(|_| ())
                        }
                        Command::Toggle => engine.toggle().map(|_| ()),
                        Command::Previous => engine.previous().map(|_| ()),
                        Command::Next => engine.next().map(|_| ()),
                        Command::Shutdown => {
                            if let Err(err) = engine.stop() {
                                log::error!("could not stop playback: {err}");
                            }
                            break;
                        }
                    };
                    if let Err(err) = result {
                        set_status(&weak, format!("Playback failed: {err}"));
                        let latest_play = latest_play.clone();
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            if selection.is_some_and(|generation| {
                                generation == latest_play.load(Ordering::SeqCst)
                            }) {
                                ui.set_pending_track(0);
                            }
                        });
                    }
                    refresh_playback(&weak, &engine, &lookup);
                }
            })
            .map_err(|err| anyhow::anyhow!("could not start the transport worker: {err}"))?;
        Ok(Self {
            playback,
            thread: Some(thread),
        })
    }

    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        if let Some(thread) = self.thread.take() {
            // A closed receiver means the worker already exited; joining still
            // reports a panic rather than hiding it behind a send error.
            let _ = self.playback.sender.send(Command::Shutdown);
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("transport worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for PlaybackWorker {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown() {
            log::error!("{err}");
        }
    }
}

impl PlaybackHandle {
    pub fn play_tracks(&self, tracks: &[TrackId]) -> anyhow::Result<()> {
        let generation = self
            .latest_play
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        if let Some(track) = tracks.first() {
            if let Some(ui) = self.weak.upgrade() {
                let id = saturating_i32((*track).max(0) as u64);
                ui.set_pending_track(id);
                ui.global::<RowArtwork>().set_current_track(id);
                let info = lock(&self.lookup).labels.get(track).cloned();
                if let Some(info) = info {
                    apply_now_playing(&ui, &info);
                }
                if ui.get_lyrics_open() {
                    ui.set_lyrics_track(id);
                    ui.set_lyric_lines(model(Vec::new()));
                    ui.set_lyrics_text("".into());
                    ui.set_lyrics_status("Loading lyrics…".into());
                }
            }
        }
        self.send(Command::Play(tracks.to_vec(), generation))
    }

    pub fn toggle(&self) -> anyhow::Result<()> {
        self.send(Command::Toggle)
    }
    pub fn previous(&self) -> anyhow::Result<()> {
        self.send(Command::Previous)
    }
    pub fn next(&self) -> anyhow::Result<()> {
        self.send(Command::Next)
    }

    fn send(&self, command: Command) -> anyhow::Result<()> {
        self.sender
            .send(command)
            .map_err(|_| anyhow::anyhow!("the playback worker is unavailable"))
    }
}
