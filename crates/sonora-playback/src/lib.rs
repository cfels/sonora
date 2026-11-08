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
    /// Resolves a track id to a ReplayGain adjustment in decibels, if the
    /// track carries one. `None` means no adjustment. Defaults to a no-op
    /// resolver so existing callers (including tests) do not need to change.
    gain_resolver: GainResolver,
    /// The ReplayGain being applied to the currently playing track, in dB.
    current_gain_db: f32,
    audible_track: Option<TrackId>,
    stream_serial: u64,
    /// The audio sink currently set on the pipeline, if any.
    current_sink: Option<String>,
    current_path: Option<PathBuf>,
    pending: Option<(TrackId, PathBuf)>,
}

/// Turns a track id into a ReplayGain adjustment in decibels, if the track
/// carries one.
pub type GainResolver = Box<dyn Fn(TrackId) -> Option<f32> + Send + Sync>;

pub struct Engine {
    pipeline: gst::Element,
    shared: Arc<Mutex<Shared>>,
    state: Arc<AtomicU8>,
    finished: Arc<AtomicBool>,
    /// f64 bits, so the volume can be read without locking.
    volume: Arc<AtomicU64>,
    muted: AtomicBool,
    shutdown: Arc<AtomicBool>,
    bus_thread: Option<std::thread::JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
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
            gain_resolver: Box::new(|_| None),
            current_gain_db: 0.0,
            audible_track: None,
            stream_serial: 0,
            current_sink: sink.map(str::to_owned),
            current_path: None,
            pending: None,
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
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let error = Arc::new(Mutex::new(None));
        let thread_error = error.clone();
        let volume = Arc::new(AtomicU64::new(1.0f64.to_bits()));
        let thread_volume = volume.clone();
        let bus_shared = shared.clone();
        let bus_thread = std::thread::Builder::new()
            .name("sonora-playback-bus".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::SeqCst) {
                    let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
                        continue;
                    };
                    match message.view() {
                        gst::MessageView::StreamStart(..) => {
                            if let Some(pipeline) = weak.upgrade() {
                                let mut shared = lock(&bus_shared);
                                let uri = pipeline.property::<Option<String>>("current-uri");
                                let pending_matches =
                                    shared.pending.as_ref().is_some_and(|(_, path)| {
                                        gst::glib::filename_to_uri(path, None::<&str>)
                                            .ok()
                                            .is_some_and(|pending_uri| {
                                                uri.as_deref() == Some(pending_uri.as_str())
                                            })
                                    });
                                if pending_matches {
                                    if let Some((track, path)) = shared.pending.take() {
                                        shared.queue.activate_prepared(track);
                                        shared.current_path = Some(path);
                                    }
                                }
                                shared.audible_track = shared.queue.current();
                                shared.stream_serial = shared.stream_serial.wrapping_add(1);
                                let gain = shared
                                    .queue
                                    .current()
                                    .and_then(|track| (shared.gain_resolver)(track));
                                shared.current_gain_db = valid_gain(gain);
                                pipeline.set_property(
                                    "volume",
                                    effective_volume(
                                        f64::from_bits(thread_volume.load(Ordering::SeqCst)),
                                        shared.current_gain_db,
                                    ),
                                );
                            }
                        }
                        gst::MessageView::Eos(..) => {
                            finished_for_bus.store(true, Ordering::SeqCst);
                            state_for_bus.store(PlaybackState::Stopped.code(), Ordering::SeqCst);
                        }
                        gst::MessageView::Error(error) => {
                            let message = format!("Playback failed: {}", error.error());
                            log::error!("{message} ({:?})", error.debug());
                            *thread_error
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
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
            volume,
            muted: AtomicBool::new(false),
            shutdown,
            bus_thread: Some(bus_thread),
            error,
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
            PlaybackState::Stopped => match self.current() {
                Some(track) => {
                    self.start_track(track)?;
                    Ok(true)
                }
                None => Ok(false),
            },
        }
    }

