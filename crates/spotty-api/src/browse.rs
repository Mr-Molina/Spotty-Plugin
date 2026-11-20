//! Spotify Web API v1 browse, home, featured playlists, and recommendations.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use spotty_cache::MetadataWriteMode;
use spotty_types::{Result, SpottyError};

use crate::catalog::{is_playable_item, usize_field, SpotifyClient, SPOTIFY_LIMIT};
use crate::models::{BrowseCategory, NormalizedAlbum, NormalizedPlaylist, NormalizedTrack};
use crate::pipeline::OffsetPage;
use crate::transport::CachePolicy;

/// Spotify category ID for personalized "Made For You" mixes (`API.pm:18`).
pub const PERSONAL_MIX_CATEGORY: &str = "0JQ5DAt0tbjZptfcdMSKl3";

/// Maximum combined seed count accepted by `/v1/recommendations`.
pub const MAX_RECOMMENDATION_SEEDS: usize = 5;

/// Maximum tracks returned per `/v1/recommendations` call (`API.pm:14`).
pub const RECOMMENDATION_LIMIT: usize = 100;

/// Response from `/v1/browse/featured-playlists`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeaturedPlaylists {
    /// Optional localized Spotify headline message.
    pub message: Option<String>,
    /// Featured playlists available in the configured market.
    pub playlists: Vec<NormalizedPlaylist>,
}

/// Validated recommendation seed bundle enforcing
/// `tracks.len() + artists.len() + genres.len() <= 5`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecommendationSeeds {
    tracks: Vec<String>,
    artists: Vec<String>,
    genres: Vec<String>,
}

impl RecommendationSeeds {
    /// Builds [`RecommendationSeeds`], keeping tracks first and truncating
    /// artists then genres so the combined total never exceeds `5`.
    #[must_use]
    pub fn build(tracks: &[&str], artists: &[&str], genres: &[&str]) -> Self {
        let mut out_tracks = Vec::new();
        for raw in tracks {
            if out_tracks.len() >= MAX_RECOMMENDATION_SEEDS {
                break;
            }
            let clean = strip_seed_prefix(raw, "spotify:track:");
            if !clean.is_empty() {
                out_tracks.push(clean.to_owned());
            }
        }

        let mut out_artists = Vec::new();
        for raw in artists {
            if out_tracks.len() + out_artists.len() >= MAX_RECOMMENDATION_SEEDS {
                break;
            }
            let clean = strip_seed_prefix(raw, "spotify:artist:");
            if !clean.is_empty() {
                out_artists.push(clean.to_owned());
            }
        }

        let mut out_genres = Vec::new();
        for raw in genres {
            if out_tracks.len() + out_artists.len() + out_genres.len() >= MAX_RECOMMENDATION_SEEDS {
                break;
            }
            let clean = raw.trim();
            if !clean.is_empty() {
                out_genres.push(clean.to_owned());
            }
        }

        Self {
            tracks: out_tracks,
            artists: out_artists,
            genres: out_genres,
        }
    }

    /// Returns the selected track seed IDs.
    #[must_use]
    pub fn tracks(&self) -> &[String] {
        &self.tracks
    }

    /// Returns the selected artist seed IDs.
    #[must_use]
    pub fn artists(&self) -> &[String] {
        &self.artists
    }

    /// Returns the selected genre seed names.
    #[must_use]
    pub fn genres(&self) -> &[String] {
        &self.genres
    }

    /// Returns the combined seed count (`<= 5`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len() + self.artists.len() + self.genres.len()
    }

    /// Returns `true` when no seeds are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Formats `epoch_secs` as an ISO-8601 UTC timestamp rounded down to the
/// nearest 10-minute boundary (`YYYY-MM-DDTHH:M0:00`) for cache friendliness.
#[must_use]
pub fn round_timestamp_10m(epoch_secs: u64) -> String {
    let rounded_secs = (epoch_secs / 600) * 600;
    let days = i64::try_from(rounded_secs / 86_400).unwrap_or(0);
    let secs_of_day = rounded_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u64, u64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m, d)
}

