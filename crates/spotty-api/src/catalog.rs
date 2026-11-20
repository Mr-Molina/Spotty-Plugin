//! Spotify Web API v1 catalog, search, and track/episode batch client.

use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::Arc;

use serde_json::Value;
use spotty_cache::{DualCache, MetadataWriteMode};
use spotty_types::{
    IconCode, Result, SpotifyEntityKind, SpotifyUri, SpottyError, TagCleanupPolicy,
};

use crate::models::{
    NormalizedAlbum, NormalizedArtist, NormalizedEpisode, NormalizedPlaylist, NormalizedShow,
    NormalizedTrack, PlayerContextState, SearchResults, UserProfile,
};
use crate::normalize::{normalize_user_profile, EntityNormalizer};
use crate::pipeline::{OffsetPage, Pipeline};
use crate::transport::{CachePolicy, RateLimitPolicy, SpotifyTransport};

/// Standard Spotify Web API page size limit (`50`).
pub const SPOTIFY_LIMIT: usize = 50;

/// Spotify batch `/v1/albums` chunk limit (`20`).
pub const SPOTIFY_ALBUMS_LIMIT: usize = 20;

const DEFAULT_ARTICLES: [&str; 3] = ["The", "A", "An"];

/// Entity type passed to [`SpotifyClient::search`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchKind {
    /// Search Spotify albums.
    Album,
    /// Search Spotify artists.
    Artist,
    /// Search Spotify tracks.
    Track,
    /// Search Spotify playlists.
    Playlist,
    /// Search Spotify podcast shows.
    Show,
    /// Search Spotify podcast episodes.
    Episode,
}

impl SearchKind {
    /// Returns the singular `type=` query parameter value.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Track => "track",
            Self::Playlist => "playlist",
            Self::Show => "show",
            Self::Episode => "episode",
        }
    }

    /// Returns the plural JSON response bucket key.
    #[must_use]
    pub fn bucket_key(&self) -> &'static str {
        match self {
            Self::Album => "albums",
            Self::Artist => "artists",
            Self::Track => "tracks",
            Self::Playlist => "playlists",
            Self::Show => "shows",
            Self::Episode => "episodes",
        }
    }
}

/// Lookup policy for [`SpotifyClient::track_cached`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackCacheLookup {
    /// Return only cached entries without issuing network requests.
    CacheOnly,
    /// Fall back to `/v1/tracks/{id}` or `/v1/episodes/{id}` on cache miss.
    FetchIfMissing,
}

#[derive(Debug)]
enum SearchPageItem {
    Track(NormalizedTrack),
    Album(NormalizedAlbum),
    Artist(NormalizedArtist),
    Playlist(NormalizedPlaylist),
    Show(NormalizedShow),
    Episode(NormalizedEpisode),
}

/// High-level Spotify Web API v1 client wrapping [`SpotifyTransport`],
/// [`Pipeline`], and [`DualCache`].
#[derive(Debug, Clone)]
pub struct SpotifyClient {
    transport: Arc<SpotifyTransport>,
    pipeline: Pipeline,
    cache: Arc<DualCache>,
    client_id: IconCode,
    user_id: String,
    country: String,
    rate_limit_policy: RateLimitPolicy,
    leading_articles: Vec<String>,
    tag_cleanup: TagCleanupPolicy,
}

impl SpotifyClient {
    /// Creates a [`SpotifyClient`] with default `"US"` country, `"The"/"A"/"An"`
    /// leading articles, and [`TagCleanupPolicy::StripSuffixes`].
    #[must_use]
    pub fn new(
        transport: Arc<SpotifyTransport>,
        pipeline: Pipeline,
        cache: Arc<DualCache>,
        client_id: IconCode,
        user_id: impl Into<String>,
        country: impl Into<String>,
        rate_limit_policy: RateLimitPolicy,
    ) -> Self {
        let mut leading_articles = Vec::with_capacity(DEFAULT_ARTICLES.len());
        for article in DEFAULT_ARTICLES {
            leading_articles.push(String::from(article));
        }

        let raw_country = country.into();
        let resolved_country = if raw_country.trim().is_empty() {
            String::from("US")
        } else {
            raw_country
        };

        Self {
            transport,
            pipeline,
            cache,
            client_id,
            user_id: user_id.into(),
            country: resolved_country,
            rate_limit_policy,
            leading_articles,
            tag_cleanup: TagCleanupPolicy::StripSuffixes,
        }
    }

    /// Overrides the leading-article list used for artist `sortname` sorting.
    #[must_use]
    pub fn with_leading_articles(mut self, articles: Vec<String>) -> Self {
        self.leading_articles = articles;
        self
    }

