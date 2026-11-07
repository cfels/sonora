-- Smart playlists.
--
-- A playlist whose `rules` column is set derives its tracks from those rules
-- instead of from `playlist_tracks`, so its membership follows the library
-- (rate a track, delete a file) without anyone editing the playlist.
--
-- `rules` is a JSON document owned by `sonora_core::PlaylistRules`; NULL means
-- an ordinary playlist whose entries the listener arranged by hand. The two
-- kinds share a row because they share a name, an id and a place on the
-- Playlists page; only the source of the tracks differs.

ALTER TABLE playlists ADD COLUMN rules TEXT;