fn strip_seed_prefix<'a>(raw: &'a str, prefix: &str) -> &'a str {
    let trimmed = raw.trim();
    trimmed.strip_prefix(prefix).unwrap_or(trimmed)
}

impl SpotifyClient {
    /// Fetches newly released albums (`/v1/browse/new-releases`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn new_releases(&self, max_items: usize) -> Result<Vec<NormalizedAlbum>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let normalizer = self.normalizer();
        let cache = self.cache();
        let country = self.country();
        let params = [(String::from("country"), country.to_owned())];

        self.pipeline()
            .fetch_offset_parallel(
                "browse/new-releases",
                &params,
                SPOTIFY_LIMIT,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let Some(bucket) = json.get("albums").and_then(Value::as_object) else {
                        return Ok(OffsetPage::new(Vec::new(), 0, None));
                    };
                    let total = usize_field(bucket.get("total"));
                    let next = bucket
                        .get("next")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let raw_items = bucket
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut albums = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() || !is_playable_item(raw, country) {
                            continue;
                        }
                        albums.push(normalizer.normalize_album(
                            raw,
                            cache,
                            MetadataWriteMode::MergeExisting,
                        )?);
                    }
                    Ok(OffsetPage::new(albums, total, next))
                },
            )
            .await
    }

    /// Fetches browse categories (`/v1/browse/categories`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport or JSON failure.
    pub async fn categories(
        &self,
        locale: Option<&str>,
        max_items: usize,
    ) -> Result<Vec<BrowseCategory>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let normalizer = self.normalizer();
        let mut params = vec![(String::from("country"), self.country().to_owned())];
        if let Some(loc) = locale {
            if !loc.trim().is_empty() {
                params.push((String::from("locale"), loc.trim().to_owned()));
            }
        }

        self.pipeline()
            .fetch_offset_parallel(
                "browse/categories",
                &params,
                SPOTIFY_LIMIT,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let Some(bucket) = json.get("categories").and_then(Value::as_object) else {
                        return Ok(OffsetPage::new(Vec::new(), 0, None));
                    };
                    let total = usize_field(bucket.get("total"));
                    let next = bucket
                        .get("next")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let raw_items = bucket
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() {
                            continue;
                        }
                        out.push(normalizer.normalize_browse_category(raw)?);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await
    }

    /// Fetches playlists for a browse category
    /// (`/v1/browse/categories/{category_id}/playlists`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `category_id` is empty, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn category_playlists(
        &self,
        category_id: &str,
        max_items: usize,
    ) -> Result<Vec<NormalizedPlaylist>> {
        let trimmed = category_id.trim();
        if trimmed.is_empty() {
            return Err(SpottyError::InvalidUri(String::from(
                "category ID must not be empty",
            )));
        }
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let path = format!("browse/categories/{trimmed}/playlists");
        let params = [(String::from("country"), self.country().to_owned())];
        let normalizer = self.normalizer();
        let cache = self.cache();
        let country = self.country();

        self.pipeline()
            .fetch_offset_parallel(
                &path,
                &params,
                SPOTIFY_LIMIT,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let Some(bucket) = json.get("playlists").and_then(Value::as_object) else {
                        return Ok(OffsetPage::new(Vec::new(), 0, None));
                    };
                    let total = usize_field(bucket.get("total"));
                    let next = bucket
                        .get("next")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let raw_items = bucket
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() || !is_playable_item(raw, country) {
                            continue;
                        }
                        out.push(normalizer.normalize_playlist(raw, cache)?);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await
    }

    /// Fetches personalized "Made For You" home playlists by delegating to
    /// [`SpotifyClient::category_playlists`] with [`PERSONAL_MIX_CATEGORY`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn home(&self, max_items: usize) -> Result<Vec<NormalizedPlaylist>> {
        self.category_playlists(PERSONAL_MIX_CATEGORY, max_items)
            .await
    }

    /// Fetches featured playlists (`/v1/browse/featured-playlists`) using the
    /// current UTC timestamp rounded down to 10-minute intervals.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn featured_playlists(
        &self,
        locale: Option<&str>,
        max_items: usize,
    ) -> Result<FeaturedPlaylists> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.featured_playlists_at(locale, now, max_items).await
    }

    /// Fetches featured playlists (`/v1/browse/featured-playlists`) using an
    /// explicit `epoch_secs` rounded down to 10-minute intervals.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn featured_playlists_at(
        &self,
        locale: Option<&str>,
        epoch_secs: u64,
        max_items: usize,
    ) -> Result<FeaturedPlaylists> {
        if max_items == 0 {
            return Ok(FeaturedPlaylists::default());
        }
        let timestamp = round_timestamp_10m(epoch_secs);
        let mut params = vec![
            (String::from("country"), self.country().to_owned()),
            (String::from("timestamp"), timestamp),
        ];
        if let Some(loc) = locale {
            if !loc.trim().is_empty() {
                params.push((String::from("locale"), loc.trim().to_owned()));
            }
        }

        let normalizer = self.normalizer();
        let cache = self.cache();
        let country = self.country();
        let first_message = std::sync::Mutex::new(None);

        let playlists = self
            .pipeline()
            .fetch_offset_parallel(
                "browse/featured-playlists",
                &params,
                SPOTIFY_LIMIT,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    if let Some(msg) = json.get("message").and_then(Value::as_str) {
                        if let Ok(mut guard) = first_message.lock() {
                            if guard.is_none() {
                                *guard = Some(msg.to_owned());
                            }
                        }
                    }
                    let Some(bucket) = json.get("playlists").and_then(Value::as_object) else {
                        return Ok(OffsetPage::new(Vec::new(), 0, None));
                    };
                    let total = usize_field(bucket.get("total"));
                    let next = bucket
                        .get("next")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let raw_items = bucket
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() || !is_playable_item(raw, country) {
                            continue;
                        }
                        out.push(normalizer.normalize_playlist(raw, cache)?);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await?;

        let message = first_message.into_inner().ok().flatten();
        Ok(FeaturedPlaylists { message, playlists })
    }

    /// Fetches track recommendations (`/v1/recommendations`) using validated
    /// [`RecommendationSeeds`] (`<= 5` combined seeds).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `seeds` is empty, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn recommendations(
        &self,
        seeds: &RecommendationSeeds,
        limit: usize,
    ) -> Result<Vec<NormalizedTrack>> {
        if seeds.is_empty() {
            return Err(SpottyError::InvalidUri(String::from(
                "at least one recommendation seed is required",
            )));
        }

        let effective_limit = limit.clamp(1, RECOMMENDATION_LIMIT);
        let mut params = vec![
            (String::from("market"), self.country().to_owned()),
            (String::from("limit"), effective_limit.to_string()),
        ];
        if !seeds.tracks().is_empty() {
            params.push((String::from("seed_tracks"), seeds.tracks().join(",")));
        }
        if !seeds.artists().is_empty() {
            params.push((String::from("seed_artists"), seeds.artists().join(",")));
        }
        if !seeds.genres().is_empty() {
            params.push((String::from("seed_genres"), seeds.genres().join(",")));
        }

        let resp = self
            .transport()
            .get("recommendations", &params, CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        let raw_tracks = json
            .get("tracks")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let normalizer = self.normalizer();
        let cache = self.cache();
        let country = self.country();

        let mut tracks = Vec::with_capacity(raw_tracks.len());
        for raw in raw_tracks {
            if !raw.is_object() || !is_playable_item(raw, country) {
                continue;
            }
            tracks.push(normalizer.normalize_track(
                raw,
                None,
                cache,
                MetadataWriteMode::MergeExisting,
            )?);
        }
        Ok(tracks)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{round_timestamp_10m, RecommendationSeeds, PERSONAL_MIX_CATEGORY};
    use crate::catalog::test_support::build_test_client;

    #[test]
    fn recommendation_seeds_enforces_combined_cap_of_five() {
        let seeds = RecommendationSeeds::build(
            &["spotify:track:t1", "t2", "t3"],
            &["spotify:artist:a1", "a2", "a3", "a4"],
            &["rock", "jazz"],
        );
        assert_eq!(seeds.len(), 5, "combined seeds must never exceed 5");
        assert_eq!(seeds.tracks(), &["t1", "t2", "t3"]);
        assert_eq!(seeds.artists(), &["a1", "a2"]);
        assert!(seeds.genres().is_empty(), "genres must truncate to 0");

        let overflow_tracks =
            RecommendationSeeds::build(&["t1", "t2", "t3", "t4", "t5", "t6"], &["a1"], &["pop"]);
        assert_eq!(overflow_tracks.len(), 5);
        assert_eq!(overflow_tracks.tracks().len(), 5);
        assert!(overflow_tracks.artists().is_empty());
        assert!(overflow_tracks.genres().is_empty());
    }

    #[test]
    fn round_timestamp_10m_rounds_minutes_down_to_10_min_boundary() {
        // 2026-11-19T14:27:49Z -> 1795098469
        assert_eq!(round_timestamp_10m(1_795_098_469), "2026-11-19T14:20:00");
        // 2026-11-19T14:09:59Z -> 1795097399
        assert_eq!(round_timestamp_10m(1_795_097_399), "2026-11-19T14:00:00");
    }

    #[tokio::test]
    async fn featured_playlists_rounds_timestamp_and_home_uses_personal_mix_category() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/browse/featured-playlists"))
            .and(query_param("timestamp", "2026-11-19T14:20:00"))
            .and(query_param("country", "US"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": "Afternoon Focus",
                "playlists": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "fp1",
                        "uri": "spotify:playlist:fp1",
                        "name": "Deep Focus",
                        "owner": {"id": "spotify"},
                        "images": []
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let featured = client
            .featured_playlists_at(None, 1_795_098_469, 50)
            .await
            .expect("featured_playlists_at must succeed");
        assert_eq!(featured.message.as_deref(), Some("Afternoon Focus"));
        assert_eq!(featured.playlists.len(), 1);
        assert_eq!(featured.playlists[0].id, "fp1");

        let home_path = format!("/v1/browse/categories/{PERSONAL_MIX_CATEGORY}/playlists");
        Mock::given(method("GET"))
            .and(path(home_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "playlists": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "mix1",
                        "uri": "spotify:playlist:mix1",
                        "name": "Daily Mix 1",
                        "owner": {"id": "spotify"},
                        "images": []
                    }]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let home = client.home(50).await.expect("home must succeed");
        assert_eq!(home.len(), 1);
        assert_eq!(home[0].name, "Daily Mix 1");
    }

    #[tokio::test]
    async fn recommendations_sends_combined_capped_seeds() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let seeds = RecommendationSeeds::build(
            &["spotify:track:t1", "t2"],
            &["spotify:artist:a1", "a2", "a3", "a4"],
            &["rock"],
        );

        Mock::given(method("GET"))
            .and(path("/v1/recommendations"))
            .and(query_param("seed_tracks", "t1,t2"))
            .and(query_param("seed_artists", "a1,a2,a3"))
            .and(query_param("limit", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [{
                    "id": "rec1",
                    "uri": "spotify:track:rec1",
                    "name": "Recommended Track",
                    "is_playable": true,
                    "artists": [],
                    "album": {"id": "al1", "uri": "spotify:album:al1", "name": "Al", "images": []}
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let recs = client
            .recommendations(&seeds, 20)
            .await
            .expect("recommendations must succeed");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "rec1");
    }
}
