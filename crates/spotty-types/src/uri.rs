//! Spotify URI and URL parsing and canonical serialization types.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;
use crate::error::SpottyError;

/// Entity kind represented by a Spotify URI or web URL.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SpotifyEntityKind {
    /// Individual audio track (`spotify:track:<id>`).
    Track,
    /// Podcast episode (`spotify:episode:<id>`).
    Episode,
    /// Music album (`spotify:album:<id>`).
    Album,
    /// Artist profile (`spotify:artist:<id>`).
    Artist,
    /// User or editorial playlist (`spotify:playlist:<id>`).
    Playlist,
    /// Podcast show (`spotify:show:<id>`).
    Show,
    /// Spotify user profile (`spotify:user:<id>`).
    User,
}

impl SpotifyEntityKind {
    /// Returns the lowercase canonical Spotify type segment.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Episode => "episode",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Show => "show",
            Self::User => "user",
        }
    }
}

impl fmt::Display for SpotifyEntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SpotifyEntityKind {
    type Err = SpottyError;

    fn from_str(raw: &str) -> Result<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "track" => Ok(Self::Track),
            "episode" => Ok(Self::Episode),
            "album" => Ok(Self::Album),
            "artist" => Ok(Self::Artist),
            "playlist" => Ok(Self::Playlist),
            "show" => Ok(Self::Show),
            "user" => Ok(Self::User),
            _ => Err(SpottyError::InvalidUri(format!(
                "unsupported Spotify entity kind: {raw}"
            ))),
        }
    }
}

/// Canonical Spotify resource identifier parsed from URIs or `open.spotify.com`
/// URLs.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpotifyUri {
    kind: SpotifyEntityKind,
    id: String,
}

impl SpotifyUri {
    /// Constructs a validated [`SpotifyUri`] from a [`SpotifyEntityKind`] and
    /// resource ID.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `id` is empty or contains invalid
    /// characters for the given `kind`.
    pub fn new(kind: SpotifyEntityKind, id: impl Into<String>) -> Result<Self> {
        let id_str = id.into();
        if !is_valid_entity_id(kind, &id_str) {
            return Err(SpottyError::InvalidUri(format!(
                "invalid {kind} identifier: {id_str}"
            )));
        }
        Ok(Self { kind, id: id_str })
    }

    /// Returns the parsed [`SpotifyEntityKind`].
    #[must_use]
    pub fn kind(&self) -> SpotifyEntityKind {
        self.kind
    }

    /// Returns the entity's Spotify identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Formats the URI in colon-delimited Spotify form (`spotify:track:<id>`).
    #[must_use]
    pub fn to_colon_uri(&self) -> String {
        format!("spotify:{}:{}", self.kind, self.id)
    }

    /// Formats the URI as an LMS protocol URL (`spotify://track:<id>`).
    #[must_use]
    pub fn to_protocol_url(&self) -> String {
        format!("spotify://{}:{}", self.kind, self.id)
    }
}

impl fmt::Display for SpotifyUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "spotify:{}:{}", self.kind, self.id)
    }
}

impl FromStr for SpotifyUri {
    type Err = SpottyError;

    fn from_str(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SpottyError::InvalidUri(String::from("empty Spotify URI")));
        }

        if let Some(after_scheme) = strip_prefix_ascii_ci(trimmed, "spotify:") {
            return parse_spotify_scheme(after_scheme, trimmed);
        }

        if let Some(after_http) = strip_http_open_spotify(trimmed) {
            return parse_open_spotify_path(after_http, trimmed);
        }

        Err(SpottyError::InvalidUri(format!(
            "unrecognized Spotify URI format: {trimmed}"
        )))
    }
}

