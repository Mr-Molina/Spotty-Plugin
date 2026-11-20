//! `DontStopTheMusic` smart mix recommender (`DontStopTheMusic.pm`).

use std::collections::HashSet;

use spotty_api::{RecommendationSeeds, SearchKind, SpotifyClient, MAX_RECOMMENDATION_SEEDS};
use spotty_types::{Result, SpotifyEntityKind, SpotifyUri};

/// LMS `DontStopTheMusic` handler identifier (`DontStopTheMusic.pm:16`).
pub const DSTM_HANDLER_ID: &str = "PLUGIN_SPOTTY_RECOMMENDATIONS";

/// Maximum recommendation tracks requested per DSTM mix
/// (`DontStopTheMusic.pm:37`).
pub const DSTM_RECOMMENDATION_LIMIT: usize = 25;

const DSTM_SEARCH_LIMIT: usize = 5;

/// Candidate seed track from the player's active playlist
/// (`DontStopTheMusic.pm:56-73`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SeedTrackInput {
    /// Optional track URL or Spotify URI (such as `spotify:track:<id>`).
    pub url: Option<String>,
    /// Optional performing artist name used when `url` is not a Spotify track.
    pub artist: Option<String>,
    /// Optional track title used when `url` is not a Spotify track.
    pub title: Option<String>,
}

impl SeedTrackInput {
    /// Creates a new [`SeedTrackInput`].
    #[must_use]
    pub fn new(url: Option<String>, artist: Option<String>, title: Option<String>) -> Self {
        Self { url, artist, title }
    }

    /// Creates a [`SeedTrackInput`] from a Spotify track URL or URI.
    #[must_use]
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            artist: None,
            title: None,
        }
    }

    /// Creates a local-track [`SeedTrackInput`] from an artist and track title.
    #[must_use]
    pub fn from_artist_title(artist: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            url: None,
            artist: Some(artist.into()),
            title: Some(title.into()),
        }
    }
}

/// Smart mix recommender resolving playlist seeds into Spotify recommendations.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
pub struct DstmRecommender;

