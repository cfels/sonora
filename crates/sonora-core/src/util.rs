//! Small, well-tested helpers shared across crates.

use std::path::Path;

/// `225_000` → `"3:45"`, `3_723_000` → `"1:02:03"`.
pub fn format_duration(ms: i64) -> String {
    if ms <= 0 {
        return "0:00".to_string();
    }
    let total_seconds = ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Case- and punctuation-insensitive key used for search ranking and duplicate
/// detection. Accents are preserved intentionally: diacritics are handled by
/// the FTS5 tokenizer, and stripping them here would merge distinct names.
pub fn normalize_key(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sort key that moves leading articles out of the way ("The Beatles" → "beatles").
pub fn sort_key(value: &str) -> String {
    let normalized = normalize_key(value);
    for article in ["the ", "a ", "an "] {
        if let Some(rest) = normalized.strip_prefix(article) {
            return rest.to_string();
        }
    }
    normalized
}

/// Strip a trailing audio extension: `"03 - Song.flac"` → `"03 - Song"`.
pub fn without_extension(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Best-effort recovery of artist/title from file names such as
/// `"Artist - 01 - Title"` or `"01. Title"`. Used only when a file carries no
/// usable tags — we never overwrite real tags with a guess.
pub fn parse_artist_title(stem: &str) -> (Option<String>, String) {
    let cleaned = strip_track_prefix(stem.trim());

    // "Artist - Title" (the most common convention), but only when the parts
    // are non-empty; " - " inside a title is left alone when the prefix is
    // clearly a track number.
    if let Some((left, right)) = cleaned.split_once(" - ") {
        let left = left.trim();
        let right = strip_track_prefix(right.trim());
        if !left.is_empty() && !right.is_empty() && !is_numeric(left) {
            return (Some(left.to_string()), right);
        }
    }

    (None, cleaned)
}

fn strip_track_prefix(value: &str) -> String {
    let value = value.trim_start();
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return value.replace('_', " ").trim().to_string();
    }
    let rest = &value[digits.len()..];
    let rest = rest.trim_start_matches(['.', '-', ')', ']', ' ']).trim();
    if rest.is_empty() {
        value.replace('_', " ").trim().to_string()
    } else {
        rest.replace('_', " ").trim().to_string()
    }
}

fn is_numeric(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(59_000), "0:59");
        assert_eq!(format_duration(225_000), "3:45");
        assert_eq!(format_duration(3_723_000), "1:02:03");
    }

    #[test]
    fn extracts_artist_and_title() {
        assert_eq!(
            parse_artist_title("Aphex Twin - Xtal"),
            (Some("Aphex Twin".to_string()), "Xtal".to_string())
        );
        assert_eq!(
            parse_artist_title("04 - Windowlicker"),
            (None, "Windowlicker".to_string())
        );
        assert_eq!(
            parse_artist_title("Boards of Canada - 05 - Roygbiv"),
            (Some("Boards of Canada".to_string()), "Roygbiv".to_string())
        );
        assert_eq!(
            parse_artist_title("untitled"),
            (None, "untitled".to_string())
        );
    }

    #[test]
    fn keeps_titles_with_dashes_when_prefix_is_a_number() {
        assert_eq!(
            parse_artist_title("07 - 8 - 9"),
            (None, "8 - 9".to_string())
        );
    }

    #[test]
    fn sorts_around_articles() {
        assert_eq!(sort_key("The Beatles"), "beatles");
        assert_eq!(sort_key("A Tribe Called Quest"), "tribe called quest");
        assert_eq!(sort_key("Björk"), "björk");
    }
}