    /// Overrides the [`TagCleanupPolicy`] applied when normalizing titles.
    #[must_use]
    pub fn with_tag_cleanup(mut self, policy: TagCleanupPolicy) -> Self {
        self.tag_cleanup = policy;
        self
    }

    /// Returns a reference to the underlying [`SpotifyTransport`].
    #[must_use]
    pub fn transport(&self) -> &Arc<SpotifyTransport> {
        &self.transport
    }

    /// Returns a reference to the underlying [`Pipeline`].
    #[must_use]
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// Returns a reference to the underlying [`DualCache`].
    #[must_use]
    pub fn cache(&self) -> &Arc<DualCache> {
        &self.cache
    }

    /// Returns the configured Spotify client [`IconCode`].
    #[must_use]
    pub fn client_id(&self) -> &IconCode {
        &self.client_id
    }

    /// Returns the configured Spotify user ID.
    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Returns the configured ISO 3166-1 alpha-2 country code.
    #[must_use]
    pub fn country(&self) -> &str {
        &self.country
    }

    /// Returns the configured [`RateLimitPolicy`].
    #[must_use]
    pub fn rate_limit_policy(&self) -> RateLimitPolicy {
        self.rate_limit_policy
    }

    /// Returns the configured leading-article list.
    #[must_use]
    pub fn leading_articles(&self) -> &[String] {
        &self.leading_articles
    }

    pub(crate) fn normalizer(&self) -> EntityNormalizer {
        EntityNormalizer::new(self.tag_cleanup)
    }

    pub(crate) fn article_slices(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(self.leading_articles.len());
        for article in &self.leading_articles {
            out.push(article.as_str());
        }
        out
    }

    /// Fetches the current authenticated user's profile (`/v1/me`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, auth, rate-limit, or JSON failure.
    pub async fn me(&self) -> Result<UserProfile> {
        let resp = self.transport.get("me", &[], CachePolicy::UseCache).await?;
        let json: Value = resp.json()?;
        normalize_user_profile(&json)
    }

    /// Fetches a public user profile (`/v1/users/{id}`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `user_id` is empty, or
    /// [`SpottyError`] on transport or JSON failure.
    pub async fn user(&self, user_id: &str) -> Result<UserProfile> {
        let trimmed = user_id.trim();
        if trimmed.is_empty() {
            return Err(SpottyError::InvalidUri(String::from(
                "user ID must not be empty",
            )));
        }
        let normalized = encode_user_segment(trimmed);
        let path = format!("users/{normalized}");
        let resp = self
            .transport
            .get(&path, &[], CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        normalize_user_profile(&json)
    }

    /// Fetches live Spotify Connect playback state (`/v1/me/player`) bypassing
    /// HTTP cache.
    ///
    /// Returns `Ok(None)` when Spotify responds with `204 No Content`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, auth, rate-limit, or JSON failure.
    pub async fn player(&self) -> Result<Option<PlayerContextState>> {
        let params = [(String::from("market"), String::from("from_token"))];
        let resp = self
            .transport
            .get("me/player", &params, CachePolicy::BypassCache)
            .await?;
        if resp.status == 204 || resp.body.trim().is_empty() {
            return Ok(None);
        }
        let json: Value = resp.json()?;
        let state = self.normalizer().normalize_player_state(
            &json,
            &self.cache,
            MetadataWriteMode::MergeExisting,
        )?;
        Ok(Some(state))
    }

    /// Searches Spotify for a single entity [`SearchKind`] via [`Pipeline`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn search(
        &self,
        query: &str,
        kind: SearchKind,
        limit: usize,
        max_items: usize,
    ) -> Result<SearchResults> {
        let trimmed = query.trim();
        if trimmed.is_empty() || max_items == 0 {
            return Ok(SearchResults::default());
        }

        let page_limit = limit.clamp(1, SPOTIFY_LIMIT);
        let params = [
            (String::from("q"), trimmed.to_owned()),
            (String::from("type"), String::from(kind.as_str())),
            (String::from("market"), String::from("from_token")),
        ];
        let bucket_key = kind.bucket_key();
        let articles = self.article_slices();
        let normalizer = self.normalizer();
        let country = self.country.as_str();

        let items = self
            .pipeline
            .fetch_offset_parallel(
                "search",
                &params,
                page_limit,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let Some(bucket) = json.get(bucket_key).and_then(Value::as_object) else {
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

                    let mut page_items = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if raw.is_null() || !is_playable_item(raw, country) {
                            continue;
                        }
                        let parsed = match kind {
                            SearchKind::Track => {
                                SearchPageItem::Track(normalizer.normalize_track(
                                    raw,
                                    None,
                                    &self.cache,
                                    MetadataWriteMode::MergeExisting,
                                )?)
                            }
                            SearchKind::Album => {
                                SearchPageItem::Album(normalizer.normalize_album(
                                    raw,
                                    &self.cache,
                                    MetadataWriteMode::MergeExisting,
                                )?)
                            }
                            SearchKind::Artist => SearchPageItem::Artist(
                                normalizer.normalize_artist(raw, &self.cache, &articles)?,
                            ),
                            SearchKind::Playlist => SearchPageItem::Playlist(
                                normalizer.normalize_playlist(raw, &self.cache)?,
                            ),
                            SearchKind::Show => SearchPageItem::Show(normalizer.normalize_show(
                                raw,
                                &self.cache,
                                MetadataWriteMode::MergeExisting,
                            )?),
                            SearchKind::Episode => {
                                SearchPageItem::Episode(normalizer.normalize_episode(
                                    raw,
                                    None,
                                    &self.cache,
                                    MetadataWriteMode::MergeExisting,
                                )?)
                            }
                        };
                        page_items.push(parsed);
                    }
                    Ok(OffsetPage::new(page_items, total, next))
                },
            )
            .await?;

