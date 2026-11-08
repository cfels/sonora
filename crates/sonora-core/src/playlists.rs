//! Rules for a smart playlist.
//!
//! A smart playlist stores the rules that decide its tracks instead of the
//! tracks themselves, so its membership follows the library: rate a track and
//! it can appear, delete a file and it disappears. The rules are a small
//! document rather than free-form SQL, so the database layer can only ever
//! build a query from the fields below and the UI can round-trip exactly what
//! the listener picked.
//!
//! Nothing here is trusted: an editor can produce a document with a rating of
//! 60, a blank filter, an inverted year range or a zero limit, so every path
//! that stores or runs rules goes through [`PlaylistRules::normalized`] first.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// How many tracks a smart playlist returns when the listener sets no limit.
pub const SMART_LIMIT_DEFAULT: i64 = 200;

/// The most a smart playlist will ever return, so one playlist cannot be asked
/// to materialise the entire library.
pub const SMART_LIMIT_MAX: i64 = 2000;

/// The rules behind a smart playlist.
///
/// `Default` is a valid playlist — an empty filter is "everything" — which is
/// why a new smart playlist is usable before the listener edits anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistRules {
    /// Matched against the track title, the track artist and the album title.
    pub text: String,
    /// The lowest star rating to include; `0` means any rating at all.
    pub min_rating: u8,
    /// Include only favourites.
    pub favourites_only: bool,
    /// Earliest release year to include, inclusive.
    pub year_from: Option<i64>,
    /// Latest release year to include, inclusive.
    pub year_to: Option<i64>,
    /// How many tracks to return, newest albums first when the limit bites.
    pub limit: i64,
}

impl Default for PlaylistRules {
    fn default() -> Self {
        Self {
            text: String::new(),
            min_rating: 0,
            favourites_only: false,
            year_from: None,
            year_to: None,
            limit: SMART_LIMIT_DEFAULT,
        }
    }
}

impl PlaylistRules {
    /// Clamp and tidy the document so it can be stored and run safely.
    ///
    /// This never fails: a rule set that is out of range is corrected rather
    /// than rejected, because refusing to save would leave the editor holding
    /// something the library cannot show.
    pub fn normalized(mut self) -> Self {
        self.text = self.text.trim().to_string();
        self.min_rating = self.min_rating.min(5);
        self.year_from = self.year_from.filter(|year| (1000..=9999).contains(year));
        self.year_to = self.year_to.filter(|year| (1000..=9999).contains(year));
        if let (Some(from), Some(to)) = (self.year_from, self.year_to) {
            if from > to {
                self.year_from = Some(to);
                self.year_to = Some(from);
            }
        }
        if self.limit <= 0 {
            self.limit = SMART_LIMIT_DEFAULT;
        }
        self.limit = self.limit.min(SMART_LIMIT_MAX);
        self
    }

    /// Whether the rules narrow the library at all. A "smart" playlist with no
    /// constraints is really "everything", which is allowed but worth saying
    /// out loud in the UI.
    pub fn is_unfiltered(&self) -> bool {
        self.text.is_empty()
            && self.min_rating == 0
            && !self.favourites_only
            && self.year_from.is_none()
            && self.year_to.is_none()
    }

