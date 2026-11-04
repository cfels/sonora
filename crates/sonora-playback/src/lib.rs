//! GStreamer playback engine.
//!
//! One `playbin` per process, driven by [`sonora_core::Queue`]. Gapless playback
//! comes from the `about-to-finish` signal: before the current URI ends,
//! GStreamer asks for the next one and the engine hands it the next queue entry,
//! so there is no silence between tracks.
//!
//! The engine owns no UI state. It exposes the transport (play/pause/seek/skip),
//! the queue (add, play-next, reorder, shuffle, repeat) and read-only clocks;
//! a UI layer polls or subscribes and renders that. Track ids are resolved to
//! files by a caller-supplied closure so the engine never touches SQL.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use gstreamer as gst;
use gstreamer::prelude::*;
use sonora_core::{Error, Queue, RepeatMode, Result, TrackId};

/// Turns a library track into a file the engine can play.
pub type Resolver = Box<dyn Fn(TrackId) -> Option<PathBuf> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

impl PlaybackState {
    fn code(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Paused => 1,
            Self::Playing => 2,
        }
    }
}

/// State the `about-to-finish` handler and the transport both need.
struct Shared {
    queue: Queue,
    resolve: Resolver,
    current_path: Option<PathBuf>,
}

pub struct Engine {
    pipeline: gst::Element,
    shared: Arc<Mutex<Shared>>,
    state: Arc<AtomicU8>,
    finished: Arc<AtomicBool>,
    /// f64 bits, so the volume can be read without locking.
    volume: AtomicU64,
}

impl Engine {
    /// Build an engine that plays through the system audio sink.
    pub fn new(resolve: Resolver) -> Result<Self> {
        Self::with_sink(resolve, None)
    }

    /// Build an engine with an explicit sink name. Tests use `fakesink`, which
    /// exercises the whole pipeline without an audio device.
    pub fn with_sink(resolve: Resolver, sink: Option<&str>) -> Result<Self> {
        gst::init()
            .map_err(|err| Error::playback(format!("could not initialise GStreamer: {err}")))?;

        let pipeline = gst::ElementFactory::make("playbin")
            .build()
            .map_err(|err| {
                Error::playback(format!("the GStreamer playbin element is missing: {err}"))
            })?;

        if let Some(sink_name) = sink {
            let sink = gst::ElementFactory::make(sink_name)
                .build()
                .map_err(|err| Error::playback(format!("sink {sink_name} is missing: {err}")))?;
            // A fakesink without sync would consume a whole file at CPU speed,
            // which is useless for transport tests.
            if sink_name == "fakesink" {
                sink.set_property("sync", true);
            }
            pipeline.set_property("audio-sink", &sink);
        }

        let shared = Arc::new(Mutex::new(Shared {
            queue: Queue::new(),
            resolve,
            current_path: None,
        }));

        // Gapless: playbin asks for the next URI just before the current one
        // finishes. Advancing the queue here is what makes the handover exact.
        {
            let shared = shared.clone();
            let weak = pipeline.downgrade();
            pipeline.connect("about-to-finish", false, move |_| {
                let next = {
                    let mut shared = lock(&shared);
                    next_path(&mut shared)
                };
                if let (Some(path), Some(pipeline)) = (next, weak.upgrade()) {
                    match gst::glib::filename_to_uri(&path, None::<&str>) {
                        Ok(uri) => pipeline.set_property("uri", Some(uri.as_str())),
                        Err(err) => log::warn!("playback: bad path {}: {err}", path.display()),
                    }
                }
                None
            });
        }

        let finished = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicU8::new(PlaybackState::Stopped.code()));