impl DstmRecommender {
    /// Generates up to [`DSTM_RECOMMENDATION_LIMIT`] `spotify://track:<id>` URLs
    /// from the provided playlist `seeds`.
    ///
    /// Enforces Spotify's combined 5-seed cap via [`RecommendationSeeds::build`]
    /// (`seed_tracks.len() + seed_artists.len() <= 5`, prioritizing tracks), and
    /// returns `Ok(Vec::new())` when zero seeds resolve.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if Spotify search or recommendation
    /// requests fail.
    pub async fn generate_mix(
        client: &SpotifyClient,
        seeds: &[SeedTrackInput],
    ) -> Result<Vec<String>> {
        let mut seed_tracks: Vec<String> = Vec::new();
        let mut seen_track_ids: HashSet<String> = HashSet::new();
        let mut search_pairs: Vec<(String, String)> = Vec::new();
        let mut seen_pairs: HashSet<(String, String)> = HashSet::new();

        for seed in seeds {
            if let Some(track_id) = extract_track_id(seed.url.as_deref()) {
                if seen_track_ids.insert(track_id.clone()) {
                    seed_tracks.push(track_id);
                }
                continue;
            }

            let Some(artist_raw) = seed.artist.as_deref() else {
                continue;
            };
            let Some(title_raw) = seed.title.as_deref() else {
                continue;
            };
            let artist = artist_raw.trim();
            let title = title_raw.trim();
            if artist.is_empty() || title.is_empty() {
                continue;
            }
            let dedup_key = (artist.to_lowercase(), title.to_lowercase());
            if seen_pairs.insert(dedup_key) {
                search_pairs.push((artist.to_owned(), title.to_owned()));
            }
        }

        let mut unresolved_pairs: Vec<(String, String)> = Vec::new();
        if !search_pairs.is_empty() && seed_tracks.len() < MAX_RECOMMENDATION_SEEDS {
            for (artist, title) in search_pairs {
                if seed_tracks.len() >= MAX_RECOMMENDATION_SEEDS {
                    break;
                }
                let escaped_title = escape_search_value(&title);
                let escaped_artist = escape_search_value(&artist);
                let query = format!("{escaped_title} artist:\"{escaped_artist}\"");
                let res = client
                    .search(
                        &query,
                        SearchKind::Track,
                        DSTM_SEARCH_LIMIT,
                        DSTM_SEARCH_LIMIT,
                    )
                    .await?;
                if let Some(track_id) = find_matching_track_id(&res.tracks, &artist, &title) {
                    if seen_track_ids.insert(track_id.clone()) {
                        seed_tracks.push(track_id);
                    }
                } else {
                    unresolved_pairs.push((artist, title));
                }
            }
        }

        let mut seed_artists: Vec<String> = Vec::new();
        let mut seen_artist_ids: HashSet<String> = HashSet::new();
        let mut seen_artist_queries: HashSet<String> = HashSet::new();
        let remaining_slots = MAX_RECOMMENDATION_SEEDS.saturating_sub(seed_tracks.len());
        if !unresolved_pairs.is_empty() && remaining_slots > 0 {
            for (artist, _title) in unresolved_pairs {
                if seed_artists.len() >= remaining_slots {
                    break;
                }
                let artist_lower = artist.to_lowercase();
                if !seen_artist_queries.insert(artist_lower) {
                    continue;
                }
                let escaped_artist = escape_search_value(&artist);
                let query = format!("artist:\"{escaped_artist}\"");
                let res = client
                    .search(
                        &query,
                        SearchKind::Artist,
                        DSTM_SEARCH_LIMIT,
                        DSTM_SEARCH_LIMIT,
                    )
                    .await?;
                if let Some(artist_id) = find_matching_artist_id(&res.artists, &artist) {
                    if seen_artist_ids.insert(artist_id.clone()) {
                        seed_artists.push(artist_id);
                    }
                }
            }
        }

        let mut track_refs: Vec<&str> = Vec::with_capacity(seed_tracks.len());
        for track_id in &seed_tracks {
            track_refs.push(track_id.as_str());
        }
        let mut artist_refs: Vec<&str> = Vec::with_capacity(seed_artists.len());
        for artist_id in &seed_artists {
            artist_refs.push(artist_id.as_str());
        }

        let rec_seeds = RecommendationSeeds::build(&track_refs, &artist_refs, &[]);
        if rec_seeds.is_empty() {
            return Ok(Vec::new());
        }

        let recommendations = client
            .recommendations(&rec_seeds, DSTM_RECOMMENDATION_LIMIT)
            .await?;

        let mut urls = Vec::with_capacity(recommendations.len());
        for track in recommendations {
            if let Some(protocol_url) = track_to_protocol_url(&track.id, &track.uri) {
                urls.push(protocol_url);
            }
        }
        Ok(urls)
    }
}

fn escape_search_value(value: &str) -> String {
    value.replace('"', "\\\"")
}

fn extract_track_id(raw: Option<&str>) -> Option<String> {
    use std::str::FromStr as _;

    let raw_str = raw?.trim();
    if raw_str.is_empty() {
        return None;
    }
    if let Ok(uri) = SpotifyUri::from_str(raw_str) {
        if uri.kind() == SpotifyEntityKind::Track {
            return Some(uri.id().to_owned());
        }
        return None;
    }

    let lower = raw_str.to_ascii_lowercase();
    let prefix_len = if lower.starts_with("spotify://track:") {
        "spotify://track:".len()
    } else if lower.starts_with("spotify:track:") {
        "spotify:track:".len()
    } else if lower.starts_with("track:") {
        "track:".len()
    } else {
        return None;
    };
    let after = raw_str.get(prefix_len..)?;
    let mut id_end = 0usize;
    for (offset, ch) in after.char_indices() {
        if !ch.is_ascii_alphanumeric() {
            break;
        }
        id_end = offset + ch.len_utf8();
    }
    let candidate = after.get(..id_end)?;
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_owned())
    }
}

fn contains_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if haystack == needle {
        return true;
    }
    let mut start = 0usize;
    while let Some(rel) = haystack[start..].find(needle) {
        let idx = start + rel;
        let end = idx + needle.len();
        let left_ok = haystack[..idx]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric());
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_alphanumeric());
        if left_ok && right_ok {
            return true;
        }
        start = idx + 1;
        if !haystack.is_char_boundary(start) {
            while start < haystack.len() && !haystack.is_char_boundary(start) {
                start += 1;
            }
        }
    }
    false
}