fn parse_spotify_scheme(after_scheme: &str, original: &str) -> Result<SpotifyUri> {
    // Strip protocol handler slashes (`spotify://track:<id>` -> `track:<id>`).
    let cleaned = after_scheme.replace('/', "");
    let parts: Vec<&str> = cleaned.split(':').collect();
    match parts.as_slice() {
        [kind_raw, id_raw] => {
            let kind: SpotifyEntityKind = kind_raw.parse()?;
            SpotifyUri::new(kind, *id_raw)
        }
        ["user", user_id, playlist_kw, playlist_id]
            if playlist_kw.eq_ignore_ascii_case("playlist") =>
        {
            if !is_valid_user_id(user_id) {
                return Err(SpottyError::InvalidUri(format!(
                    "invalid Spotify user id in playlist URI: {original}"
                )));
            }
            SpotifyUri::new(SpotifyEntityKind::Playlist, *playlist_id)
        }
        _ => Err(SpottyError::InvalidUri(format!(
            "malformed spotify URI: {original}"
        ))),
    }
}

fn parse_open_spotify_path(after_host: &str, original: &str) -> Result<SpotifyUri> {
    let without_query = after_host.split(['?', '#']).next().unwrap_or("");
    let segments: Vec<&str> = without_query
        .trim_matches('/')
        .split('/')
        .filter(|seg| !seg.is_empty())
        .collect();

    let (kind_raw, id_raw) = match segments.as_slice() {
        [kind_seg, id_seg] => (*kind_seg, *id_seg),
        [locale_seg, kind_seg, id_seg] if starts_with_ascii_ci(locale_seg, "intl-") => {
            (*kind_seg, *id_seg)
        }
        ["user", user_id, playlist_kw, playlist_id]
            if playlist_kw.eq_ignore_ascii_case("playlist") =>
        {
            if !is_valid_user_id(user_id) {
                return Err(SpottyError::InvalidUri(format!(
                    "invalid Spotify user id in URL: {original}"
                )));
            }
            (*playlist_kw, *playlist_id)
        }
        _ => {
            return Err(SpottyError::InvalidUri(format!(
                "malformed open.spotify.com URL path: {original}"
            )));
        }
    };

    let kind: SpotifyEntityKind = kind_raw.parse()?;
    SpotifyUri::new(kind, id_raw)
}

fn strip_http_open_spotify(input: &str) -> Option<&str> {
    let after_proto = strip_prefix_ascii_ci(input, "https://")
        .or_else(|| strip_prefix_ascii_ci(input, "http://"))?;
    strip_prefix_ascii_ci(after_proto, "open.spotify.com/")
}

fn strip_prefix_ascii_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    let head = input.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        input.get(prefix.len()..)
    } else {
        None
    }
}

fn starts_with_ascii_ci(input: &str, prefix: &str) -> bool {
    strip_prefix_ascii_ci(input, prefix).is_some()
}

