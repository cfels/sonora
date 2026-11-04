//! The playback queue.
//!
//! Sonora's queue follows Apple Music's behaviour rather than a flat playlist:
//!
//! - Shuffling only reorders the **upcoming** tracks. The track that is playing
//!   and everything already played stay where they are.
//! - *Play next* puts a track at the very front of the queue; *add to queue*
//!   appends it to the end.
//! - The upcoming list can be reordered by dragging (see [`Queue::move_upcoming`]).
//! - Turning shuffle off restores the order the tracks were originally queued
//!   in, including tracks inserted with *play next*.
//!
//! The queue is pure data: it knows nothing about GStreamer, files or the UI.
//! That keeps the tricky ordering rules testable without an audio device, which
//! is exactly what the playback engine will drive.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::TrackId;

/// What happens once the last queued track finishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    /// Restart the queue once it is exhausted.
    All,
    /// Replay the current track until the mode changes.
    One,
}

impl RepeatMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::All => "Repeat all",
            Self::One => "Repeat one",
        }
    }
}

/// One queued track plus the order it should have when shuffle is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    track: TrackId,
    /// Gaps are fine: only the relative order matters. Tracks queued with
    /// *play next* get ranks below everything else, appended tracks above.
    rank: i64,
}

/// The upcoming tracks plus what is playing and what has already played.
#[derive(Debug, Clone)]
pub struct Queue {
    current: Option<Entry>,
    upcoming: Vec<Entry>,
    /// Most recently played last.
    history: Vec<Entry>,
    shuffle: bool,
    repeat: RepeatMode,
    rng_state: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self::with_seed(seed)
    }

    /// Deterministic queue, used by tests and by anything that needs
    /// reproducible shuffle order.
    pub fn with_seed(seed: u64) -> Self {
        Self {
            current: None,
            upcoming: Vec::new(),
            history: Vec::new(),
            shuffle: false,
            repeat: RepeatMode::Off,
            // xorshift must never be seeded with zero.
            rng_state: seed | 1,
        }
    }

    /// A queue that starts with the first track of `tracks` playing.
    pub fn from_tracks<I: IntoIterator<Item = TrackId>>(tracks: I) -> Self {
        let mut queue = Self::new();
        queue.replace(tracks);
        queue
    }

    /// Deterministic [`Queue::from_tracks`] for tests.
    pub fn from_tracks_with_seed<I: IntoIterator<Item = TrackId>>(tracks: I, seed: u64) -> Self {
        let mut queue = Self::with_seed(seed);
        queue.replace(tracks);
        queue
    }

    /// Clear the queue and start again, playing the first track immediately.
    pub fn replace<I: IntoIterator<Item = TrackId>>(&mut self, tracks: I) {
        self.history.clear();
        self.upcoming.clear();
        self.current = None;

        let mut tracks = tracks.into_iter();
        if let Some(first) = tracks.next() {
            self.current = Some(Entry {
                track: first,
                rank: 0,
            });
        }
        self.enqueue(tracks);
    }

    pub fn current(&self) -> Option<TrackId> {
        self.current.map(|entry| entry.track)
    }

    pub fn upcoming(&self) -> impl Iterator<Item = TrackId> + '_ {
        self.upcoming.iter().map(|entry| entry.track)
    }

    pub fn upcoming_tracks(&self) -> Vec<TrackId> {
        self.upcoming().collect()
    }

    /// Everything already played, oldest first.
    pub fn history_tracks(&self) -> Vec<TrackId> {
        self.history.iter().map(|entry| entry.track).collect()
    }

    pub fn upcoming_len(&self) -> usize {
        self.upcoming.len()
    }

    /// True when there is nothing playing and nothing left to play.
    pub fn is_empty(&self) -> bool {
        self.current.is_none() && self.upcoming.is_empty()
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
    }

    /// Shuffle only the tracks that are still to come.
    pub fn set_shuffle(&mut self, enabled: bool) {
        if enabled == self.shuffle {
            return;
        }
        self.shuffle = enabled;
        if enabled {
            self.shuffle_upcoming();
        } else {
            // Restore the order the tracks were queued in; dragged order counts
            // as the intended order because it reassigns ranks.
            self.upcoming.sort_by_key(|entry| entry.rank);
        }
    }

    /// Pick a new random order for the upcoming tracks. No-op when shuffle is
    /// off, so a "shuffle again" control cannot silently change the order.
    pub fn reshuffle(&mut self) {
        if self.shuffle {
            self.shuffle_upcoming();
        }
    }

    /// Append a track to the end of the queue.
    pub fn add_to_queue(&mut self, track: TrackId) {
        let rank = self.tail_rank();
        self.upcoming.push(Entry { track, rank });
    }

    /// Append several tracks (an album, a playlist) to the end of the queue.
    pub fn enqueue<I: IntoIterator<Item = TrackId>>(&mut self, tracks: I) {
        let start = self.tail_rank();
        for (offset, track) in tracks.into_iter().enumerate() {
            self.upcoming.push(Entry {
                track,
                rank: start + offset as i64,
            });
        }
    }

    /// Insert a track directly after the one that is playing.
    pub fn play_next(&mut self, track: TrackId) {
        let rank = self.head_rank();
        self.upcoming.insert(0, Entry { track, rank });
    }

    pub fn remove_upcoming(&mut self, index: usize) -> Option<TrackId> {
        if index >= self.upcoming.len() {
            return None;
        }
        let removed = self.upcoming.remove(index);
        self.reassign_ranks();
        Some(removed.track)
    }

    /// Move an upcoming track, as done by dragging a row in the queue panel.
    /// Returns `false` for out-of-range or no-op moves.
    pub fn move_upcoming(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.upcoming.len() || to >= self.upcoming.len() {
            return false;
        }
        let entry = self.upcoming.remove(from);
        self.upcoming.insert(to, entry);
        self.reassign_ranks();
        true
    }

    pub fn clear_upcoming(&mut self) {
        self.upcoming.clear();
    }

    pub fn clear(&mut self) {
        self.current = None;
        self.upcoming.clear();
        self.history.clear();
    }

    /// The track that should play now. With repeat-one the current track is
    /// returned again and the history is left alone.
    pub fn advance(&mut self) -> Option<TrackId> {
        if self.repeat == RepeatMode::One {
            if let Some(current) = self.current {
                return Some(current.track);
            }
        }

        if self.upcoming.is_empty() && self.repeat == RepeatMode::All {
            self.recycle_history();
        }

        let next = self.upcoming.first().copied()?;
        self.upcoming.remove(0);
        if let Some(played) = self.current.replace(next) {
            self.history.push(played);
        }
        Some(next.track)
    }

    /// Go back to the previously played track, putting the current one back at
    /// the front of the queue so "next" returns to it.
    pub fn previous(&mut self) -> Option<TrackId> {
        let previous = self.history.pop()?;
        if let Some(current) = self.current.replace(previous) {
            self.upcoming.insert(0, current);
            self.reassign_ranks();
        }
        Some(previous.track)
    }

    fn tail_rank(&self) -> i64 {
        self.upcoming
            .iter()
            .map(|entry| entry.rank)
            .max()
            .map_or(0, |rank| rank + 1)
    }

    fn head_rank(&self) -> i64 {
        self.upcoming
            .iter()
            .map(|entry| entry.rank)
            .min()
            .map_or(0, |rank| rank - 1)
    }

    /// Keep ranks dense after a drag so "shuffle off" reproduces the order the
    /// user sees right now.
    fn reassign_ranks(&mut self) {
        for (index, entry) in self.upcoming.iter_mut().enumerate() {
            entry.rank = index as i64;
        }
    }

    fn shuffle_upcoming(&mut self) {
        let len = self.upcoming.len();
        for index in (1..len).rev() {
            let swap_with = (self.next_random() % (index as u64 + 1)) as usize;
            self.upcoming.swap(index, swap_with);
        }
    }

    /// Repeat-all: put the played tracks back behind the current one.
    fn recycle_history(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let mut recycled = std::mem::take(&mut self.history);
        recycled.sort_by_key(|entry| entry.rank);
        self.upcoming = recycled;
        self.reassign_ranks();
        if self.shuffle {
            self.shuffle_upcoming();
        }
    }

    /// xorshift64*, so shuffle needs no extra dependency.
    fn next_random(&mut self) -> u64 {
        let mut state = self.rng_state;
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        self.rng_state = state;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(tracks: &[TrackId]) -> Queue {
        Queue::from_tracks_with_seed(tracks.iter().copied(), 0x51_0A)
    }

    #[test]
    fn first_track_plays_immediately() {
        let queue = queued(&[1, 2, 3]);
        assert_eq!(queue.current(), Some(1));
        assert_eq!(queue.upcoming_tracks(), vec![2, 3]);
        assert!(queue.history_tracks().is_empty());
    }

    #[test]
    fn advance_moves_the_played_track_to_history() {
        let mut queue = queued(&[1, 2, 3]);
        assert_eq!(queue.advance(), Some(2));
        assert_eq!(queue.current(), Some(2));
        assert_eq!(queue.history_tracks(), vec![1]);
        assert_eq!(queue.upcoming_tracks(), vec![3]);
    }

    #[test]
    fn queue_stops_at_the_end_with_repeat_off() {
        let mut queue = queued(&[1]);
        assert_eq!(queue.advance(), None);
        assert_eq!(queue.current(), Some(1));
        assert!(!queue.is_empty());
    }

    #[test]
    fn play_next_goes_before_everything_queued() {
        let mut queue = queued(&[1, 2, 3]);
        queue.play_next(9);
        assert_eq!(queue.upcoming_tracks(), vec![9, 2, 3]);

        // A second "play next" jumps ahead of the first one.
        queue.play_next(8);
        assert_eq!(queue.upcoming_tracks(), vec![8, 9, 2, 3]);
    }

    #[test]
    fn add_to_queue_appends() {
        let mut queue = queued(&[1, 2]);
        queue.add_to_queue(9);
        queue.enqueue([7, 6]);
        assert_eq!(queue.upcoming_tracks(), vec![2, 9, 7, 6]);
    }

    #[test]
    fn shuffling_only_touches_upcoming_tracks() {
        let mut queue = queued(&[1, 2, 3, 4, 5, 6, 7, 8]);
        queue.advance();
        queue.advance();
        let current = queue.current();
        let history = queue.history_tracks();

        queue.set_shuffle(true);

        let mut shuffled = queue.upcoming_tracks();
        assert_eq!(queue.current(), current);
        assert_eq!(queue.history_tracks(), history);
        shuffled.sort_unstable();
        assert_eq!(shuffled, vec![4, 5, 6, 7, 8]);
        assert_ne!(queue.upcoming_tracks(), vec![4, 5, 6, 7, 8]);
    }

    #[test]
    fn turning_shuffle_off_restores_the_queued_order() {
        let mut queue = queued(&[1, 2, 3, 4, 5, 6, 7, 8]);
        queue.play_next(99);

        queue.set_shuffle(true);
        assert_ne!(queue.upcoming_tracks(), vec![99, 2, 3, 4, 5, 6, 7, 8]);

        queue.set_shuffle(false);
        assert_eq!(queue.upcoming_tracks(), vec![99, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(queue.current(), Some(1));
    }

    #[test]
    fn dragging_an_upcoming_track_reorders_the_queue() {
        let mut queue = queued(&[1, 2, 3, 4]);
        assert!(queue.move_upcoming(2, 0));
        assert_eq!(queue.upcoming_tracks(), vec![4, 2, 3]);
        assert!(!queue.move_upcoming(0, 0));
        assert!(!queue.move_upcoming(0, 5));
        assert_eq!(queue.upcoming_tracks(), vec![4, 2, 3]);
    }

    #[test]
    fn dragged_order_survives_a_shuffle_round_trip() {
        let mut queue = queued(&[1, 2, 3, 4]);
        queue.move_upcoming(2, 0);
        queue.set_shuffle(true);
        queue.set_shuffle(false);
        assert_eq!(queue.upcoming_tracks(), vec![4, 2, 3]);
    }

    #[test]
    fn removing_an_upcoming_track_keeps_the_rest_in_order() {
        let mut queue = queued(&[1, 2, 3, 4]);
        assert_eq!(queue.remove_upcoming(1), Some(3));
        assert_eq!(queue.remove_upcoming(9), None);
        assert_eq!(queue.upcoming_tracks(), vec![2, 4]);
    }

    #[test]
    fn repeat_one_replays_the_current_track() {
        let mut queue = queued(&[1, 2]);
        queue.set_repeat(RepeatMode::One);
        assert_eq!(queue.advance(), Some(1));
        assert_eq!(queue.advance(), Some(1));
        assert_eq!(queue.current(), Some(1));
        assert!(queue.history_tracks().is_empty());
        assert_eq!(queue.upcoming_tracks(), vec![2]);
    }

    #[test]
    fn repeat_all_recycles_played_tracks() {
        let mut queue = queued(&[1, 2]);
        assert_eq!(queue.advance(), Some(2));
        assert_eq!(queue.advance(), None);

        queue.set_repeat(RepeatMode::All);
        assert_eq!(queue.advance(), Some(1));
        assert_eq!(queue.current(), Some(1));
        assert_eq!(queue.history_tracks(), vec![2]);
        assert_eq!(queue.advance(), Some(2));
    }

    #[test]
    fn previous_returns_the_current_track_to_the_queue() {
        let mut queue = queued(&[1, 2, 3]);
        queue.advance();
        queue.advance();
        assert_eq!(queue.current(), Some(3));

        assert_eq!(queue.previous(), Some(2));
        assert_eq!(queue.current(), Some(2));
        assert_eq!(queue.upcoming_tracks(), vec![3]);
        assert_eq!(queue.previous(), Some(1));
        assert_eq!(queue.previous(), None);
    }

    #[test]
    fn shuffled_playback_plays_every_track_once() {
        let tracks: Vec<TrackId> = (1..=20).collect();
        let mut queue = Queue::from_tracks_with_seed(tracks.iter().copied(), 42);
        queue.set_shuffle(true);

        let mut played = vec![queue.current().expect("current track")];
        while let Some(track) = queue.advance() {
            played.push(track);
        }

        assert_eq!(played.len(), 20);
        let mut sorted = played;
        sorted.sort_unstable();
        assert_eq!(sorted, tracks);
    }

    #[test]
    fn empty_queue_has_nothing_to_play() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
        assert_eq!(queue.advance(), None);
        assert_eq!(queue.previous(), None);
    }

    #[test]
    fn clearing_the_queue_forgets_everything() {
        let mut queue = queued(&[1, 2, 3]);
        queue.advance();
        queue.clear();
        assert!(queue.is_empty());
        assert!(queue.history_tracks().is_empty());

        let mut queue = queued(&[1, 2, 3]);
        queue.clear_upcoming();
        assert_eq!(queue.current(), Some(1));
        assert!(queue.upcoming_tracks().is_empty());
        assert!(queue.advance().is_none());
    }
}