        let mut results = SearchResults::default();
        let count = items.len();
        for item in items {
            match item {
                SearchPageItem::Track(t) => results.tracks.push(t),
                SearchPageItem::Album(a) => results.albums.push(a),
                SearchPageItem::Artist(a) => results.artists.push(a),
                SearchPageItem::Playlist(p) => results.playlists.push(p),
                SearchPageItem::Show(s) => results.shows.push(s),
                SearchPageItem::Episode(e) => results.episodes.push(e),
            }
        }
        results.total = Some(u32::try_from(count).unwrap_or(u32::MAX));
        Ok(results)
    }

    /// Fetches an album (`/v1/albums/{id}`), automatically paginating
    /// `/v1/albums/{id}/tracks` via [`Pipeline`] when `total_tracks > 50`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid album URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn album(&self, uri_or_id: &str) -> Result<NormalizedAlbum> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Album)?;
        let path = format!("albums/{id}");
        let params = [
            (String::from("market"), String::from("from_token")),
            (String::from("limit"), SPOTIFY_LIMIT.to_string()),
            (String::from("offset"), String::from("0")),
        ];
        let resp = self
            .transport
            .get(&path, &params, CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;

        let tracks_total = usize_field(json.get("tracks").and_then(|t| t.get("total")));
        let normalizer = self.normalizer();
        let mut album =
            normalizer.normalize_album(&json, &self.cache, MetadataWriteMode::MergeExisting)?;

        let total = tracks_total.max(album.total_tracks as usize);
        if total > SPOTIFY_LIMIT {
            let min_album = album.to_min_album();
            let tracks_path = format!("albums/{id}/tracks");
            let track_params = [(String::from("market"), String::from("from_token"))];
            let country = self.country.as_str();

            let hydrated = self
                .pipeline
                .fetch_offset_parallel(
                    &tracks_path,
                    &track_params,
                    SPOTIFY_LIMIT,
                    total,
                    CachePolicy::UseCache,
                    |page_json| {
                        let page_total = usize_field(page_json.get("total"));
                        let next = page_json
                            .get("next")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let raw_items = page_json
                            .get("items")
                            .and_then(Value::as_array)
                            .map_or(&[][..], Vec::as_slice);
                        let mut tracks = Vec::with_capacity(raw_items.len());
                        for raw_track in raw_items {
                            if raw_track.is_null() || !is_playable_item(raw_track, country) {
                                continue;
                            }
                            let normalized = normalizer.normalize_track(
                                raw_track,
                                Some(&min_album),
                                &self.cache,
                                MetadataWriteMode::MergeExisting,
                            )?;
                            tracks.push(normalized);
                        }
                        Ok(OffsetPage::new(tracks, page_total, next))
                    },
                )
                .await?;
            album.tracks = hydrated;

            if !album.uri.is_empty() {
                let payload = serde_json::to_value(&album)?;
                self.cache.store_metadata(
                    &album.uri,
                    None,
                    &payload,
                    MetadataWriteMode::OverwriteDirect,
                )?;
            }
        } else {
            let mut playable = Vec::with_capacity(album.tracks.len());
            for track in album.tracks {
                if track.is_playable {
                    playable.push(track);
                }
            }
            album.tracks = playable;
        }

        Ok(album)
    }

    /// Fetches multiple albums (`/v1/albums`) in 20-ID chunks.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid album ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn albums_batch(&self, ids_or_uris: &[&str]) -> Result<Vec<NormalizedAlbum>> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Album)?);
        }
        if clean_ids.is_empty() {
            return Ok(Vec::new());
        }

        let chunks = build_id_chunks(&clean_ids, SPOTIFY_ALBUMS_LIMIT, false);
        let normalizer = self.normalizer();
        self.pipeline
            .execute_indexed_chunks("albums", &chunks, CachePolicy::UseCache, |json| {
                let raw_albums = json
                    .get("albums")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                let mut out = Vec::with_capacity(raw_albums.len());
                for raw in raw_albums {
                    if !raw.is_object() {
                        continue;
                    }
                    out.push(normalizer.normalize_album(
                        raw,
                        &self.cache,
                        MetadataWriteMode::MergeExisting,
                    )?);
                }
                Ok(out)
            })
            .await
    }

    /// Fetches a single artist (`/v1/artists/{id}`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn artist(&self, uri_or_id: &str) -> Result<NormalizedArtist> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Artist)?;
        let path = format!("artists/{id}");
        let resp = self
            .transport
            .get(&path, &[], CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        let articles = self.article_slices();
        self.normalizer()
            .normalize_artist(&json, &self.cache, &articles)
    }

    /// Fetches multiple artists (`/v1/artists`) in 50-ID chunks.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn artists_batch(&self, ids_or_uris: &[&str]) -> Result<Vec<NormalizedArtist>> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Artist)?);
        }
        if clean_ids.is_empty() {
            return Ok(Vec::new());
        }

        let chunks = build_id_chunks(&clean_ids, SPOTIFY_LIMIT, false);
        let articles = self.article_slices();
        let normalizer = self.normalizer();
        self.pipeline
            .execute_indexed_chunks("artists", &chunks, CachePolicy::UseCache, |json| {
                let raw_artists = json
                    .get("artists")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                let mut out = Vec::with_capacity(raw_artists.len());
                for raw in raw_artists {
                    if !raw.is_object() {
                        continue;
                    }
                    out.push(normalizer.normalize_artist(raw, &self.cache, &articles)?);
                }
                Ok(out)
            })
            .await
    }

    /// Fetches related artists (`/v1/artists/{id}/related-artists`) sorted
    /// alphabetically by `sortname`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn related_artists(&self, uri_or_id: &str) -> Result<Vec<NormalizedArtist>> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Artist)?;
        let path = format!("artists/{id}/related-artists");
        let resp = self
            .transport
            .get(&path, &[], CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        let raw_artists = json
            .get("artists")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let articles = self.article_slices();
        let normalizer = self.normalizer();
        let mut artists = Vec::with_capacity(raw_artists.len());
        for raw in raw_artists {
            if !raw.is_object() {
                continue;
            }
            artists.push(normalizer.normalize_artist(raw, &self.cache, &articles)?);
        }
        artists.sort_by_key(|a| a.sortname.to_lowercase());
        Ok(artists)
    }

    /// Fetches an artist's top tracks (`/v1/artists/{id}/top-tracks`) for
    /// `country`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn artist_tracks(&self, uri_or_id: &str) -> Result<Vec<NormalizedTrack>> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Artist)?;
        let path = format!("artists/{id}/top-tracks");
        let params = [(String::from("country"), self.country.clone())];
        let resp = self
            .transport
            .get(&path, &params, CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        let raw_tracks = json
            .get("tracks")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let normalizer = self.normalizer();
        let mut tracks = Vec::with_capacity(raw_tracks.len());
        for raw in raw_tracks {
            if !raw.is_object() || !is_playable_item(raw, &self.country) {
                continue;
            }
            tracks.push(normalizer.normalize_track(
                raw,
                None,
                &self.cache,
                MetadataWriteMode::MergeExisting,
            )?);
        }
        Ok(tracks)
    }

    /// Fetches an artist's albums (`/v1/artists/{id}/albums`) via [`Pipeline`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn artist_albums(
        &self,
        uri_or_id: &str,
        include_groups: Option<&str>,
        limit: usize,
        max_items: usize,
    ) -> Result<Vec<NormalizedAlbum>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Artist)?;
        let path = format!("artists/{id}/albums");
        let groups = include_groups.unwrap_or("album,single,appears_on,compilation");
        let params = [
            (String::from("market"), self.country.clone()),
            (String::from("include_groups"), groups.to_owned()),
        ];
        let page_limit = limit.clamp(1, SPOTIFY_LIMIT);
        let normalizer = self.normalizer();

        self.pipeline
            .fetch_offset_parallel(
                &path,
                &params,
                page_limit,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let total = usize_field(json.get("total"));
                    let next = json.get("next").and_then(Value::as_str).map(str::to_owned);
                    let raw_items = json
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut albums = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() {
                            continue;
                        }
                        albums.push(normalizer.normalize_album(
                            raw,
                            &self.cache,
                            MetadataWriteMode::MergeExisting,
                        )?);
                    }
                    Ok(OffsetPage::new(albums, total, next))
                },
            )
            .await
    }

    /// Checks [`DualCache`] first for a track or episode URI, optionally
    /// fetching from Spotify on cache miss.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `uri` is not a valid track or
    /// episode URI, or [`SpottyError`] on cache, transport, or JSON failure.
    pub async fn track_cached(
        &self,
        uri: &str,
        lookup: TrackCacheLookup,
    ) -> Result<Option<NormalizedTrack>> {
        let parsed = SpotifyUri::from_str(uri)?;
        let canonical = parsed.to_colon_uri();
        match parsed.kind() {
            SpotifyEntityKind::Track => {
                if let Some(cached) = self.cache.metadata_value(&canonical)? {
                    if let Ok(track) = serde_json::from_value::<NormalizedTrack>(cached) {
                        return Ok(Some(track));
                    }
                }
                if lookup == TrackCacheLookup::CacheOnly {
                    return Ok(None);
                }
                let path = format!("tracks/{}", parsed.id());
                let params = [(String::from("market"), String::from("from_token"))];
                let resp = self
                    .transport
                    .get(&path, &params, CachePolicy::UseCache)
                    .await?;
                let json: Value = resp.json()?;
                let track = self.normalizer().normalize_track(
                    &json,
                    None,
                    &self.cache,
                    MetadataWriteMode::MergeExisting,
                )?;
                Ok(Some(track))
            }
            SpotifyEntityKind::Episode => {
                if let Some(cached) = self.cache.ephemeral_value(&canonical)? {
                    if let Ok(ep) = serde_json::from_value::<NormalizedEpisode>(cached.clone()) {
                        return Ok(Some(ep.to_track_view()));
                    }
                    if let Ok(track) = serde_json::from_value::<NormalizedTrack>(cached) {
                        return Ok(Some(track));
                    }
                }
                if lookup == TrackCacheLookup::CacheOnly {
                    return Ok(None);
                }
                let path = format!("episodes/{}", parsed.id());
                let params = [(String::from("market"), String::from("from_token"))];
                let resp = self
                    .transport
                    .get(&path, &params, CachePolicy::UseCache)
                    .await?;
                let json: Value = resp.json()?;
                let ep = self.normalizer().normalize_episode(
                    &json,
                    None,
                    &self.cache,
                    MetadataWriteMode::MergeExisting,
                )?;
                Ok(Some(ep.to_track_view()))
            }
            other => Err(SpottyError::InvalidUri(format!(
                "expected track or episode URI, got {other}"
            ))),
        }
    }

    /// Fetches mixed `spotify:track:*` and `spotify:episode:*` URIs in
    /// concurrent 50-ID batches via `tokio::try_join!`, returning a single
    /// combined list in the original input URI order.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if any item is not a valid track or
    /// episode URI, or [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn tracks_and_episodes(&self, uris: &[&str]) -> Result<Vec<NormalizedTrack>> {
        if uris.is_empty() {
            return Ok(Vec::new());
        }

        let mut ordered_refs = Vec::with_capacity(uris.len());
        let mut track_ids = Vec::new();
        let mut seen_tracks = HashSet::new();
        let mut episode_ids = Vec::new();
        let mut seen_episodes = HashSet::new();

        for raw in uris {
            let parsed = SpotifyUri::from_str(raw)?;
            let id = parsed.id().to_owned();
            match parsed.kind() {
                SpotifyEntityKind::Track => {
                    if seen_tracks.insert(id.clone()) {
                        track_ids.push(id.clone());
                    }
                    ordered_refs.push((SpotifyEntityKind::Track, id));
                }
                SpotifyEntityKind::Episode => {
                    if seen_episodes.insert(id.clone()) {
                        episode_ids.push(id.clone());
                    }
                    ordered_refs.push((SpotifyEntityKind::Episode, id));
                }
                other => {
                    return Err(SpottyError::InvalidUri(format!(
                        "unsupported URI kind in tracks_and_episodes: {other}"
                    )));
                }
            }
        }

        let track_chunks = build_id_chunks(&track_ids, SPOTIFY_LIMIT, true);
        let episode_chunks = build_id_chunks(&episode_ids, SPOTIFY_LIMIT, true);
        let normalizer = self.normalizer();
        let country = self.country.as_str();

        let tracks_fut = async {
            if track_chunks.is_empty() {
                return Ok::<Vec<NormalizedTrack>, SpottyError>(Vec::new());
            }
            self.pipeline
                .execute_indexed_chunks("tracks", &track_chunks, CachePolicy::UseCache, |json| {
                    let raw_tracks = json
                        .get("tracks")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_tracks.len());
                    for raw in raw_tracks {
                        if !raw.is_object() || !is_playable_item(raw, country) {
                            continue;
                        }
                        out.push(normalizer.normalize_track(
                            raw,
                            None,
                            &self.cache,
                            MetadataWriteMode::MergeExisting,
                        )?);
                    }
                    Ok(out)
                })
                .await
        };

        let episodes_fut = async {
            if episode_chunks.is_empty() {
                return Ok::<Vec<NormalizedTrack>, SpottyError>(Vec::new());
            }
            self.pipeline
                .execute_indexed_chunks(
                    "episodes",
                    &episode_chunks,
                    CachePolicy::UseCache,
                    |json| {
                        let raw_episodes = json
                            .get("episodes")
                            .and_then(Value::as_array)
                            .map_or(&[][..], Vec::as_slice);
                        let mut out = Vec::with_capacity(raw_episodes.len());
                        for raw in raw_episodes {
                            if !raw.is_object() || !is_playable_item(raw, country) {
                                continue;
                            }
                            let ep = normalizer.normalize_episode(
                                raw,
                                None,
                                &self.cache,
                                MetadataWriteMode::MergeExisting,
                            )?;
                            out.push(ep.to_track_view());
                        }
                        Ok(out)
                    },
                )
                .await
        };

        let (fetched_tracks, fetched_episodes) = tokio::try_join!(tracks_fut, episodes_fut)?;

        let mut track_map: HashMap<String, NormalizedTrack> =
            HashMap::with_capacity(fetched_tracks.len() * 2);
        for track in fetched_tracks {
            if let Some(linked) = &track.linked_from {
                if !linked.id.is_empty() {
                    track_map.insert(linked.id.clone(), track.clone());
                }
            }
            track_map.insert(track.id.clone(), track);
        }

        let mut episode_map: HashMap<String, NormalizedTrack> =
            HashMap::with_capacity(fetched_episodes.len());
        for ep_track in fetched_episodes {
            episode_map.insert(ep_track.id.clone(), ep_track);
        }

        let mut combined = Vec::with_capacity(ordered_refs.len());
        for (kind, id) in ordered_refs {
            let found = match kind {
                SpotifyEntityKind::Track => track_map.get(&id).cloned(),
                SpotifyEntityKind::Episode => episode_map.get(&id).cloned(),
                _ => None,
            };
            if let Some(item) = found {
                combined.push(item);
            }
        }
        Ok(combined)
    }

    /// Fetches a podcast show (`/v1/shows/{id}`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid show ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn show(&self, uri_or_id: &str) -> Result<NormalizedShow> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Show)?;
        let path = format!("shows/{id}");
        let params = [
            (String::from("market"), String::from("from_token")),
            (String::from("limit"), SPOTIFY_LIMIT.to_string()),
            (String::from("offset"), String::from("0")),
        ];
        let resp = self
            .transport
            .get(&path, &params, CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        self.normalizer()
            .normalize_show(&json, &self.cache, MetadataWriteMode::MergeExisting)
    }

    /// Fetches paginated episodes for a podcast show (`/v1/shows/{id}/episodes`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid show ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn episodes(
        &self,
        show_uri_or_id: &str,
        limit: usize,
        max_items: usize,
    ) -> Result<Vec<NormalizedEpisode>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let id = extract_entity_id(show_uri_or_id, SpotifyEntityKind::Show)?;
        let path = format!("shows/{id}/episodes");
        let params = [(String::from("market"), String::from("from_token"))];
        let page_limit = limit.clamp(1, SPOTIFY_LIMIT);
        let normalizer = self.normalizer();

        self.pipeline
            .fetch_offset_parallel(
                &path,
                &params,
                page_limit,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let total = usize_field(json.get("total"));
                    let next = json.get("next").and_then(Value::as_str).map(str::to_owned);
                    let raw_items = json
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() {
                            continue;
                        }
                        out.push(normalizer.normalize_episode(
                            raw,
                            None,
                            &self.cache,
                            MetadataWriteMode::MergeExisting,
                        )?);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await
    }
}