        let bus = pipeline
            .bus()
            .ok_or_else(|| Error::playback("playbin has no message bus"))?;
        let finished_for_bus = finished.clone();
        let state_for_bus = state.clone();
        let weak = pipeline.downgrade();
        std::thread::Builder::new()
            .name("sonora-playback-bus".to_string())
            .spawn(move || {
                while let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                    match message.view() {
                        gst::MessageView::Eos(..) => {
                            finished_for_bus.store(true, Ordering::SeqCst);
                            state_for_bus.store(PlaybackState::Stopped.code(), Ordering::SeqCst);
                        }
                        gst::MessageView::Error(error) => {
                            log::error!("playback error: {} ({:?})", error.error(), error.debug());
                            state_for_bus.store(PlaybackState::Stopped.code(), Ordering::SeqCst);
                        }
                        gst::MessageView::StateChanged(changed) => {
                            let from_pipeline = weak.upgrade().is_some_and(|pipeline| {
                                message.src() == Some(pipeline.upcast_ref())
                            });
                            if from_pipeline {
                                match changed.current() {
                                    gst::State::Playing => {
                                        state_for_bus
                                            .store(PlaybackState::Playing.code(), Ordering::SeqCst);
                                    }
                                    gst::State::Paused => {
                                        state_for_bus
                                            .store(PlaybackState::Paused.code(), Ordering::SeqCst);
                                    }
                                    gst::State::Null => {
                                        state_for_bus
                                            .store(PlaybackState::Stopped.code(), Ordering::SeqCst);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|err| Error::playback(format!("could not start the bus thread: {err}")))?;

        Ok(Self {
            pipeline,
            shared,
            state,
            finished,
            volume: AtomicU64::new(1.0f64.to_bits()),
        })
    }

    // ------------------------------------------------------------- transport

    /// Replace the queue with `tracks` and start playing the first one.
    pub fn play_tracks(&self, tracks: &[TrackId]) -> Result<Option<TrackId>> {
        let first = {
            let mut shared = lock(&self.shared);
            shared.queue.replace(tracks.iter().copied());
            shared.queue.current()
        };
        if let Some(track) = first {
            self.start_track(track)?;
        } else {
            self.stop()?;
        }
        Ok(first)
    }

    pub fn pause(&self) -> Result<()> {
        self.set_state(PlaybackState::Paused)
    }

    pub fn resume(&self) -> Result<()> {
        self.set_state(PlaybackState::Playing)
    }

    pub fn toggle(&self) -> Result<bool> {
        match self.state() {
            PlaybackState::Playing => {
                self.pause()?;
                Ok(false)
            }
            PlaybackState::Paused => {
                self.resume()?;
                Ok(true)
            }
            PlaybackState::Stopped => Ok(false),
        }
    }

    pub fn stop(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|err| Error::playback(format!("could not stop: {err}")))?;
        self.state
            .store(PlaybackState::Stopped.code(), Ordering::SeqCst);
        Ok(())
    }

    /// Skip to the next queued track (or stop when the queue is exhausted).
    pub fn next(&self) -> Result<Option<TrackId>> {
        let next = {
            let mut shared = lock(&self.shared);
            shared.queue.advance()
        };
        match next {
            Some(track) => {
                self.start_track(track)?;
                Ok(Some(track))
            }
            None => {
                self.stop()?;
                Ok(None)
            }
        }
    }

    /// Go back to the previously played track.
    pub fn previous(&self) -> Result<Option<TrackId>> {
        let previous = {
            let mut shared = lock(&self.shared);
            shared.queue.previous()
        };
        match previous {
            Some(track) => {
                self.start_track(track)?;
                Ok(Some(track))
            }
            None => Ok(None),
        }
    }

    pub fn seek_ms(&self, position_ms: u64) -> Result<()> {
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(position_ms),
            )
            .map_err(|err| Error::playback(format!("could not seek: {err}")))
    }

    pub fn set_volume(&self, volume: f64) {
        let volume = volume.clamp(0.0, 1.0);
        self.volume.store(volume.to_bits(), Ordering::SeqCst);
        self.pipeline.set_property("volume", volume);
    }

    pub fn volume(&self) -> f64 {
        f64::from_bits(self.volume.load(Ordering::SeqCst))
    }

    pub fn state(&self) -> PlaybackState {
        match self.state.load(Ordering::SeqCst) {
            2 => PlaybackState::Playing,
            1 => PlaybackState::Paused,
            _ => PlaybackState::Stopped,
        }
    }

    /// True once playback reached the end of the queue.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn position_ms(&self) -> Option<u64> {
        self.pipeline
            .query_position::<gst::ClockTime>()
            .map(|position| position.mseconds())
    }

    pub fn duration_ms(&self) -> Option<u64> {
        self.pipeline
            .query_duration::<gst::ClockTime>()
            .map(|duration| duration.mseconds())
    }

    // ----------------------------------------------------------------- queue

    pub fn current(&self) -> Option<TrackId> {
        lock(&self.shared).queue.current()
    }

    pub fn upcoming(&self) -> Vec<TrackId> {
        lock(&self.shared).queue.upcoming_tracks()
    }

    pub fn history(&self) -> Vec<TrackId> {
        lock(&self.shared).queue.history_tracks()
    }

    pub fn add_to_queue(&self, track: TrackId) {
        lock(&self.shared).queue.add_to_queue(track);
    }

    pub fn play_next(&self, track: TrackId) {
        lock(&self.shared).queue.play_next(track);
    }

    pub fn move_upcoming(&self, from: usize, to: usize) -> bool {
        lock(&self.shared).queue.move_upcoming(from, to)
    }

    pub fn remove_upcoming(&self, index: usize) -> Option<TrackId> {
        lock(&self.shared).queue.remove_upcoming(index)
    }

    pub fn shuffle(&self) -> bool {
        lock(&self.shared).queue.shuffle()
    }

    pub fn set_shuffle(&self, enabled: bool) {
        lock(&self.shared).queue.set_shuffle(enabled);
    }

    pub fn repeat(&self) -> RepeatMode {
        lock(&self.shared).queue.repeat()
    }

    pub fn set_repeat(&self, repeat: RepeatMode) {
        lock(&self.shared).queue.set_repeat(repeat);
    }

    // ---------------------------------------------------------------- private

    fn start_track(&self, track: TrackId) -> Result<()> {
        let path = {
            let mut shared = lock(&self.shared);
            resolve_track(&mut shared, track)
        };
        let path = path
            .ok_or_else(|| Error::playback(format!("track {track} has no file Sonora can play")))?;
        let uri = gst::glib::filename_to_uri(&path, None::<&str>).map_err(|err| {
            Error::playback(format!(
                "could not turn {} into a playable URI: {err}",
                path.display()
            ))
        })?;

        self.pipeline.set_property("uri", Some(uri.as_str()));
        self.set_state(PlaybackState::Playing)
    }

    fn set_state(&self, state: PlaybackState) -> Result<()> {
        let target = match state {
            PlaybackState::Stopped => gst::State::Null,
            PlaybackState::Paused => gst::State::Paused,
            PlaybackState::Playing => gst::State::Playing,
        };
        self.pipeline
            .set_state(target)
            .map_err(|err| Error::playback(format!("could not switch to {state:?}: {err}")))?;
        self.state.store(state.code(), Ordering::SeqCst);
        if state != PlaybackState::Stopped {
            self.finished.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn lock(shared: &Arc<Mutex<Shared>>) -> MutexGuard<'_, Shared> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn resolve_track(shared: &mut Shared, track: TrackId) -> Option<PathBuf> {
    let path = (shared.resolve)(track)?;
    shared.current_path = Some(path.clone());
    Some(path)
}

/// Next URI for the gapless handover.
fn next_path(shared: &mut Shared) -> Option<PathBuf> {
    let track = shared.queue.advance()?;
    resolve_track(shared, track)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn wav(path: &Path, seconds: f32) {
        let sample_rate = 44_100u32;
        let frames = (sample_rate as f32 * seconds) as u32;
        let samples: Vec<u8> = (0..frames)
            .flat_map(|index| {
                let value = if (index / 100) % 2 == 0 {
                    4000i16
                } else {
                    -4000
                };
                value.to_le_bytes()
            })
            .collect();
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        file.extend_from_slice(b"WAVEfmt ");
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&sample_rate.to_le_bytes());
        file.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        file.extend_from_slice(&2u16.to_le_bytes());
        file.extend_from_slice(&16u16.to_le_bytes());
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        file.extend_from_slice(&samples);
        std::fs::write(path, file).expect("write wav fixture");
    }

    /// Two fixtures plus an engine that resolves ids to them and writes sound
    /// into `fakesink`, so no audio device is needed.
    fn fixtures(seconds: f32) -> (tempfile::TempDir, HashMap<TrackId, PathBuf>, Engine) {
        let directory = tempfile::tempdir().expect("temp dir");
        let mut files = HashMap::new();
        for track in 1..=2 {
            let path = directory.path().join(format!("track-{track}.wav"));
            wav(&path, seconds);
            files.insert(track, path);
        }

        let resolve: Resolver = {
            let files = files.clone();
            Box::new(move |track| files.get(&track).cloned())
        };
        let engine = Engine::with_sink(resolve, Some("fakesink")).expect("engine");
        (directory, files, engine)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        predicate()
    }

    #[test]
    fn engine_starts_stopped_with_an_empty_queue() {
        let (_directory, _files, engine) = fixtures(0.1);
        assert_eq!(engine.state(), PlaybackState::Stopped);
        assert_eq!(engine.current(), None);
        assert!(engine.upcoming().is_empty());
        assert_eq!(engine.volume(), 1.0);
    }

    #[test]
    fn plays_a_track_to_the_end() {
        let (_directory, _files, engine) = fixtures(0.2);
        engine.play_tracks(&[1]).expect("play");
        assert_eq!(engine.state(), PlaybackState::Playing);
        assert_eq!(engine.current(), Some(1));

        assert!(
            wait_until(|| engine.finished(), Duration::from_secs(5)),
            "playback never reached the end"
        );
        assert_eq!(engine.state(), PlaybackState::Stopped);
    }

    #[test]
    fn gapless_handover_advances_the_queue() {
        let (_directory, _files, engine) = fixtures(0.15);
        engine.play_tracks(&[1, 2]).expect("play");
        assert_eq!(engine.upcoming(), vec![2]);

        assert!(
            wait_until(|| engine.finished(), Duration::from_secs(8)),
            "the second track never finished"
        );
        assert_eq!(engine.current(), Some(2));
        assert_eq!(engine.history(), vec![1]);
    }

    #[test]
    fn pause_freezes_the_clock_and_resume_continues() {
        let (_directory, _files, engine) = fixtures(3.0);
        engine.play_tracks(&[1]).expect("play");
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 50,
            Duration::from_secs(3)
        ));

        engine.pause().expect("pause");
        assert_eq!(engine.state(), PlaybackState::Paused);
        // Let the sink finish draining the buffer it already had.
        std::thread::sleep(Duration::from_millis(200));
        let paused_at = engine.position_ms().unwrap_or(0);
        std::thread::sleep(Duration::from_millis(300));
        let still_paused = engine.position_ms().unwrap_or(0);
        assert!(
            still_paused.abs_diff(paused_at) < 60,
            "position moved while paused: {paused_at} -> {still_paused}"
        );

        engine.resume().expect("resume");
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > still_paused + 50,
            Duration::from_secs(3)
        ));
    }

    #[test]
    fn seek_moves_the_playback_position() {
        let (_directory, _files, engine) = fixtures(4.0);
        engine.play_tracks(&[1]).expect("play");
        assert!(wait_until(
            || engine.duration_ms().is_some(),
            Duration::from_secs(3)
        ));

        engine.seek_ms(2000).expect("seek");
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) >= 1900,
            Duration::from_secs(3)
        ));
    }

    #[test]
    fn volume_is_clamped_to_the_mixer_range() {
        let (_directory, _files, engine) = fixtures(0.1);
        engine.set_volume(9.0);
        assert!((engine.volume() - 1.0).abs() < f64::EPSILON);
        engine.set_volume(-1.0);
        assert!(engine.volume().abs() < f64::EPSILON);
        engine.set_volume(0.4);
        assert!((engine.volume() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn play_next_jumps_ahead_of_the_queue() {
        let (_directory, _files, engine) = fixtures(0.1);
        engine.play_tracks(&[1, 2]).expect("play");
        engine.play_next(42);
        engine.add_to_queue(43);
        assert_eq!(engine.upcoming(), vec![42, 2, 43]);

        assert!(engine.move_upcoming(0, 2));
        assert_eq!(engine.upcoming(), vec![2, 43, 42]);
        assert_eq!(engine.remove_upcoming(1), Some(43));
        assert_eq!(engine.upcoming(), vec![2, 42]);
    }

    #[test]
    fn skipping_manually_moves_through_the_queue() {
        let (_directory, _files, engine) = fixtures(2.0);
        engine.play_tracks(&[1, 2]).expect("play");

        assert_eq!(engine.next().expect("next"), Some(2));
        assert_eq!(engine.current(), Some(2));
        assert_eq!(engine.previous().expect("previous"), Some(1));
        assert_eq!(engine.current(), Some(1));
        assert_eq!(engine.previous().expect("previous"), None);
    }

    #[test]
    fn a_track_with_no_file_is_reported() {
        let resolve: Resolver = Box::new(|_| None);
        let engine = Engine::with_sink(resolve, Some("fakesink")).expect("engine");
        let err = engine.play_tracks(&[7]).unwrap_err();
        assert!(err.to_string().contains("no file"), "{err}");
    }
}
