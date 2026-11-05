//! Last.fm tags — the only provider that needs a key.
//!
//! The key belongs to the listener, so the client is inert without one and the
//! rest of the metadata pipeline keeps working.

use serde_json::Value;

use crate::http::Http;
use crate::{encode, MetadataError, Result};

pub struct LastFm {
    api_key: String,
}

impl LastFm {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    pub fn top_tags_url(&self, artist: &str, track: &str) -> String {
        format!(
            "https://ws.audioscrobbler.com/2.0/?method=track.gettoptags&artist={}&track={}&api_key={}&format=json",
            encode(artist),
            encode(track),
            encode(self.api_key.trim())
        )
    }

    /// Listener-assigned tags for a track, strongest first.
    pub fn track_tags(&self, http: &dyn Http, artist: &str, track: &str) -> Result<Vec<String>> {
        if !self.has_key() {
            return Err(MetadataError::MissingApiKey);
        }
        let body = http.get(&self.top_tags_url(artist, track))?;
        parse_tags(&body)
    }
}

fn parse_tags(body: &[u8]) -> Result<Vec<String>> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| MetadataError::Parse(format!("Last.fm response: {err}")))?;

    // Last.fm returns `{"toptags":{"tag":[]}}` for a track nobody tagged, and
    // sometimes an object instead of an array for a single tag.
    let Some(tags) = value
        .get("toptags")
        .and_then(|top| top.get("tag"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };

    Ok(tags
        .iter()
        .filter_map(|tag| tag.get("name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Stub(HashMap<String, Vec<u8>>);

    impl Http for Stub {
        fn get(&self, url: &str) -> std::result::Result<Vec<u8>, crate::HttpError> {
            self.0
                .get(url)
                .cloned()
                .ok_or(crate::HttpError::Status(404))
        }
    }

    #[test]
    fn builds_a_keyed_request() {
        let client = LastFm::new("abc123");
        let url = client.top_tags_url("Aphex Twin", "Xtal");
        assert!(url.contains("method=track.gettoptags"));
        assert!(url.contains("artist=Aphex%20Twin"));
        assert!(url.contains("api_key=abc123"));
        assert!(url.contains("format=json"));
        assert!(client.has_key());
        assert!(!LastFm::new("  ").has_key());
    }

    #[test]
    fn reads_tags_in_order() {
        let body = r#"{"toptags":{"tag":[{"name":"electronic","count":100},{"name":"ambient","count":70}]}}"#;
        assert_eq!(
            parse_tags(body.as_bytes()).unwrap(),
            vec!["electronic".to_string(), "ambient".to_string()]
        );
    }

    #[test]
    fn an_untagged_track_yields_no_tags() {
        let body = r#"{"toptags":{"tag":[]}}"#;
        assert!(parse_tags(body.as_bytes()).unwrap().is_empty());
        assert!(parse_tags(b"{}").unwrap().is_empty());
    }

    #[test]
    fn without_a_key_the_client_refuses_instead_of_calling_out() {
        let client = LastFm::new("");
        let error = client
            .track_tags(&Stub(HashMap::new()), "Aphex Twin", "Xtal")
            .unwrap_err();
        assert!(matches!(error, MetadataError::MissingApiKey));
    }

    #[test]
    fn a_keyed_lookup_returns_the_parsed_tags() {
        let client = LastFm::new("abc123");
        let url = client.top_tags_url("Aphex Twin", "Xtal");
        let stub = Stub(HashMap::from([(
            url,
            br#"{"toptags":{"tag":[{"name":"idm"}]}}"#.to_vec(),
        )]));
        assert_eq!(
            client.track_tags(&stub, "Aphex Twin", "Xtal").unwrap(),
            vec!["idm".to_string()]
        );
    }
}
