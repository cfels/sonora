//! Tag reading with `lofty`.
//!
//! Sonora treats tags as the source of truth and only falls back to file names
//! for the fields a file genuinely lacks. Nothing here writes to the file; tag
//! editing lives in `sonora-metadata` (phase 3).

use std::path::Path;
use std::time::UNIX_EPOCH;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use sonora_core::{
    parse_artist_title, without_extension, AudioFormat, Error, Result, ScannedTrack,
};

/// Read a file's tags into a [`ScannedTrack`].
pub fn extract(path: &Path) -> Result<ScannedTrack> {
    let metadata = std::fs::metadata(path).map_err(|err| Error::io(path, err))?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);

    let tagged = Probe::open(path)
        .map_err(|err| Error::metadata(path, err.to_string()))?
        .read()
        .map_err(|err| Error::metadata(path, err.to_string()))?;

    let properties = tagged.properties();
    let duration_ms = properties.duration().as_millis() as u64;
    let codec = codec_label(path, tagged.file_type());

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let mut title = tag
        .and_then(|tag| tag.title())
        .map(|value| value.to_string());
    let artist = tag
        .and_then(|tag| tag.artist())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let album = tag
        .and_then(|tag| tag.album())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let album_artist = tag
        .and_then(|tag| tag.get_string(ItemKey::AlbumArtist))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    // lofty 0.25 exposes the release date as a `Timestamp`; the bare year lives
    // inside it, so "1999", "1999-04" and "1999-04-12" all end up as 1999.
    let year = tag
        .and_then(|tag| tag.date())
        .map(|date| date.year as i32)
        .filter(|year| (1000..=3000).contains(year));
    let track_no = tag.and_then(|tag| tag.track());
    let disc_no = tag.and_then(|tag| tag.disk());
    let genres = tag
        .and_then(|tag| tag.genre())
        .map(|value| split_genres(&value))
        .unwrap_or_default();

    if title
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        title = None;
    }

    // Filename fallback. We only invent an artist when the file name itself
    // states one; otherwise the track keeps "Unknown Artist".
    let stem = without_extension(path);
    let (guessed_artist, guessed_title) = parse_artist_title(&stem);
    let title = title.unwrap_or(guessed_title);
    let artist = artist.or(guessed_artist);

    Ok(ScannedTrack {
        path: path.to_path_buf(),
        title,
        artist,
        album_artist,
        album,
        genres,
        track_no,
        disc_no,
        year,
        duration_ms,
        bitrate_kbps: properties.audio_bitrate(),
        sample_rate: properties.sample_rate(),
        channels: properties.channels(),
        codec,
        file_size: metadata.len(),
        modified_at,
    })
}

/// Prefer our own format label (stable, user-facing) over lofty's debug name.
fn codec_label(path: &Path, file_type: lofty::file::FileType) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(AudioFormat::from_extension)
        .map(|format| format.label().to_string())
        .unwrap_or_else(|| format!("{file_type:?}"))
}

/// Tags often pack several genres into one string ("Rock; Pop").
pub fn split_genres(value: &str) -> Vec<String> {
    value
        .split([';', '/', ','])
        .map(|genre| genre.trim())
        .filter(|genre| !genre.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split_genres;

    #[test]
    fn splits_multi_value_genres() {
        assert_eq!(split_genres("Rock; Pop"), vec!["Rock", "Pop"]);
        assert_eq!(split_genres("Ambient / Drone"), vec!["Ambient", "Drone"]);
        assert_eq!(split_genres("  "), Vec::<String>::new());
    }
}