    pub fn stop(&self) -> Result<()> {
        lock(&self.shared).pending = None;
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
        let duration = self
            .duration_ms()
            .filter(|duration| *duration > 0)
            .ok_or_else(|| Error::playback("this track has no known duration"))?;
        if !self.is_seekable() {
            return Err(Error::playback("this track does not support seeking"));
        }
        self.pipeline
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(position_ms.min(duration.saturating_sub(1))),
            )
            .map_err(|err| Error::playback(format!("could not seek: {err}")))
    }

    pub fn set_volume(&self, volume: f64) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            return;
        };
        self.volume.store(volume.to_bits(), Ordering::SeqCst);
        // Re-apply the current track's ReplayGain on top of the new user volume.
        let gain = lock(&self.shared).current_gain_db;
        self.pipeline
            .set_property("volume", effective_volume(volume, gain));
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        self.pipeline.set_property("mute", muted);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    pub fn is_seekable(&self) -> bool {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        self.pipeline.query(&mut query) && query.result().0
    }

    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
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

    /// A stream start distinguishes repeated plays of the same track from a
    /// pause/resume, and avoids counting a prepared URI as an audible play.
    pub fn active_session(&self) -> (Option<TrackId>, u64) {
        let shared = lock(&self.shared);
        (shared.audible_track, shared.stream_serial)
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

    /// Snapshot the playback queue for persistence.
    ///
    /// Returns the full queue state including the shuffle RNG, so a saved
    /// snapshot can be restored verbatim on the next launch.
    pub fn snapshot(&self) -> sonora_core::QueueSnapshot {
        lock(&self.shared).queue.snapshot()
    }

    /// Restore a previously saved queue snapshot.
    ///
    /// Replaces the current queue with the snapshot's state, preserving the
    /// RNG so that a resumed shuffle continues from where it left off.
    /// Does not touch the audio pipeline or the current playback position.
    pub fn restore_from_snapshot(&self, snapshot: sonora_core::QueueSnapshot) {
        let mut shared = lock(&self.shared);
        shared.queue = sonora_core::Queue::from_snapshot(snapshot);
    }

    // ----------------------------------------------------------------- device

    /// Configure a sink before playback starts. Replacing a live audio sink is
    /// unsafe on some Windows backends; the UI saves changes for the next launch.
    pub fn set_device(&self, name: Option<&str>) -> Result<()> {
        if self.state() != PlaybackState::Stopped
            || self.pipeline.current_state() != gst::State::Null
        {
            return Err(Error::device("restart Sonora to change the audio output"));
        }
        let sink_element = match name {
            Some(sink_name) => {
                // Validate that the sink element factory exists before trying to
                // create it.
                if gst::ElementFactory::find(sink_name).is_none() {
                    return Err(Error::device(format!(
                        "no audio output named {sink_name} is available on this system"
                    )));
                }
                gst::ElementFactory::make(sink_name)
                    .build()
                    .map_err(|err| {
                        Error::device(format!(
                            "could not create the audio output {sink_name}: {err}"
                        ))
                    })?
            }
            None => {
                // Reverting to the system default: use autoaudiosink when it
                // exists; on systems (or test environments) without it, leave the
                // current sink in place so playback is not interrupted.
                match gst::ElementFactory::find("autoaudiosink") {
                    Some(_) => {
                        gst::ElementFactory::make("autoaudiosink")
                            .build()
                            .map_err(|err| {
                                Error::device(format!(
                                    "could not create the default audio output: {err}"
                                ))
                            })?
                    }
                    None => return Ok(()),
                }
            }
        };
        // set_property on a GStreamer element returns () and logs failures
        // internally, so the builder validation above is the main guard.
        if name == Some("fakesink") {
            sink_element.set_property("sync", true);
        }
        self.pipeline.set_property("audio-sink", &sink_element);
        lock(&self.shared).current_sink = name.map(String::from);
        Ok(())
    }

    /// The audio output device currently in use, if one was selected.
    pub fn device(&self) -> Option<String> {
        lock(&self.shared).current_sink.clone()
    }

    /// Audio output sinks that can be offered in a device picker.
    ///
    /// Candidates are filtered against the element factories GStreamer actually
    /// has, so a picker never lists a device that would fail to open. The system
    /// default (`autoaudiosink`) is listed first when available; if nothing
    /// matches, it is still returned so the picker always has an entry and the
    /// device can be reset to the default.
    pub fn available_sinks() -> Vec<String> {
        if let Err(err) = gst::init() {
            log::warn!("could not enumerate audio outputs: {err}");
            return Vec::new();
        }
        const CANDIDATES: &[&str] = &[
            "autoaudiosink",
            "pulsesink",
            "pipewiresink",
            "alsasink",
            "oss4sink",
            "osssink",
            "jackaudiosink",
            "directsoundsink",
            "wasapi2sink",
            "wasapisink",
            "coreaudiosink",
        ];
        let mut sinks: Vec<String> = CANDIDATES
            .iter()
            .filter(|name| gst::ElementFactory::find(name).is_some())
            .map(|name| (*name).to_string())
            .collect();
        if sinks.is_empty() {
            sinks.push("autoaudiosink".to_string());
        }
        sinks
    }

    // ------------------------------------------------------------ replaygain

    /// Supply a resolver that turns a track id into its ReplayGain adjustment.
    /// Called once, after the engine is built, with a resolver that reads from
    /// the library's current track data. Defaults to a no-op until then.
    pub fn set_gain_resolver(&self, resolver: GainResolver) {
        lock(&self.shared).gain_resolver = resolver;
    }

    /// The ReplayGain adjustment currently applied to the playing track, in dB.
    /// `None` when no track is playing or the track carries no gain metadata.
    pub fn track_gain(&self) -> Option<f32> {
        let gain = lock(&self.shared).current_gain_db;
        if gain == 0.0 {
            None
        } else {
            Some(gain)
        }
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

        lock(&self.shared).pending = None;
        // READY permits a new URI while retaining the audio device resources.
        self.pipeline
            .set_state(gst::State::Ready)
            .map_err(|err| Error::playback(format!("could not prepare a track change: {err}")))?;
        self.pipeline.set_property("uri", Some(uri.as_str()));

        // Apply this track's ReplayGain, if it carries one, on top of the
        // user's volume. Gapless handover uses this path too, so the gain is
        // applied for every track the engine starts internally.
        {
            let gain = {
                let shared = lock(&self.shared);
                (shared.gain_resolver)(track)
            };
            self.apply_gain(gain);
        }

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

    /// Apply a ReplayGain adjustment to the mixer volume, on top of the user's
    /// volume setting. A gain of `None` or `0.0` restores the user volume.
    fn apply_gain(&self, gain_db: Option<f32>) {
        let gain = valid_gain(gain_db);
        lock(&self.shared).current_gain_db = gain;
        self.pipeline
            .set_property("volume", effective_volume(self.volume(), gain));
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.bus_thread.take() {
            if thread.join().is_err() {
                log::error!("playback bus thread panicked during shutdown");
            }
        }
    }
}

fn valid_gain(gain: Option<f32>) -> f32 {
    gain.filter(|value| value.is_finite()).unwrap_or(0.0)
}

fn effective_volume(volume: f64, gain: f32) -> f64 {
    (volume * (f64::from(gain) / 20.0 * std::f64::consts::LN_10).exp()).clamp(0.0, 1.0)
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
    let mut preview = shared.queue.clone();
    let track = preview.advance()?;
    let path = (shared.resolve)(track)?;
    shared.pending = Some((track, path.clone()));
    Some(path)
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
    fn replay_gain_resolver_drives_the_applied_gain() {
        let (_directory, _files, engine) = fixtures(0.5);
        engine.set_gain_resolver(Box::new(|_| Some(-6.0)));
        engine.play_tracks(&[1]).expect("play");
        assert_eq!(engine.track_gain(), Some(-6.0));

        // A track without gain restores the user's volume, so the adjustment
        // must not linger from the track that played before it.
        engine.set_gain_resolver(Box::new(|_| None));
        engine.play_tracks(&[2]).expect("play");
        assert_eq!(engine.track_gain(), None);
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
    #[test]
    fn bus_survives_an_idle_period_before_playback() {
        let (_directory, _files, engine) = fixtures(0.3);
        std::thread::sleep(Duration::from_millis(450));
        engine.play_tracks(&[1]).unwrap();
        assert!(
            wait_until(|| engine.finished(), Duration::from_secs(5)),
            "bus stopped watching after an idle timeout"
        );
    }

    #[test]
    fn mixer_volume_works_without_gain_and_while_paused() {
        let (_directory, _files, engine) = fixtures(4.0);
        engine.play_tracks(&[1]).unwrap();
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 50,
            Duration::from_secs(3)
        ));
        engine.set_volume(0.37);
        assert!((engine.pipeline.property::<f64>("volume") - 0.37).abs() < 1e-6);
        engine.pause().unwrap();
        engine.set_volume(0.19);
        assert!((engine.pipeline.property::<f64>("volume") - 0.19).abs() < 1e-6);
        engine.set_muted(true);
        assert!(engine.pipeline.property::<bool>("mute"));
        assert_eq!(engine.volume(), 0.19);
        engine.set_muted(false);
        assert!(!engine.pipeline.property::<bool>("mute"));
        engine.set_volume(f64::NAN);
        assert_eq!(engine.volume(), 0.19);
    }

    #[test]
    fn gain_tracks_and_manual_skips_keep_user_volume() {
        let (_directory, _files, engine) = fixtures(4.0);
        engine.set_gain_resolver(Box::new(|track| (track == 1).then_some(-6.0)));
        engine.set_volume(0.4);
        engine.play_tracks(&[1, 2]).unwrap();
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 50,
            Duration::from_secs(3)
        ));
        assert!(
            (engine.pipeline.property::<f64>("volume") - 0.4 * 10f64.powf(-6.0 / 20.0)).abs()
                < 1e-6
        );
        engine.next().unwrap();
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 50,
            Duration::from_secs(3)
        ));
        assert!((engine.pipeline.property::<f64>("volume") - 0.4).abs() < 1e-6);
        assert_eq!(
            engine.pipeline.property::<Option<String>>("current-uri"),
            Some(
                gst::glib::filename_to_uri(
                    engine.shared.lock().unwrap().current_path.clone().unwrap(),
                    None::<&str>
                )
                .unwrap()
                .to_string()
            )
        );
    }

    #[test]
    fn rapid_manual_switches_keep_the_sink_and_finish_on_the_last_selection() {
        let (_directory, _files, engine) = fixtures(4.0);
        engine.play_tracks(&[1]).unwrap();
        let sink = engine.pipeline.property::<gst::Element>("audio-sink");
        for id in [2, 1, 2, 1, 2] {
            engine.play_tracks(&[id]).unwrap();
        }
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 50,
            Duration::from_secs(3)
        ));
        assert_eq!(engine.current(), Some(2));
        assert_eq!(engine.pipeline.property::<gst::Element>("audio-sink"), sink);
        assert!(engine.take_error().is_none());
    }

    #[test]
    fn seek_while_paused_and_refuse_live_sink_replacement() {
        let (_directory, _files, engine) = fixtures(4.0);
        assert!(engine.seek_ms(1000).is_err());
        engine.play_tracks(&[1]).unwrap();
        assert!(wait_until(|| engine.is_seekable(), Duration::from_secs(3)));
        let original = engine.pipeline.property::<gst::Element>("audio-sink");
        assert!(engine.set_device(Some("autoaudiosink")).is_err());
        assert_eq!(
            engine.pipeline.property::<gst::Element>("audio-sink"),
            original
        );
        engine.pause().unwrap();
        engine.seek_ms(2300).unwrap();
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0).abs_diff(2300) < 100,
            Duration::from_secs(3)
        ));
    }

    #[test]
    fn gapless_preparation_does_not_change_the_audible_track_or_gain() {
        let (_directory, _files, engine) = fixtures(2.0);
        engine.set_gain_resolver(Box::new(|track| Some(if track == 1 { -3.0 } else { -9.0 })));
        engine.play_tracks(&[1, 2]).unwrap();
        assert!(wait_until(
            || engine.position_ms().unwrap_or(0) > 200,
            Duration::from_secs(3)
        ));
        assert_eq!(engine.current(), Some(1));
        assert_eq!(engine.track_gain(), Some(-3.0));
        assert_eq!(engine.upcoming(), vec![2]);
        assert!(wait_until(
            || engine.current() == Some(2),
            Duration::from_secs(5)
        ));
        assert_eq!(engine.track_gain(), Some(-9.0));
    }
    #[test]
    fn repeating_one_track_starts_a_new_audible_session() {
        let (_directory, _files, engine) = fixtures(0.25);
        engine.set_repeat(RepeatMode::One);
        engine.play_tracks(&[1]).unwrap();
        assert!(wait_until(
            || engine.active_session().1 >= 2,
            Duration::from_secs(5)
        ));
        assert_eq!(engine.active_session().0, Some(1));
        assert_eq!(engine.current(), Some(1));
    }
}