pub(crate) fn extract_entity_id(
    uri_or_id: &str,
    expected_kind: SpotifyEntityKind,
) -> Result<String> {
    let trimmed = uri_or_id.trim();
    if trimmed.is_empty() {
        return Err(SpottyError::InvalidUri(format!(
            "empty {expected_kind} identifier"
        )));
    }
    if trimmed.contains(':') || trimmed.contains('/') {
        let parsed = SpotifyUri::from_str(trimmed)?;
        if parsed.kind() != expected_kind {
            return Err(SpottyError::InvalidUri(format!(
                "expected {expected_kind} URI, got {}",
                parsed.kind()
            )));
        }
        return Ok(parsed.id().to_owned());
    }
    let validated = SpotifyUri::new(expected_kind, trimmed)?;
    Ok(validated.id().to_owned())
}

pub(crate) fn build_id_chunks(
    ids: &[String],
    chunk_size: usize,
    include_market: bool,
) -> Vec<(usize, Vec<(String, String)>)> {
    let size = chunk_size.max(1);
    let mut chunks = Vec::new();
    for (idx, chunk) in ids.chunks(size).enumerate() {
        let mut params = Vec::with_capacity(2);
        params.push((String::from("ids"), chunk.join(",")));
        if include_market {
            params.push((String::from("market"), String::from("from_token")));
        }
        chunks.push((idx, params));
    }
    chunks
}

