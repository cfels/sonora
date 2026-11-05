//! Cover art from keyless services.
//!
//! Order matters: MusicBrainz + Cover Art Archive gives the real release cover
//! (the one the album actually shipped with), Deezer is a strong fallback for
//! smaller catalogue entries, and iTunes covers nearly everything else.
//!
//! A provider is only accepted when the bytes really are an image, so an HTML
//! error page is never written to the artwork store as a "cover".

use serde_json::Value;

use crate::http::Http;
use crate::lyrics::TrackQuery;
use crate::{encode, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSource {
    CoverArtArchive,
    Deezer,
    Itunes,
}

impl CoverSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::CoverArtArchive => "Cover Art Archive",
            Self::Deezer => "Deezer",
            Self::Itunes => "iTunes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    pub bytes: Vec<u8>,
    pub source: CoverSource,
    pub url: String,
}

/// Find the best available cover for a track's album.
///
/// Returns `Ok(None)` when every provider came up empty; network failures on an
/// individual provider are treated as "no candidate" so one unreachable service
/// cannot hide a cover the others have.
pub fn find_cover(http: &dyn Http, query: &TrackQuery) -> Result<Option<Cover>> {
    for source in [
        CoverSource::CoverArtArchive,
        CoverSource::Deezer,
        CoverSource::Itunes,
    ] {
        let Some(url) = candidate_url(http, source, query) else {
            continue;
        };
        match http.get(&url) {
            Ok(bytes) if is_image(&bytes) => return Ok(Some(Cover { bytes, source, url })),
            Ok(_) => log::warn!("{} returned something that is not an image", source.label()),
            Err(err) => log::debug!("{} has no cover for this album: {err}", source.label()),
        }
    }
    Ok(None)
}

fn candidate_url(http: &dyn Http, source: CoverSource, query: &TrackQuery) -> Option<String> {
    match source {
        CoverSource::CoverArtArchive => musicbrainz_release_id(http, query)
            .map(|id| format!("https://coverartarchive.org/release/{id}/front-500")),
        CoverSource::Deezer => deezer_cover_url(http, query),
        CoverSource::Itunes => itunes_cover_url(http, query),
    }
}

fn musicbrainz_release_id(http: &dyn Http, query: &TrackQuery) -> Option<String> {
    let album = query.album.as_deref().filter(|album| !album.is_empty())?;
    let url = format!(
        "https://musicbrainz.org/ws/2/release/?query=release:{}%20AND%20artist:{}&fmt=json&limit=1",
        encode(&format!("\"{album}\"")),
        encode(&format!("\"{}\"", query.artist))
    );
    let body = http.get(&url).ok()?;
    parse_musicbrainz_release_id(&body)
}

fn deezer_cover_url(http: &dyn Http, query: &TrackQuery) -> Option<String> {
    let url = format!(
        "https://api.deezer.com/search?q=artist:{}%20track:{}&limit=1",
        encode(&format!("\"{}\"", query.artist)),
        encode(&format!("\"{}\"", query.title))
    );
    let body = http.get(&url).ok()?;
    parse_deezer_cover(&body)
}

fn itunes_cover_url(http: &dyn Http, query: &TrackQuery) -> Option<String> {
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=song&limit=1",
        encode(&format!("{} {}", query.artist, query.title))
    );
    let body = http.get(&url).ok()?;
    parse_itunes_cover(&body)
}