fn title_matches(track_name: &str, title_lower: &str) -> bool {
    if title_lower.is_empty() {
        return false;
    }
    let name_lower = track_name.to_lowercase();
    if name_lower == title_lower {
        return true;
    }
    if let Some(rest) = name_lower.strip_prefix(title_lower) {
        let trimmed = rest.trim_start();
        return trimmed.starts_with('(')
            || trimmed.starts_with('[')
            || trimmed.starts_with('-')
            || trimmed.starts_with('/');
    }
    false
}

fn find_matching_track_id(
    tracks: &[spotty_api::NormalizedTrack],
    artist: &str,
    title: &str,
) -> Option<String> {
    let title_lower = title.trim().to_lowercase();
    let artist_lower = artist.trim().to_lowercase();
    if title_lower.is_empty() || artist_lower.is_empty() {
        return None;
    }

    for track in tracks {
        if track.id.is_empty() || !title_matches(&track.name, &title_lower) {
            continue;
        }
        let artist_matched = track.artists.iter().any(|track_artist| {
            contains_word_boundary(&track_artist.name.to_lowercase(), &artist_lower)
        });
        if artist_matched {
            return Some(track.id.clone());
        }
    }
    None
}

fn find_matching_artist_id(
    artists: &[spotty_api::NormalizedArtist],
    artist: &str,
) -> Option<String> {
    let artist_lower = artist.trim().to_lowercase();
    if artist_lower.is_empty() {
        return None;
    }
    for item in artists {
        if item.id.is_empty() {
            continue;
        }
        if contains_word_boundary(&item.name.to_lowercase(), &artist_lower) {
            return Some(item.id.clone());
        }
    }
    None
}