    /// Serialise the document for storage. The result is already normalized, so
    /// what the database reads back is what a query will actually run.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.clone().normalized())
            .map_err(|err| Error::Other(format!("playlist rules cannot be stored: {err}")))
    }

    /// Read a stored document. A malformed one is an error rather than silently
    /// becoming "everything", because a smart playlist that quietly widened to
    /// the whole library would be worse than a visible failure.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str::<Self>(json)
            .map(Self::normalized)
            .map_err(|err| Error::Other(format!("playlist rules cannot be read: {err}")))
    }

    /// One line describing what the rules pick, for the playlist header.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.text.is_empty() {
            parts.push(format!("\"{}\"", self.text));
        }
        if self.favourites_only {
            parts.push("favourites".to_string());
        }
        if self.min_rating > 0 {
            parts.push(format!("{} stars and up", self.min_rating));
        }
        match (self.year_from, self.year_to) {
            (Some(from), Some(to)) if from == to => parts.push(format!("from {from}")),
            (Some(from), Some(to)) => parts.push(format!("{from}\u{2013}{to}")),
            (Some(from), None) => parts.push(format!("{from} and later")),
            (None, Some(to)) => parts.push(format!("up to {to}")),
            (None, None) => {}
        }
        if parts.is_empty() {
            "Everything in the library".to_string()
        } else {
            parts.join(" \u{00b7} ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_usable_unfiltered_playlist() {
        let rules = PlaylistRules::default();
        assert_eq!(rules.limit, SMART_LIMIT_DEFAULT);
        assert!(rules.is_unfiltered());
        assert_eq!(rules.describe(), "Everything in the library");
        assert_eq!(rules.clone().normalized(), rules);
    }

    #[test]
    fn normalization_clamps_ratings_years_and_limits() {
        let rules = PlaylistRules {
            text: "  nina simone  ".into(),
            min_rating: 60,
            favourites_only: true,
            year_from: Some(12),
            year_to: Some(1965),
            limit: 0,
        }
        .normalized();
        assert_eq!(rules.text, "nina simone");
        assert_eq!(rules.min_rating, 5);
        assert_eq!(rules.year_from, None, "a year below 1000 is dropped");
        assert_eq!(rules.year_to, Some(1965));
        assert_eq!(rules.limit, SMART_LIMIT_DEFAULT);

        let huge = PlaylistRules {
            limit: SMART_LIMIT_MAX * 10,
            ..PlaylistRules::default()
        }
        .normalized();
        assert_eq!(huge.limit, SMART_LIMIT_MAX);
    }

    #[test]
    fn an_inverted_year_range_is_put_back_in_order() {
        let rules = PlaylistRules {
            year_from: Some(1999),
            year_to: Some(1980),
            ..PlaylistRules::default()
        }
        .normalized();
        assert_eq!((rules.year_from, rules.year_to), (Some(1980), Some(1999)));
    }

    #[test]
    fn a_description_names_only_the_filters_in_play() {
        let rules = PlaylistRules {
            text: "blue".into(),
            min_rating: 4,
            favourites_only: true,
            year_from: Some(1971),
            year_to: Some(1975),
            limit: 50,
        };
        let description = rules.describe();
        assert!(description.contains("\"blue\""), "{description}");
        assert!(description.contains("favourites"), "{description}");
        assert!(description.contains("4 stars and up"), "{description}");
        assert!(description.contains("1971\u{2013}1975"), "{description}");

        let open_ended = PlaylistRules {
            year_from: Some(2001),
            ..PlaylistRules::default()
        };
        assert!(open_ended.describe().contains("2001 and later"));

        let single_year = PlaylistRules {
            year_from: Some(1969),
            year_to: Some(1969),
            ..PlaylistRules::default()
        };
        assert!(single_year.describe().contains("from 1969"));
    }

    #[test]
    fn rules_round_trip_through_json() {
        let rules = PlaylistRules {
            text: "live".into(),
            min_rating: 2,
            favourites_only: false,
            year_from: Some(1990),
            year_to: None,
            limit: 25,
        };
        let json = rules.to_json().expect("rules serialise");
        let back = PlaylistRules::from_json(&json).expect("rules deserialise");
        assert_eq!(back, rules);
        // An older or partial document still loads and keeps the defaults.
        let partial = PlaylistRules::from_json(r#"{"min_rating":3}"#).expect("partial rules load");
        assert_eq!(partial.min_rating, 3);
        assert_eq!(partial.limit, SMART_LIMIT_DEFAULT);
        // A document we cannot read is an error, not an empty filter.
        assert!(PlaylistRules::from_json("{ nope").is_err());
        // Storing normalizes, so 9 stars on disk comes back as 5.
        let stored = PlaylistRules {
            min_rating: 9,
            ..PlaylistRules::default()
        }
        .to_json()
        .expect("clamped rules serialise");
        assert_eq!(PlaylistRules::from_json(&stored).unwrap().min_rating, 5);
    }
}