fn is_valid_user_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_valid_entity_id(kind: SpotifyEntityKind, id: &str) -> bool {
    if matches!(kind, SpotifyEntityKind::User) {
        return is_valid_user_id(id);
    }
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_colon_and_protocol_uris() {
        let colon: SpotifyUri = "spotify:track:4uLU6hMCjMI75M1A2tKUQC"
            .parse()
            .expect("colon URI should parse");
        assert_eq!(colon.kind(), SpotifyEntityKind::Track);
        assert_eq!(colon.id(), "4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(colon.to_colon_uri(), "spotify:track:4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(
            colon.to_protocol_url(),
            "spotify://track:4uLU6hMCjMI75M1A2tKUQC"
        );
        assert_eq!(colon.to_string(), "spotify:track:4uLU6hMCjMI75M1A2tKUQC");

        let proto: SpotifyUri = "spotify://episode:512HGM7vA9h4vA3p2j"
            .parse()
            .expect("protocol URL should parse");
        assert_eq!(proto.kind(), SpotifyEntityKind::Episode);
        assert_eq!(proto.id(), "512HGM7vA9h4vA3p2j");
        assert_eq!(proto.to_colon_uri(), "spotify:episode:512HGM7vA9h4vA3p2j");
    }

    #[test]
    fn parses_legacy_user_playlist_uri() {
        let chart: SpotifyUri = "spotify:user:spotifycharts:playlist:37i9dQZEVXbLRQDuF5jeBp"
            .parse()
            .expect("legacy user playlist URI should parse");
        assert_eq!(chart.kind(), SpotifyEntityKind::Playlist);
        assert_eq!(chart.id(), "37i9dQZEVXbLRQDuF5jeBp");
        assert_eq!(
            chart.to_colon_uri(),
            "spotify:playlist:37i9dQZEVXbLRQDuF5jeBp"
        );
    }

    #[test]
    fn parses_open_spotify_urls_with_optional_locale_and_query() {
        let plain: SpotifyUri = "https://open.spotify.com/playlist/3i6JdDL2IaDoIdrZCVngUv"
            .parse()
            .expect("https open.spotify.com URL should parse");
        assert_eq!(plain.kind(), SpotifyEntityKind::Playlist);
        assert_eq!(plain.id(), "3i6JdDL2IaDoIdrZCVngUv");

        let intl: SpotifyUri =
            "https://open.spotify.com/intl-de/album/3xN9KNcFz7zgjfNu6mQD6M?si=pYVY9S1WTSm"
                .parse()
                .expect("intl-de open.spotify.com URL should parse");
        assert_eq!(intl.kind(), SpotifyEntityKind::Album);
        assert_eq!(intl.id(), "3xN9KNcFz7zgjfNu6mQD6M");

        let http_user: SpotifyUri = "http://open.spotify.com/user/test_user.1?si=abc#frag"
            .parse()
            .expect("http user URL should parse");
        assert_eq!(http_user.kind(), SpotifyEntityKind::User);
        assert_eq!(http_user.id(), "test_user.1");

        let web_user_playlist: SpotifyUri =
            "https://open.spotify.com/user/spotifycharts/playlist/37i9dQZEVXbLRQDuF5jeBp?si=1"
                .parse()
                .expect("legacy web user playlist URL should parse");
        assert_eq!(web_user_playlist.kind(), SpotifyEntityKind::Playlist);
        assert_eq!(web_user_playlist.id(), "37i9dQZEVXbLRQDuF5jeBp");
    }

    #[test]
    fn rejects_invalid_uris() {
        let invalid_cases = [
            "",
            "   ",
            "spotify:unknown:123456",
            "spotify:track:",
            "spotify:track:bad!id",
            "spotify:user::playlist:123",
            "https://example.com/track/4uLU6hMCjMI75M1A2tKUQC",
            "https://open.spotify.com/fr/album/3xN9KNcFz7zgjfNu6mQD6M",
            "https://open.spotify.com/track",
        ];

        for raw in invalid_cases {
            let parsed = raw.parse::<SpotifyUri>();
            assert!(
                matches!(parsed, Err(SpottyError::InvalidUri(_))),
                "expected InvalidUri for {raw:?}, got {parsed:?}"
            );
        }
    }

    fn entity_kind_strategy() -> impl Strategy<Value = SpotifyEntityKind> {
        prop_oneof![
            Just(SpotifyEntityKind::Track),
            Just(SpotifyEntityKind::Episode),
            Just(SpotifyEntityKind::Album),
            Just(SpotifyEntityKind::Artist),
            Just(SpotifyEntityKind::Playlist),
            Just(SpotifyEntityKind::Show),
            Just(SpotifyEntityKind::User),
        ]
    }

    proptest! {
        #[test]
        fn roundtrips_colon_protocol_and_web_urls(
            kind in entity_kind_strategy(),
            id in "[a-zA-Z0-9]{1,32}",
            use_intl in any::<bool>(),
            locale in "[a-z]{2}",
            query in "[a-zA-Z0-9]{1,12}",
        ) {
            let uri = SpotifyUri::new(kind, &id).expect("alphanumeric ID should construct");

            let from_colon: SpotifyUri = uri.to_colon_uri().parse().expect("colon URI roundtrip");
            prop_assert_eq!(&from_colon, &uri);

            let from_proto: SpotifyUri = uri
                .to_protocol_url()
                .parse()
                .expect("protocol URL roundtrip");
            prop_assert_eq!(&from_proto, &uri);

            let web_url = if use_intl {
                format!("https://open.spotify.com/intl-{locale}/{kind}/{id}?si={query}")
            } else {
                format!("https://open.spotify.com/{kind}/{id}?si={query}")
            };
            let from_web: SpotifyUri = web_url.parse().expect("web URL roundtrip");
            prop_assert_eq!(&from_web, &uri);
        }
    }
}