pub(crate) fn encode_user_segment(user_id: &str) -> String {
    use std::fmt::Write as _;

    let lowered = user_id.trim().to_ascii_lowercase().replace(' ', "+");
    let mut out = String::with_capacity(lowered.len());
    for byte in lowered.bytes() {
        let is_safe = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+');
        if is_safe {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub(crate) fn usize_field(raw: Option<&Value>) -> usize {
    raw.and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0)
}

pub(crate) fn is_playable_item(raw: &Value, country: &str) -> bool {
    let Some(obj) = raw.as_object() else {
        return false;
    };
    if obj.get("is_local").and_then(Value::as_bool) == Some(true) {
        return false;
    }

    let uri = obj.get("uri").and_then(Value::as_str).unwrap_or("");
    let is_playable = obj.get("is_playable").and_then(Value::as_bool);
    let episode_flag = obj.get("episode").and_then(Value::as_bool);
    if is_playable == Some(false)
        && !(episode_flag == Some(false) && uri.starts_with("spotify:episode:"))
    {
        return false;
    }

    if let Some(markets) = obj.get("available_markets").and_then(Value::as_array) {
        if !markets.is_empty() {
            let mut matched = false;
            for market in markets {
                if let Some(code) = market.as_str() {
                    if code.eq_ignore_ascii_case(country) {
                        matched = true;
                        break;
                    }
                }
            }
            if !matched {
                return false;
            }
        }
    }

    true
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use spotty_cache::{AccountStore, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;

    use super::SpotifyClient;
    use crate::pipeline::Pipeline;
    use crate::token::TokenManager;
    use crate::transport::{RateLimitGate, RateLimitPolicy, SpotifyTransport};

    pub(crate) fn build_test_client(mock_uri: &str, tmp: &TempDir) -> SpotifyClient {
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
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::test_support::build_test_client;
    use super::{SearchKind, TrackCacheLookup};

    #[tokio::test]
    async fn tracks_and_episodes_merges_mixed_uris_in_single_ordered_result() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/tracks"))
            .and(query_param(
                "ids",
                "1111111111111111111111,3333333333333333333333",
            ))
            .and(query_param("market", "from_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [
                    {
                        "id": "1111111111111111111111",
                        "uri": "spotify:track:1111111111111111111111",
                        "name": "First Track",
                        "duration_ms": 180000,
                        "is_playable": true,
                        "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "A1"}],
                        "album": {
                            "id": "al1",
                            "uri": "spotify:album:al1",
                            "name": "Album 1",
                            "images": [{"url": "https://img/al1.jpg", "width": 640, "height": 640}]
                        }
                    },
                    {
                        "id": "3333333333333333333333",
                        "uri": "spotify:track:3333333333333333333333",
                        "name": "Third Track",
                        "duration_ms": 200000,
                        "is_playable": true,
                        "artists": [{"id": "a2", "uri": "spotify:artist:a2", "name": "A2"}],
                        "album": {
                            "id": "al2",
                            "uri": "spotify:album:al2",
                            "name": "Album 2",
                            "images": []
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/episodes"))
            .and(query_param("ids", "2222222222222222222222"))
            .and(query_param("market", "from_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "episodes": [
                    {
                        "id": "2222222222222222222222",
                        "uri": "spotify:episode:2222222222222222222222",
                        "name": "Middle Episode",
                        "description": "<p>Podcast</p>",
                        "duration_ms": 900000,
                        "is_playable": true,
                        "show": {
                            "id": "sh1",
                            "uri": "spotify:show:sh1",
                            "name": "Tech Show",
                            "publisher": "Pub",
                            "images": [{"url": "https://img/sh1.jpg", "width": 640, "height": 640}]
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let combined = client
            .tracks_and_episodes(&[
                "spotify:track:1111111111111111111111",
                "spotify:episode:2222222222222222222222",
                "spotify:track:3333333333333333333333",
            ])
            .await
            .expect("tracks_and_episodes must succeed");

        assert_eq!(combined.len(), 3, "expected 3 combined items: {combined:?}");
        assert_eq!(combined[0].id, "1111111111111111111111");
        assert_eq!(combined[1].id, "2222222222222222222222");
        assert_eq!(combined[1].album.album_type, "show");
        assert_eq!(combined[2].id, "3333333333333333333333");
    }

    #[tokio::test]
    async fn album_auto_paginates_when_total_tracks_exceeds_50() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let mut first_50 = Vec::with_capacity(50);
        for idx in 1..=50_u32 {
            first_50.push(json!({
                "id": format!("trk{idx:019}"),
                "uri": format!("spotify:track:trk{idx:019}"),
                "name": format!("Track {idx}"),
                "track_number": idx,
                "duration_ms": 120000,
                "is_playable": true,
                "artists": [{"id": "art1", "uri": "spotify:artist:art1", "name": "Artist"}]
            }));
        }
        let mut next_25 = Vec::with_capacity(25);
        for idx in 51..=75_u32 {
            next_25.push(json!({
                "id": format!("trk{idx:019}"),
                "uri": format!("spotify:track:trk{idx:019}"),
                "name": format!("Track {idx}"),
                "track_number": idx,
                "duration_ms": 120000,
                "is_playable": true,
                "artists": [{"id": "art1", "uri": "spotify:artist:art1", "name": "Artist"}]
            }));
        }

        Mock::given(method("GET"))
            .and(path("/v1/albums/alb0000000000000000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "alb0000000000000000001",
                "uri": "spotify:album:alb0000000000000000001",
                "name": "Long Audiobook",
                "album_type": "album",
                "release_date": "2026-01-01",
                "total_tracks": 75,
                "artists": [{"id": "art1", "uri": "spotify:artist:art1", "name": "Artist"}],
                "images": [{"url": "https://img/alb.jpg", "width": 640, "height": 640}],
                "tracks": {
                    "total": 75,
                    "items": first_50
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let next_url = format!(
            "{}/v1/albums/alb0000000000000000001/tracks?offset=50&limit=50",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/v1/albums/alb0000000000000000001/tracks"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 75,
                "next": next_url,
                "items": first_50
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/albums/alb0000000000000000001/tracks"))
            .and(query_param("limit", "50"))
            .and(query_param("offset", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 75,
                "next": null,
                "items": next_25
            })))
            .expect(1)
            .mount(&server)
            .await;

        let album = client
            .album("spotify:album:alb0000000000000000001")
            .await
            .expect("75-track album fetch must succeed");
        assert_eq!(
            album.tracks.len(),
            75,
            "expected 75 tracks: {}",
            album.tracks.len()
        );
        assert_eq!(album.tracks[0].album.name, "Long Audiobook");
        assert_eq!(album.tracks[74].name, "Track 75");
        assert_eq!(album.tracks[74].album.image, "https://img/alb.jpg");
    }

    #[tokio::test]
    async fn player_handles_204_and_track_cached_uses_dual_cache() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/me/player"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let state = client.player().await.expect("player 204 must succeed");
        assert!(
            state.is_none(),
            "expected None for 204 No Content: {state:?}"
        );

        Mock::given(method("GET"))
            .and(path("/v1/tracks/trk0000000000000000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "trk0000000000000000001",
                "uri": "spotify:track:trk0000000000000000001",
                "name": "Cached Hit",
                "duration_ms": 150000,
                "is_playable": true,
                "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "A1"}],
                "album": {"id": "al1", "uri": "spotify:album:al1", "name": "Al1", "images": []}
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert!(
            client
                .track_cached(
                    "spotify:track:trk0000000000000000001",
                    TrackCacheLookup::CacheOnly
                )
                .await
                .expect("cache check")
                .is_none(),
            "cache should initially be empty"
        );

        let fetched = client
            .track_cached(
                "spotify:track:trk0000000000000000001",
                TrackCacheLookup::FetchIfMissing,
            )
            .await
            .expect("fetch on miss")
            .expect("track returned");
        assert_eq!(fetched.name, "Cached Hit");

        let from_cache = client
            .track_cached(
                "spotify:track:trk0000000000000000001",
                TrackCacheLookup::CacheOnly,
            )
            .await
            .expect("cache hit")
            .expect("track in cache");
        assert_eq!(from_cache.id, "trk0000000000000000001");
    }

    #[tokio::test]
    async fn search_filters_unplayable_tracks() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "track"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": {
                    "total": 2,
                    "next": null,
                    "items": [
                        {
                            "id": "t1",
                            "uri": "spotify:track:t1",
                            "name": "Playable",
                            "is_playable": true,
                            "artists": [],
                            "album": {
                                "id": "a1",
                                "uri": "spotify:album:a1",
                                "name": "A1",
                                "images": []
                            }
                        },
                        {
                            "id": "t2",
                            "uri": "spotify:track:t2",
                            "name": "Local",
                            "is_local": true,
                            "artists": [],
                            "album": {
                                "id": "a1",
                                "uri": "spotify:album:a1",
                                "name": "A1",
                                "images": []
                            }
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let res = client
            .search("hello", SearchKind::Track, 50, 50)
            .await
            .expect("search must succeed");
        assert_eq!(res.tracks.len(), 1, "expected local track filtered out");
        assert_eq!(res.tracks[0].id, "t1");
    }
}