fn track_to_protocol_url(id: &str, uri: &str) -> Option<String> {
    use std::str::FromStr as _;

    let trimmed_id = id.trim();
    if !trimmed_id.is_empty() {
        return Some(format!("spotify://track:{trimmed_id}"));
    }
    if let Ok(parsed) = SpotifyUri::from_str(uri) {
        if parsed.kind() == SpotifyEntityKind::Track {
            return Some(parsed.to_protocol_url());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::json;
    use spotty_api::{
        Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
    };
    use spotty_cache::{AccountStore, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn build_test_client(mock_uri: &str, tmp: &TempDir) -> SpotifyClient {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let accounts = Arc::new(AccountStore::new(tmp.path()));
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client");
        let tokens = Arc::new(TokenManager::with_http(
            format!("{mock_uri}/api/token"),
            http,
            Arc::clone(&cache),
            accounts,
            1_700_000_000,
        ));
        let client_id = IconCode::default();
        tokens
            .cache_tokens(&client_id, "alice", "tok-test", 3600, None)
            .expect("seed access token");

        let gate = Arc::new(RateLimitGate::new());
        let transport = Arc::new(
            SpotifyTransport::with_base_url(
                format!("{mock_uri}/v1"),
                tokens,
                Arc::clone(&cache),
                gate,
                client_id.clone(),
                "alice",
            )
            .expect("build SpotifyTransport"),
        );
        let pipeline = Pipeline::new(Arc::clone(&transport));

        SpotifyClient::new(
            transport,
            pipeline,
            cache,
            client_id,
            "alice",
            "US",
            RateLimitPolicy::FailFastServer,
        )
    }

    #[test]
    fn extract_track_id_handles_uris_and_local_paths() {
        assert_eq!(
            extract_track_id(Some("spotify:track:trk001")).as_deref(),
            Some("trk001"),
            "spotify:track:<id> must extract track ID"
        );
        assert_eq!(
            extract_track_id(Some("track:trk002")).as_deref(),
            Some("trk002"),
            "bare track:<id> must extract track ID"
        );
        assert_eq!(
            extract_track_id(Some("file:///music/local.flac")),
            None,
            "local file URL must return None"
        );
        assert_eq!(
            extract_track_id(Some("file:///music/soundtrack:01.flac")),
            None,
            "local file URL containing 'track:' must return None"
        );
    }

    #[test]
    fn track_and_artist_matching_respects_boundaries_and_escapes_quotes() {
        assert_eq!(
            escape_search_value(r#"Rock "n" Roll"#),
            r#"Rock \"n\" Roll"#
        );
        assert!(title_matches("One (Remastered)", "one"));
        assert!(!title_matches("One More Time", "one"));
        assert!(contains_word_boundary("the artist one", "artist one"));
        assert!(!contains_word_boundary("mozart", "art"));
    }

    #[tokio::test]
    async fn generate_mix_caps_combined_seeds_at_five_with_four_spotify_and_three_local_tracks() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        // Local 1 resolves via track search, reaching 5 track seeds immediately
        // so Local 2 and Local 3 do not need to be queried.
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "track"))
            .and(query_param("q", "Local One artist:\"Artist One\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "lt1",
                        "uri": "spotify:track:lt1",
                        "name": "Local One (Remastered)",
                        "is_playable": true,
                        "artists": [{
                            "id": "a1",
                            "uri": "spotify:artist:a1",
                            "name": "The Artist One"
                        }],
                        "album": {
                            "id": "al1",
                            "uri": "spotify:album:al1",
                            "name": "Al1",
                            "images": []
                        }
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/recommendations"))
            .and(query_param("seed_tracks", "sp1,sp2,sp3,sp4,lt1"))
            .and(query_param("limit", "25"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [{
                    "id": "rec1",
                    "uri": "spotify:track:rec1",
                    "name": "Recommended One",
                    "is_playable": true,
                    "artists": [],
                    "album": {"id": "al1", "uri": "spotify:album:al1", "name": "Al", "images": []}
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let seeds = vec![
            SeedTrackInput::from_url("spotify:track:sp1"),
            SeedTrackInput::from_url("spotify:track:sp2"),
            SeedTrackInput::from_url("track:sp3"),
            SeedTrackInput::from_url("spotify://track:sp4"),
            SeedTrackInput::from_artist_title("Artist One", "Local One"),
            SeedTrackInput::from_artist_title("Artist Two", "Local Two"),
            SeedTrackInput::from_artist_title("Artist Three", "Local Three"),
        ];

        let urls = DstmRecommender::generate_mix(&client, &seeds)
            .await
            .expect("generate_mix must succeed");
        assert_eq!(urls, vec!["spotify://track:rec1"]);
    }

    #[tokio::test]
    async fn generate_mix_artist_fallback_caps_combined_track_plus_artist_seeds_at_five() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "track"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": { "total": 0, "next": null, "items": [] }
            })))
            .expect(3)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "artist"))
            .and(query_param("q", "artist:\"Artist A\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "artA",
                        "uri": "spotify:artist:artA",
                        "name": "Artist A",
                        "images": []
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/recommendations"))
            .and(query_param("seed_tracks", "sp1,sp2,sp3,sp4"))
            .and(query_param("seed_artists", "artA"))
            .and(query_param("limit", "25"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [{
                    "id": "rec9",
                    "uri": "spotify:track:rec9",
                    "name": "Recommended Nine",
                    "is_playable": true,
                    "artists": [],
                    "album": {"id": "al1", "uri": "spotify:album:al1", "name": "Al", "images": []}
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let seeds = vec![
            SeedTrackInput::from_url("spotify:track:sp1"),
            SeedTrackInput::from_url("spotify:track:sp2"),
            SeedTrackInput::from_url("spotify:track:sp3"),
            SeedTrackInput::from_url("spotify:track:sp4"),
            SeedTrackInput::from_artist_title("Artist A", "Song A"),
            SeedTrackInput::from_artist_title("Artist B", "Song B"),
            SeedTrackInput::from_artist_title("Artist C", "Song C"),
        ];

        let urls = DstmRecommender::generate_mix(&client, &seeds)
            .await
            .expect("generate_mix with artist fallback must succeed");
        assert_eq!(urls, vec!["spotify://track:rec9"]);
    }

    #[tokio::test]
    async fn generate_mix_returns_empty_when_zero_seeds_resolve() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let urls = DstmRecommender::generate_mix(&client, &[])
            .await
            .expect("empty seeds must return Ok(Vec::new())");
        assert!(urls.is_empty(), "expected empty URLs: {urls:?}");
    }
}
