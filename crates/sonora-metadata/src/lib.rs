//! Keyless metadata services.
//!
//! Sonora's library works without an account and without an API key, so the
//! default providers are the ones that accept anonymous requests:
//!
//! * **MusicBrainz** for release identity (which then points at Cover Art Archive)
//! * **Cover Art Archive** for release covers
//! * **Deezer** and **iTunes** as keyless artwork fallbacks
//! * **LRCLIB** for synced and plain lyrics
//!
//! Last.fm is the one service that needs a key, and it is purely additive: it
//! supplies listener tags on top of the keyless data, and everything still works
//! when no key is configured.
//!
//! Every provider is reached through the [`Http`] trait so the parsing and the
//! fallback order can be tested against canned responses instead of the network.

pub mod artwork;
pub mod cache;
pub mod http;
pub mod lastfm;
pub mod lyrics;

pub use artwork::{find_cover, Cover, CoverSource};
pub use cache::DiskCache;
pub use http::{Http, HttpError, UreqHttp};
pub use lastfm::LastFm;
pub use lyrics::{fetch_lyrics, Lyrics, LyricsSource, TrackQuery};

use thiserror::Error;

/// User agent sent to every service. MusicBrainz and LRCLIB both reject
/// requests without one.
pub const USER_AGENT: &str = concat!(
    "Sonora/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/takyon303/sonora )"
);

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("the service answered with something Sonora could not read: {0}")]
    Parse(String),
    #[error("network request failed: {0}")]
    Http(#[from] HttpError),
    #[error("Last.fm needs an API key; add one in Settings to enable it")]
    MissingApiKey,
}

pub type Result<T> = std::result::Result<T, MetadataError>;

/// Percent-encode a value for a query string.
///
/// Every provider takes plain words as search terms, so the safe set is the
/// unreserved one plus spaces turned into `%20`; everything else is escaped.
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_escapes_spaces_and_reserved_characters() {
        assert_eq!(encode("Boards of Canada"), "Boards%20of%20Canada");
        assert_eq!(encode("A&B/C?"), "A%26B%2FC%3F");
        assert_eq!(encode("Äpfel"), "%C3%84pfel");
    }
}