fn parse_musicbrainz_release_id(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .get("releases")?
        .as_array()?
        .first()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

fn parse_deezer_cover(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let album = value.get("data")?.as_array()?.first()?.get("album")?;
    ["cover_xl", "cover_big", "cover_medium"]
        .iter()
        .find_map(|key| album.get(key)?.as_str().filter(|url| !url.is_empty()))
        .map(str::to_string)
}

fn parse_itunes_cover(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let artwork = value
        .get("results")?
        .as_array()?
        .first()?
        .get("artworkUrl100")?
        .as_str()?;
    Some(upgrade_itunes_url(artwork))
}

/// iTunes always returns a 100 px thumbnail; the same path serves bigger sizes.
fn upgrade_itunes_url(url: &str) -> String {
    url.replace("/100x100bb", "/600x600bb")
}

/// Accept only real image payloads.
pub fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || (bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

    struct Stub {
        routes: HashMap<String, Vec<u8>>,
        requested: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(routes: Vec<(&str, Vec<u8>)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(url, body)| (url.to_string(), body))
                    .collect(),
                requested: Mutex::new(Vec::new()),
            }
        }

        fn json(routes: Vec<(&str, &str)>) -> Self {
            Self::new(
                routes
                    .into_iter()
                    .map(|(url, body)| (url, body.as_bytes().to_vec()))
                    .collect(),
            )
        }
    }
    impl Http for Stub {
        fn get(&self, url: &str) -> std::result::Result<Vec<u8>, crate::HttpError> {
            self.requested.lock().unwrap().push(url.to_string());
            self.routes
                .get(url)
                .cloned()
                .ok_or(crate::HttpError::Status(404))
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
    fn reads_a_release_id_from_musicbrainz() {
        let body =
            r#"{"releases":[{"id":"0f2c1b6e-0000-0000-0000-000000000000","title":"SAW 85-92"}]}"#;
        assert_eq!(
            parse_musicbrainz_release_id(body.as_bytes()).as_deref(),
            Some("0f2c1b6e-0000-0000-0000-000000000000")
        );
        assert_eq!(parse_musicbrainz_release_id(b"{}"), None);
    }

    #[test]
    fn prefers_the_largest_deezer_cover() {
        let body = r#"{"data":[{"album":{"cover_medium":"m.jpg","cover_big":"b.jpg","cover_xl":"xl.jpg"}}]}"#;
        assert_eq!(
            parse_deezer_cover(body.as_bytes()).as_deref(),
            Some("xl.jpg")
        );
    }

    #[test]
    fn upgrades_itunes_thumbnails_to_six_hundred_pixels() {
        let body =
            r#"{"results":[{"artworkUrl100":"https://is1.mzstatic.com/image/100x100bb.jpg"}]}"#;
        assert_eq!(
            parse_itunes_cover(body.as_bytes()).as_deref(),
            Some("https://is1.mzstatic.com/image/600x600bb.jpg")
        );
    }

    #[test]
    fn musicbrainz_plus_cover_art_archive_wins() {
        let mbid = "0f2c1b6e-0000-0000-0000-000000000000";
        let mb_url = format!(
            "https://musicbrainz.org/ws/2/release/?query=release:{}%20AND%20artist:{}&fmt=json&limit=1",
            encode("\"Selected Ambient Works 85-92\""),
            encode("\"Aphex Twin\"")
        );
        let http = Stub::new(vec![
            (
                &mb_url.clone(),
                format!("{{\"releases\":[{{\"id\":\"{mbid}\"}}]}}").into_bytes(),
            ),
            (
                &format!("https://coverartarchive.org/release/{mbid}/front-500"),
                PNG.to_vec(),
            ),
        ]);

        let cover = find_cover(&http, &query()).unwrap().expect("cover");
        assert_eq!(cover.source, CoverSource::CoverArtArchive);
        assert_eq!(cover.bytes, PNG);
        let requested = http.requested.lock().unwrap().clone();
        assert_eq!(requested.len(), 2, "{requested:?}");
    }

    #[test]
    fn falls_through_to_deezer_when_musicbrainz_has_no_release() {
        let deezer_url = format!(
            "https://api.deezer.com/search?q=artist:{}%20track:{}&limit=1",
            encode("\"Aphex Twin\""),
            encode("\"Xtal\"")
        );
        let http = Stub::json(vec![(
            &deezer_url,
            r#"{"data":[{"album":{"cover_xl":"https://cdn.deezer.com/cover.jpg"}}]}"#,
        )]);
        // The CDN image is a PNG so the payload check passes.
        let mut stub = http;
        stub.routes
            .insert("https://cdn.deezer.com/cover.jpg".to_string(), PNG.to_vec());

        let cover = find_cover(&stub, &query()).unwrap().expect("cover");
        assert_eq!(cover.source, CoverSource::Deezer);
        assert_eq!(cover.url, "https://cdn.deezer.com/cover.jpg");
    }

    #[test]
    fn an_html_error_page_is_never_treated_as_a_cover() {
        let cover_url = "https://cdn.deezer.com/cover.jpg";
        let deezer_url = format!(
            "https://api.deezer.com/search?q=artist:{}%20track:{}&limit=1",
            encode("\"Aphex Twin\""),
            encode("\"Xtal\"")
        );
        let mut stub = Stub::json(vec![(
            &deezer_url,
            &format!("{{\"data\":[{{\"album\":{{\"cover_xl\":\"{cover_url}\"}}}}]}}"),
        )]);
        stub.routes
            .insert(cover_url.to_string(), b"<html>rate limited</html>".to_vec());

        assert_eq!(find_cover(&stub, &query()).unwrap(), None);
    }

    #[test]
    fn recognises_image_magic_bytes() {
        assert!(is_image(PNG));
        assert!(is_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_image(b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!is_image(b"<!doctype html>"));
        assert!(!is_image(b""));
    }
}
