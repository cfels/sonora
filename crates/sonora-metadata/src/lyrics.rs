//! Lyrics from LRCLIB.
//!
//! LRCLIB exposes an exact lookup (`/api/get`) and a fuzzy search
//! (`/api/search`). The exact lookup is tried first because it matches on album
//! and duration; the search is the fallback that still finds a song when the
//! file's tags disagree with the database.

use serde_json::Value;

use crate::http::{Http, HttpError};
use crate::{encode, MetadataError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackQuery {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
}

impl TrackQuery {
    pub fn new(artist: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            album: None,
            duration_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsSource {
    Lrclib,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lyrics {
    /// Plain text, one line per lyric line.
    pub plain: Option<String>,
    /// LRC file with timestamps, when the service has one.
    pub synced: Option<String>,
    pub source: LyricsSource,
}

impl Lyrics {
    /// The text to show when the view cannot follow timestamps.
    pub fn display_text(&self) -> Option<&str> {
        self.plain.as_deref().or(self.synced.as_deref())
    }
}

/// Exact lookup URL. LRCLIB wants the duration in whole seconds.
pub fn get_url(query: &TrackQuery) -> String {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        encode(&query.artist),
        encode(&query.title)
    );
    if let Some(album) = query.album.as_deref().filter(|album| !album.is_empty()) {
        url.push_str(&format!("&album_name={}", encode(album)));
    }
    if let Some(duration) = query.duration_ms {
        url.push_str(&format!("&duration={}", duration / 1000));
    }
    url
}

pub fn search_url(query: &TrackQuery) -> String {
    format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        encode(&query.artist),
        encode(&query.title)
    )
}

/// Look up lyrics, preferring the exact match.
///
/// A 404 from `/api/get` means "not in the database by that exact name", which
/// is the normal path into the search fallback; other failures are reported so
/// a caller can tell "no lyrics" apart from "the network is down".
pub fn fetch_lyrics(http: &dyn Http, query: &TrackQuery) -> Result<Option<Lyrics>> {
    match http.get(&get_url(query)) {
        Ok(body) => {
            if let Some(lyrics) = parse_get(&body)? {
                return Ok(Some(lyrics));
            }
        }
        Err(HttpError::Status(404)) => {}
        Err(err) => return Err(err.into()),
    }

    match http.get(&search_url(query)) {
        Ok(body) => parse_search(&body, query.duration_ms),
        Err(HttpError::Status(404)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn parse_get(body: &[u8]) -> Result<Option<Lyrics>> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| MetadataError::Parse(format!("LRCLIB response: {err}")))?;
    Ok(lyrics_from_object(&value))
}

/// Pick the search hit whose duration is closest to the file, so a live version
/// does not win over the studio recording.
fn parse_search(body: &[u8], duration_ms: Option<u64>) -> Result<Option<Lyrics>> {
    let values: Vec<Value> = serde_json::from_slice(body)
        .map_err(|err| MetadataError::Parse(format!("LRCLIB search response: {err}")))?;

    let mut best: Option<(u64, Lyrics)> = None;
    for value in values {
        let Some(lyrics) = lyrics_from_object(&value) else {
            continue;
        };
        let difference = match (duration_ms, value.get("duration").and_then(Value::as_f64)) {
            (Some(wanted), Some(found)) => (wanted as f64 - found * 1000.0).abs() as u64,
            _ => 0,
        };
        if difference > MAX_DURATION_DRIFT_MS {
            continue;
        }
        if best
            .as_ref()
            .map(|(closest, _)| difference < *closest)
            .unwrap_or(true)
        {
            best = Some((difference, lyrics));
        }
    }
    Ok(best.map(|(_, lyrics)| lyrics))
}

/// A different recording can run a few seconds longer; anything beyond this is a
/// different song.
const MAX_DURATION_DRIFT_MS: u64 = 5_000;

fn lyrics_from_object(value: &Value) -> Option<Lyrics> {
    let plain = text_field(value, "plainLyrics");
    let synced = text_field(value, "syncedLyrics");
    if plain.is_none() && synced.is_none() {
        return None;
    }
    Some(Lyrics {
        plain,
        synced,
        source: LyricsSource::Lrclib,
    })
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Stub {
        routes: HashMap<String, Vec<u8>>,
        requested: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(routes: Vec<(&str, &str)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(url, body)| (url.to_string(), body.as_bytes().to_vec()))
                    .collect(),
                requested: Mutex::new(Vec::new()),
            }
        }
    }
    impl Http for Stub {
        fn get(&self, url: &str) -> std::result::Result<Vec<u8>, HttpError> {
            self.requested.lock().unwrap().push(url.to_string());
            self.routes.get(url).cloned().ok_or(HttpError::Status(404))
        }
    }

    fn query() -> TrackQuery {
        TrackQuery {
            artist: "Aphex Twin".to_string(),
            title: "Xtal".to_string(),
            album: Some("Selected Ambient Works 85-92".to_string()),
            duration_ms: Some(293_000),
        }
    }

    #[test]
    fn exact_lookup_includes_album_and_whole_seconds() {
        let url = get_url(&query());
        assert!(url.contains("artist_name=Aphex%20Twin"), "{url}");
        assert!(url.contains("track_name=Xtal"), "{url}");
        assert!(
            url.contains("album_name=Selected%20Ambient%20Works%2085-92"),
            "{url}"
        );
        assert!(url.ends_with("&duration=293"), "{url}");
    }

    #[test]
    fn exact_lookup_omits_missing_fields() {
        let url = get_url(&TrackQuery::new("Burial", "Archangel"));
        assert!(!url.contains("album_name"), "{url}");
        assert!(!url.contains("duration"), "{url}");
    }

    #[test]
    fn parses_plain_and_synced_lyrics() {
        let body =
            r#"{"id":1,"plainLyrics":"line one\nline two","syncedLyrics":"[00:01.00] line one"}"#;
        let lyrics = parse_get(body.as_bytes()).unwrap().expect("lyrics");
        assert_eq!(lyrics.plain.as_deref(), Some("line one\nline two"));
        assert_eq!(lyrics.display_text(), Some("line one\nline two"));
        assert!(lyrics.synced.unwrap().starts_with("[00:01.00]"));
    }

    #[test]
    fn an_instrumental_entry_is_not_lyrics() {
        let body = r#"{"id":1,"plainLyrics":"","syncedLyrics":null,"instrumental":true}"#;
        assert_eq!(parse_get(body.as_bytes()).unwrap(), None);
    }

    #[test]
    fn search_prefers_the_hit_closest_in_length() {
        let body = r#"[
            {"duration": 320.0, "plainLyrics": "live version"},
            {"duration": 293.5, "plainLyrics": "studio version"},
            {"duration": 293.0, "syncedLyrics": "[00:01.00] studio synced"}
        ]"#;
        let lyrics = parse_search(body.as_bytes(), Some(293_000))
            .unwrap()
            .expect("a hit");
        assert_eq!(lyrics.plain, None);
        assert_eq!(lyrics.synced.as_deref(), Some("[00:01.00] studio synced"));
    }

    #[test]
    fn search_ignores_hits_that_are_a_different_recording() {
        let body = r#"[{"duration": 480.0, "plainLyrics": "some other song"}]"#;
        assert_eq!(parse_search(body.as_bytes(), Some(200_000)).unwrap(), None);
    }

    #[test]
    fn exact_lookup_falls_back_to_search_on_404() {
        let search = search_url(&query());
        let http = Stub::new(vec![(
            &search,
            r#"[{"duration": 293.0, "plainLyrics": "fallback text"}]"#,
        )]);
        let lyrics = fetch_lyrics(&http, &query()).unwrap().expect("lyrics");
        assert_eq!(lyrics.plain.as_deref(), Some("fallback text"));

        let requested = http.requested.lock().unwrap().clone();
        assert_eq!(requested.len(), 2, "{requested:?}");
        assert_eq!(requested[0], get_url(&query()));
        assert_eq!(requested[1], search);
    }

    #[test]
    fn a_transport_error_is_reported_instead_of_looking_like_missing_lyrics() {
        struct Down;
        impl Http for Down {
            fn get(&self, _url: &str) -> std::result::Result<Vec<u8>, HttpError> {
                Err(HttpError::Transport("dns failure".to_string()))
            }
        }
        let error = fetch_lyrics(&Down, &query()).unwrap_err();
        assert!(error.to_string().contains("dns failure"), "{error}");
    }
}
