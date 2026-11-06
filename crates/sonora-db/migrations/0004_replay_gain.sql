-- ReplayGain values read from tags at scan time.
--
-- ReplayGain is stored as text on the tag (Vorbis comments such as
-- REPLAYGAIN_TRACK_GAIN, or ID3 TXXX frames). lofty exposes them through the
-- generic ItemKey API, so we read them during scanning and persist them here.
--
-- Both the track and album gain are kept: playback prefers the track value and
-- falls back to the album value when a file only carries album gain. Peaks are
-- stored for future normalisation but are not applied to the volume today.
--
-- The chosen playback device is not stored per track: it is a single user
-- setting (`Settings::playback_device`) applied to the whole engine, so it lives
-- in the settings document rather than in this table.

ALTER TABLE tracks ADD COLUMN replay_gain_track_db   REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_album_db   REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_track_peak REAL;
ALTER TABLE tracks ADD COLUMN replay_gain_album_peak REAL;
