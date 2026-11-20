//! Spotify Web API v1 user library, saved media, and playlist endpoints.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use futures_util::future::try_join_all;
use md5::Md5;
use reqwest::Method;
use serde_json::{json, Value};
use spotty_cache::MetadataWriteMode;
use spotty_types::{Result, SortOrder, SpotifyEntityKind, SpottyError};
use tokio::sync::Semaphore;

use crate::catalog::{
    build_id_chunks, encode_user_segment, extract_entity_id, is_playable_item, usize_field,
    SpotifyClient, SPOTIFY_LIMIT,
};
use crate::models::{
    NormalizedAlbum, NormalizedArtist, NormalizedPlaylist, NormalizedShow, NormalizedTrack,
};
use crate::normalize::strip_leading_articles;
use crate::pipeline::{CursorPage, OffsetPage};
use crate::transport::CachePolicy;

/// Spotify playlist tracks page limit (`100`).
pub const SPOTIFY_PLAYLIST_TRACKS_LIMIT: usize = 100;

/// Maximum playlist tracks retrieved before halting pagination (`10_000`).
pub const SPOTIFY_PLAYLIST_MAX_ITEMS: usize = 10_000;

/// Controls whether [`SpotifyClient::my_albums`] hydrates albums exceeding
/// 50 tracks.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlbumHydration {
    /// Keep the embedded track slice returned by `/v1/me/albums`.
    SummaryOnly,
    /// Fetch remaining tracks from `/v1/albums/{id}/tracks` when `total > 50`.
    HydrateLargeAlbums,
}

/// Controls whether [`SpotifyClient::my_artists`] includes album artists.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtistsScope {
    /// Return only artists from `/v1/me/following?type=artist`.
    FollowedOnly,
    /// Merge followed artists with primary artists from saved `/v1/me/albums`.
    IncludeAlbumArtists,
}

/// Track ordering applied by [`SpotifyClient::playlist_tracks`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaylistTrackSort {
    /// Preserve the playlist order returned by Spotify.
    #[default]
    ApiOrder,
    /// Sort newest `added_at` entries first (`API.pm:487`).
    AddedAtDescending,
}

/// Summary metadata returned by [`SpotifyClient::my_albums_meta`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryAlbumsMeta {
    /// Total saved albums in the user's library.
    pub total: u64,
    /// Most recent album `added_at` timestamp, if any.
    pub latest_added_at: Option<String>,
}

/// Result returned by [`SpotifyClient::my_artists`] including scanner MD5 hash.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtistsLibrary {
    /// Deduplicated normalized artists.
    pub artists: Vec<NormalizedArtist>,
    /// Lowercase hex MD5 over `|`-joined sorted artist IDs (`Importer.pm:414`).
    pub md5_hash: String,
}

/// Options controlling [`SpotifyClient::playlist_tracks`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistTracksOptions {
    /// Optional Spotify `fields=` projection string for lightweight scans.
    pub fields: Option<String>,
    /// Track sorting mode.
    pub sort: PlaylistTrackSort,
    /// Maximum items to retrieve (clamped to [`SPOTIFY_PLAYLIST_MAX_ITEMS`]).
    pub max_items: usize,
}

impl Default for PlaylistTracksOptions {
    fn default() -> Self {
        Self {
            fields: None,
            sort: PlaylistTrackSort::ApiOrder,
            max_items: SPOTIFY_PLAYLIST_MAX_ITEMS,
        }
    }
}

impl PlaylistTracksOptions {
    /// Creates default [`PlaylistTracksOptions`] (full tracks, API order,
    /// `10_000` max items).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets an optional Spotify `fields=` projection query parameter.
    #[must_use]
    pub fn with_fields(mut self, fields: impl Into<String>) -> Self {
        self.fields = Some(fields.into());
        self
    }

    /// Sets the playlist track sort mode.
    #[must_use]
    pub fn with_sort(mut self, sort: PlaylistTrackSort) -> Self {
        self.sort = sort;
        self
    }

    /// Sets the maximum number of playlist items to retrieve.
    #[must_use]
    pub fn with_max_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items.min(SPOTIFY_PLAYLIST_MAX_ITEMS);
        self
    }
}

/// Computes the scanner watermark hash: lowercase hex MD5 of sorted artist IDs
/// joined by `|` (`Importer.pm:414`).
#[must_use]
pub fn compute_artists_md5_hash(artists: &[NormalizedArtist]) -> String {
    use md5::Digest as _;

    let mut ids = Vec::with_capacity(artists.len());
    for artist in artists {
        if !artist.id.is_empty() {
            ids.push(artist.id.as_str());
        }
    }
    ids.sort_unstable();
    let joined = ids.join("|");

    let mut hasher = Md5::new();
    hasher.update(joined.as_bytes());
    let digest = hasher.finalize();

    let mut hex = String::with_capacity(32);
    for byte in digest {
        let _ = fmt::write(&mut hex, format_args!("{byte:02x}"));
    }
    hex
}

impl SpotifyClient {
    /// Fetches saved tracks (`/v1/me/tracks`) via [`crate::Pipeline`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn my_songs(
        &self,
        write_mode: MetadataWriteMode,
        sort: SortOrder,
        max_items: usize,
    ) -> Result<Vec<NormalizedTrack>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let normalizer = self.normalizer();
        let cache = self.cache();
        let params = [(String::from("market"), String::from("from_token"))];

        let mut tracks = self
            .pipeline()
            .fetch_offset_parallel(
                "me/tracks",
                &params,
                SPOTIFY_LIMIT,
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
                    for entry in raw_items {
                        let Some(track_val) = entry.get("track") else {
                            continue;
                        };
                        if !track_val.is_object() {
                            continue;
                        }
                        let mut track =
                            normalizer.normalize_track(track_val, None, cache, write_mode)?;
                        if track.added_at.is_none() {
                            track.added_at = entry
                                .get("added_at")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        out.push(track);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await?;

        if sort == SortOrder::Alphabetical {
            let articles = self.article_slices();
            tracks.sort_by(|a, b| {
                let sa = strip_leading_articles(&a.name, &articles).to_lowercase();
                let sb = strip_leading_articles(&b.name, &articles).to_lowercase();
                sa.cmp(&sb)
            });
        }
        Ok(tracks)
    }

    /// Fetches saved albums (`/v1/me/albums`), optionally hydrating albums with
    /// `total_tracks > 50` for the library scanner.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn my_albums(
        &self,
        write_mode: MetadataWriteMode,
        hydration: AlbumHydration,
        sort: SortOrder,
        max_items: usize,
    ) -> Result<Vec<NormalizedAlbum>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let normalizer = self.normalizer();
        let cache = self.cache();
        let params = [(String::from("market"), String::from("from_token"))];

        let mut albums = self
            .pipeline()
            .fetch_offset_parallel(
                "me/albums",
                &params,
                SPOTIFY_LIMIT,
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
                    for entry in raw_items {
                        let Some(album_val) = entry.get("album") else {
                            continue;
                        };
                        if !album_val.is_object() {
                            continue;
                        }
                        let mut album = normalizer.normalize_album(album_val, cache, write_mode)?;
                        if album.added_at.is_none() {
                            album.added_at = entry
                                .get("added_at")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        out.push(album);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await?;

        if hydration == AlbumHydration::HydrateLargeAlbums {
            let to_hydrate: Vec<(usize, NormalizedAlbum)> = albums
                .iter()
                .enumerate()
                .filter(|(_, album)| {
                    let total = usize::try_from(album.total_tracks).unwrap_or(0);
                    total > SPOTIFY_LIMIT && album.tracks.len() < total && !album.id.is_empty()
                })
                .map(|(idx, album)| (idx, album.clone()))
                .collect();

            if !to_hydrate.is_empty() {
                let sem = Arc::new(Semaphore::new(self.pipeline().max_concurrency()));
                let futures = to_hydrate.into_iter().map(|(idx, mut album)| {
                    let sem = Arc::clone(&sem);
                    async move {
                        let _permit = sem
                            .acquire()
                            .await
                            .map_err(|err| SpottyError::Io(err.to_string()))?;
                        let total = usize::try_from(album.total_tracks).unwrap_or(0);
                        self.hydrate_album_remaining_tracks(&mut album, total)
                            .await?;
                        Ok::<(usize, Vec<NormalizedTrack>), SpottyError>((idx, album.tracks))
                    }
                });

                for (idx, tracks) in try_join_all(futures).await? {
                    albums[idx].tracks = tracks;
                }
            }
        }

        if sort == SortOrder::Alphabetical {
            let articles = self.article_slices();
            albums.sort_by(|a, b| {
                let sa = strip_leading_articles(&a.name, &articles).to_lowercase();
                let sb = strip_leading_articles(&b.name, &articles).to_lowercase();
                sa.cmp(&sb)
            });
        }

        Ok(albums)
    }

    /// Probes `/v1/me/albums?limit=1` for total count and latest `added_at`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport or JSON failure.
    pub async fn my_albums_meta(&self) -> Result<LibraryAlbumsMeta> {
        let params = [(String::from("limit"), String::from("1"))];
        let resp = self
            .transport()
            .get("me/albums", &params, CachePolicy::UseCache)
            .await?;
        let json: Value = resp.json()?;
        let total = json.get("total").and_then(Value::as_u64).unwrap_or(0);
        let latest_added_at = json
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|first| first.get("added_at"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        Ok(LibraryAlbumsMeta {
            total,
            latest_added_at,
        })
    }

    /// Checks whether each album ID/URI is saved (`/v1/me/albums/contains`) in
    /// 50-ID chunks.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid album ID/URI, or
    /// [`SpottyError`] on transport or JSON failure.
    pub async fn is_in_my_albums(&self, ids_or_uris: &[&str]) -> Result<HashMap<String, bool>> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Album)?);
        }
        if clean_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let chunks = build_id_chunks(&clean_ids, SPOTIFY_LIMIT, false);
        let chunk_results = self
            .pipeline()
            .execute_indexed_chunks(
                "me/albums/contains",
                &chunks,
                CachePolicy::UseCache,
                |json| {
                    let Some(arr) = json.as_array() else {
                        return Err(SpottyError::Json(String::from(
                            "expected JSON boolean array from me/albums/contains",
                        )));
                    };
                    let mut flags = Vec::with_capacity(arr.len());
                    for item in arr {
                        flags.push(item.as_bool().unwrap_or(false));
                    }
                    Ok(vec![flags])
                },
            )
            .await?;

        let mut out = HashMap::with_capacity(clean_ids.len());
        for (chunk_ids, flags) in clean_ids.chunks(SPOTIFY_LIMIT).zip(chunk_results) {
            for (id, flag) in chunk_ids.iter().zip(flags) {
                out.insert(id.clone(), flag);
            }
        }
        Ok(out)
    }

    /// Saves one or more albums to the user's library (`PUT /v1/me/albums`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid album ID/URI, or
    /// [`SpottyError`] on transport failure.
    pub async fn add_album_to_library(&self, ids_or_uris: &[&str]) -> Result<()> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Album)?);
        }
        for chunk in clean_ids.chunks(SPOTIFY_LIMIT) {
            let params = [(String::from("ids"), chunk.join(","))];
            self.transport()
                .request(
                    Method::PUT,
                    "me/albums",
                    &params,
                    None,
                    CachePolicy::BypassCache,
                )
                .await?;
        }
        Ok(())
    }

    /// Fetches followed artists via cursor pagination (`/v1/me/following`),
    /// optionally merging album artists and computing the sorted MD5 watermark.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn my_artists(&self, scope: ArtistsScope, sort: SortOrder) -> Result<ArtistsLibrary> {
        let normalizer = self.normalizer();
        let cache = self.cache();
        let articles = self.article_slices();
        let params = [
            (String::from("type"), String::from("artist")),
            (String::from("limit"), SPOTIFY_LIMIT.to_string()),
        ];

        let mut artists = self
            .pipeline()
            .fetch_cursor_serial("me/following", &params, CachePolicy::UseCache, |json| {
                let Some(artists_obj) = json.get("artists").and_then(Value::as_object) else {
                    return Ok(CursorPage::new(Vec::new(), None, None));
                };
                let next = artists_obj
                    .get("next")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let after = artists_obj
                    .get("cursors")
                    .and_then(Value::as_object)
                    .and_then(|c| c.get("after"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let raw_items = artists_obj
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);

                let mut items = Vec::with_capacity(raw_items.len());
                for raw in raw_items {
                    if !raw.is_object() {
                        continue;
                    }
                    let artist = normalizer.normalize_artist(raw, cache, &articles)?;
                    if !artist.id.is_empty() {
                        items.push(artist);
                    }
                }
                Ok(CursorPage::new(items, next, after))
            })
            .await?;

        let mut known_ids = HashSet::with_capacity(artists.len());
        for artist in &artists {
            known_ids.insert(artist.id.clone());
        }

        if scope == ArtistsScope::IncludeAlbumArtists {
            let albums = self
                .my_albums(
                    MetadataWriteMode::FastSkipIfPresent,
                    AlbumHydration::SummaryOnly,
                    SortOrder::NaturalOrder,
                    SPOTIFY_PLAYLIST_MAX_ITEMS,
                )
                .await?;
            let mut missing_artwork_ids = Vec::new();
            for album in albums {
                let Some(primary) = album.artists.first() else {
                    continue;
                };
                if primary.id.is_empty() || !known_ids.insert(primary.id.clone()) {
                    continue;
                }
                let summary_val = json!({
                    "id": primary.id,
                    "uri": primary.uri,
                    "name": primary.name,
                });
                let normalized = normalizer.normalize_artist(&summary_val, cache, &articles)?;
                if normalized.image.is_empty() {
                    missing_artwork_ids.push(normalized.id.clone());
                }
                artists.push(normalized);
            }

            if !missing_artwork_ids.is_empty() {
                let mut id_refs = Vec::with_capacity(missing_artwork_ids.len());
                for id in &missing_artwork_ids {
                    id_refs.push(id.as_str());
                }
                let detailed = self.artists_batch(&id_refs).await?;
                let mut by_id = HashMap::with_capacity(detailed.len());
                for artist in detailed {
                    by_id.insert(artist.id.clone(), artist);
                }
                for artist in &mut artists {
                    if let Some(full) = by_id.get(&artist.id) {
                        if artist.image.is_empty() {
                            artist.image = full.image.clone();
                        }
                        if artist.followers_total == 0 {
                            artist.followers_total = full.followers_total;
                        }
                        if artist.genres.is_empty() {
                            artist.genres = full.genres.clone();
                        }
                    }
                }
            }
        }

        if sort == SortOrder::Alphabetical {
            artists.sort_by_key(|a| a.sortname.to_lowercase());
        }

        let md5_hash = compute_artists_md5_hash(&artists);
        Ok(ArtistsLibrary { artists, md5_hash })
    }

    /// Follows one or more artists (`PUT /v1/me/following?type=artist`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid artist ID/URI, or
    /// [`SpottyError`] on transport failure.
    pub async fn follow_artist(&self, ids_or_uris: &[&str]) -> Result<()> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Artist)?);
        }
        for chunk in clean_ids.chunks(SPOTIFY_LIMIT) {
            let params = [
                (String::from("type"), String::from("artist")),
                (String::from("ids"), chunk.join(",")),
            ];
            self.transport()
                .request(
                    Method::PUT,
                    "me/following",
                    &params,
                    None,
                    CachePolicy::BypassCache,
                )
                .await?;
        }
        Ok(())
    }

    /// Fetches saved podcast shows (`/v1/me/shows`) sorted alphabetically.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn my_shows(&self, max_items: usize) -> Result<Vec<NormalizedShow>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let normalizer = self.normalizer();
        let cache = self.cache();
        let mut shows = self
            .pipeline()
            .fetch_offset_parallel(
                "me/shows",
                &[],
                SPOTIFY_LIMIT,
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
                    for entry in raw_items {
                        let Some(show_val) = entry.get("show") else {
                            continue;
                        };
                        if !show_val.is_object() {
                            continue;
                        }
                        out.push(normalizer.normalize_show(
                            show_val,
                            cache,
                            MetadataWriteMode::MergeExisting,
                        )?);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await?;

        shows.sort_by_key(|a| a.name.to_lowercase());
        Ok(shows)
    }

    /// Saves one or more podcast shows to the library (`PUT /v1/me/shows`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid show ID/URI, or
    /// [`SpottyError`] on transport failure.
    pub async fn add_show_to_library(&self, ids_or_uris: &[&str]) -> Result<()> {
        let mut clean_ids = Vec::with_capacity(ids_or_uris.len());
        for raw in ids_or_uris {
            if raw.trim().is_empty() {
                continue;
            }
            clean_ids.push(extract_entity_id(raw, SpotifyEntityKind::Show)?);
        }
        for chunk in clean_ids.chunks(SPOTIFY_LIMIT) {
            let params = [(String::from("ids"), chunk.join(","))];
            self.transport()
                .request(
                    Method::PUT,
                    "me/shows",
                    &params,
                    None,
                    CachePolicy::BypassCache,
                )
                .await?;
        }
        Ok(())
    }

    /// Fetches tracks (and podcast episodes) in a playlist (`100`/page up to
    /// `10_000`) with optional `fields` projection and `added_at` sorting.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid playlist ID/URI, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn playlist_tracks(
        &self,
        uri_or_id: &str,
        options: &PlaylistTracksOptions,
    ) -> Result<Vec<NormalizedTrack>> {
        if options.max_items == 0 {
            return Ok(Vec::new());
        }
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Playlist)?;
        let path = format!("playlists/{id}/tracks");
        let mut params = vec![(String::from("market"), String::from("from_token"))];
        if let Some(fields) = &options.fields {
            if !fields.trim().is_empty() {
                params.push((String::from("fields"), fields.clone()));
            }
        }

        let cap = options.max_items.min(SPOTIFY_PLAYLIST_MAX_ITEMS);
        let normalizer = self.normalizer();
        let cache = self.cache();
        let country = self.country();

        let mut tracks = self
            .pipeline()
            .fetch_offset_parallel(
                &path,
                &params,
                SPOTIFY_PLAYLIST_TRACKS_LIMIT,
                cap,
                CachePolicy::UseCache,
                |json| {
                    let total = usize_field(json.get("total"));
                    let next = json.get("next").and_then(Value::as_str).map(str::to_owned);
                    let raw_items = json
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut out = Vec::with_capacity(raw_items.len());
                    for entry in raw_items {
                        let Some(track_val) = entry.get("track") else {
                            continue;
                        };
                        if !track_val.is_object() || !is_playable_item(track_val, country) {
                            continue;
                        }
                        if track_val.get("restrictions").is_some_and(|r| !r.is_null()) {
                            continue;
                        }
                        let added_at = entry
                            .get("added_at")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let item_type = track_val.get("type").and_then(Value::as_str).unwrap_or("");
                        let item_uri = track_val.get("uri").and_then(Value::as_str).unwrap_or("");
                        let mut normalized =
                            if item_type == "episode" || item_uri.starts_with("spotify:episode:") {
                                normalizer
                                    .normalize_episode(
                                        track_val,
                                        None,
                                        cache,
                                        MetadataWriteMode::MergeExisting,
                                    )?
                                    .to_track_view()
                            } else {
                                normalizer.normalize_track(
                                    track_val,
                                    None,
                                    cache,
                                    MetadataWriteMode::MergeExisting,
                                )?
                            };
                        if normalized.added_at.is_none() {
                            normalized.added_at = added_at;
                        }
                        out.push(normalized);
                    }
                    Ok(OffsetPage::new(out, total, next))
                },
            )
            .await?;

        if options.sort == PlaylistTrackSort::AddedAtDescending {
            tracks.sort_by(|a, b| b.added_at.cmp(&a.added_at));
        }
        Ok(tracks)
    }

    /// Scans only playable track/episode URIs for a playlist using a minimal
    /// `fields` projection (`next,total,items(track(uri,restrictions,is_local))`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid playlist ID/URI, or
    /// [`SpottyError`] on transport or JSON failure.
    pub async fn playlist_track_uris(&self, uri_or_id: &str) -> Result<Vec<String>> {
        let id = extract_entity_id(uri_or_id, SpotifyEntityKind::Playlist)?;
        let path = format!("playlists/{id}/tracks");
        let params = [
            (String::from("market"), String::from("from_token")),
            (
                String::from("fields"),
                String::from("next,total,items(track(uri,restrictions,is_local))"),
            ),
        ];

        self.pipeline()
            .fetch_offset_parallel(
                &path,
                &params,
                SPOTIFY_PLAYLIST_TRACKS_LIMIT,
                SPOTIFY_PLAYLIST_MAX_ITEMS,
                CachePolicy::UseCache,
                |json| {
                    let total = usize_field(json.get("total"));
                    let next = json.get("next").and_then(Value::as_str).map(str::to_owned);
                    let raw_items = json
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut uris = Vec::with_capacity(raw_items.len());
                    for entry in raw_items {
                        let Some(track_obj) = entry.get("track").and_then(Value::as_object) else {
                            continue;
                        };
                        if track_obj.get("is_local").and_then(Value::as_bool) == Some(true) {
                            continue;
                        }
                        if track_obj.get("restrictions").is_some_and(|r| !r.is_null()) {
                            continue;
                        }
                        let Some(uri) = track_obj.get("uri").and_then(Value::as_str) else {
                            continue;
                        };
                        if uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")
                        {
                            uris.push(uri.to_owned());
                        }
                    }
                    Ok(OffsetPage::new(uris, total, next))
                },
            )
            .await
    }

    /// Fetches playlists owned by or public for `user_id`
    /// (`/v1/users/{user}/playlists`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `user_id` is empty, or
    /// [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn user_playlists(
        &self,
        user_id: &str,
        max_items: usize,
    ) -> Result<Vec<NormalizedPlaylist>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        let trimmed = user_id.trim();
        if trimmed.is_empty() {
            return Err(SpottyError::InvalidUri(String::from(
                "user ID must not be empty",
            )));
        }
        let encoded_user = encode_user_segment(trimmed);
        let path = format!("users/{encoded_user}/playlists");
        self.fetch_playlists_at(&path, max_items).await
    }

    /// Fetches the current user's playlists (`/v1/me/playlists`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] on transport, JSON, or cache failure.
    pub async fn my_playlists(&self, max_items: usize) -> Result<Vec<NormalizedPlaylist>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }
        self.fetch_playlists_at("me/playlists", max_items).await
    }

    async fn fetch_playlists_at(
        &self,
        path: &str,
        max_items: usize,
    ) -> Result<Vec<NormalizedPlaylist>> {
        let normalizer = self.normalizer();
        let cache = self.cache();
        self.pipeline()
            .fetch_offset_parallel(
                path,
                &[],
                SPOTIFY_LIMIT,
                max_items,
                CachePolicy::UseCache,
                |json| {
                    let total = usize_field(json.get("total"));
                    let next = json.get("next").and_then(Value::as_str).map(str::to_owned);
                    let raw_items = json
                        .get("items")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    let mut playlists = Vec::with_capacity(raw_items.len());
                    for raw in raw_items {
                        if !raw.is_object() {
                            continue;
                        }
                        playlists.push(normalizer.normalize_playlist(raw, cache)?);
                    }
                    Ok(OffsetPage::new(playlists, total, next))
                },
            )
            .await
    }

    /// Appends track or episode URIs to a playlist in 100-URI chunks
    /// (`POST /v1/playlists/{id}/tracks`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] on an invalid playlist ID/URI, or
    /// [`SpottyError`] on transport or JSON failure.
    pub async fn add_tracks_to_playlist(
        &self,
        playlist_uri_or_id: &str,
        track_uris: &[&str],
    ) -> Result<()> {
        if track_uris.is_empty() {
            return Ok(());
        }
        let playlist_id = extract_entity_id(playlist_uri_or_id, SpotifyEntityKind::Playlist)?;
        let path = format!("playlists/{playlist_id}/tracks");
        for chunk in track_uris.chunks(SPOTIFY_PLAYLIST_TRACKS_LIMIT) {
            let body = serde_json::to_string(&json!({ "uris": chunk }))?;
            self.transport()
                .request(
                    Method::POST,
                    &path,
                    &[],
                    Some(body),
                    CachePolicy::BypassCache,
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spotty_cache::MetadataWriteMode;
    use spotty_types::SortOrder;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        compute_artists_md5_hash, AlbumHydration, ArtistsScope, PlaylistTrackSort,
        PlaylistTracksOptions,
    };
    use crate::catalog::test_support::build_test_client;
    use crate::models::NormalizedArtist;

    #[test]
    fn compute_artists_md5_hash_sorts_ids_and_joins_with_pipe() {
        use md5::Digest as _;

        let a1 = NormalizedArtist {
            id: String::from("zArtist"),
            ..Default::default()
        };
        let a2 = NormalizedArtist {
            id: String::from("aArtist"),
            ..Default::default()
        };
        let hash = compute_artists_md5_hash(&[a1, a2]);
        let expected_digest = md5::Md5::digest(b"aArtist|zArtist");
        let mut expected = String::with_capacity(32);
        for byte in expected_digest {
            let _ = std::fmt::Write::write_fmt(&mut expected, format_args!("{byte:02x}"));
        }
        assert_eq!(hash, expected);
    }

    #[tokio::test]
    async fn my_artists_merges_followed_and_album_artists_and_computes_hash() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "items": [
                        {
                            "id": "artB",
                            "uri": "spotify:artist:artB",
                            "name": "The Beta",
                            "images": [{"url": "https://img/b.jpg", "width": 640, "height": 640}]
                        }
                    ],
                    "cursors": {"after": null},
                    "next": null
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [
                    {
                        "added_at": "2026-01-01T00:00:00Z",
                        "album": {
                            "id": "alb1",
                            "uri": "spotify:album:alb1",
                            "name": "Alpha Album",
                            "total_tracks": 1,
                            "artists": [{
                                "id": "artA",
                                "uri": "spotify:artist:artA",
                                "name": "Alpha"
                            }],
                            "images": []
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/artists"))
            .and(query_param("ids", "artA"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": [
                    {
                        "id": "artA",
                        "uri": "spotify:artist:artA",
                        "name": "Alpha",
                        "images": [{"url": "https://img/a.jpg", "width": 640, "height": 640}]
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let res = client
            .my_artists(ArtistsScope::IncludeAlbumArtists, SortOrder::Alphabetical)
            .await
            .expect("my_artists must succeed");
        assert_eq!(res.artists.len(), 2);
        assert_eq!(res.artists[0].id, "artA");
        assert_eq!(res.artists[0].image, "https://img/a.jpg");
        assert_eq!(res.artists[1].id, "artB");

        let expected = compute_artists_md5_hash(&res.artists);
        assert_eq!(res.md5_hash, expected);
    }

    #[tokio::test]
    async fn playlist_tracks_sorts_by_added_at_descending_and_supports_uri_scan() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/playlists/pl00000000000000000001/tracks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "next": null,
                "items": [
                    {
                        "added_at": "2026-01-01T10:00:00Z",
                        "track": {
                            "id": "t_old",
                            "uri": "spotify:track:t_old",
                            "name": "Older",
                            "is_playable": true,
                            "artists": [],
                            "album": {
                                "id": "a1",
                                "uri": "spotify:album:a1",
                                "name": "Al",
                                "images": []
                            }
                        }
                    },
                    {
                        "added_at": "2026-02-01T10:00:00Z",
                        "track": {
                            "id": "t_new",
                            "uri": "spotify:track:t_new",
                            "name": "Newer",
                            "is_playable": true,
                            "artists": [],
                            "album": {
                                "id": "a1",
                                "uri": "spotify:album:a1",
                                "name": "Al",
                                "images": []
                            }
                        }
                    }
                ]
            })))
            .mount(&server)
            .await;

        let opts = PlaylistTracksOptions::new().with_sort(PlaylistTrackSort::AddedAtDescending);
        let sorted = client
            .playlist_tracks("spotify:playlist:pl00000000000000000001", &opts)
            .await
            .expect("playlist_tracks must succeed");
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].id, "t_new");
        assert_eq!(sorted[1].id, "t_old");

        let uris = client
            .playlist_track_uris("pl00000000000000000001")
            .await
            .expect("playlist_track_uris must succeed");
        assert_eq!(
            uris,
            vec![
                String::from("spotify:track:t_old"),
                String::from("spotify:track:t_new")
            ]
        );
    }

    #[tokio::test]
    async fn is_in_my_albums_and_my_albums_meta_and_summary() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/me/albums/contains"))
            .and(query_param("ids", "alb1,alb2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([true, false])))
            .expect(1)
            .mount(&server)
            .await;

        let map = client
            .is_in_my_albums(&["spotify:album:alb1", "alb2"])
            .await
            .expect("is_in_my_albums must succeed");
        assert_eq!(map.get("alb1"), Some(&true));
        assert_eq!(map.get("alb2"), Some(&false));

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 42,
                "items": [{"added_at": "2026-11-18T12:00:00Z", "album": {"id": "alb1"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let meta = client
            .my_albums_meta()
            .await
            .expect("my_albums_meta must succeed");
        assert_eq!(meta.total, 42);
        assert_eq!(
            meta.latest_added_at.as_deref(),
            Some("2026-11-18T12:00:00Z")
        );

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "added_at": "2026-11-18T12:00:00Z",
                    "album": {
                        "id": "alb1",
                        "uri": "spotify:album:alb1",
                        "name": "Saved",
                        "total_tracks": 1,
                        "artists": [],
                        "images": []
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let albums = client
            .my_albums(
                MetadataWriteMode::MergeExisting,
                AlbumHydration::SummaryOnly,
                SortOrder::NaturalOrder,
                50,
            )
            .await
            .expect("my_albums must succeed");
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].added_at.as_deref(), Some("2026-11-18T12:00:00Z"));
    }

    #[tokio::test]
    async fn my_albums_hydrates_large_albums_concurrently_from_offset_50() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir");
        let client = build_test_client(&server.uri(), &tmp);

        let make_tracks = |start: u32, end: u32| -> Vec<serde_json::Value> {
            (start..=end)
                .map(|idx| {
                    json!({
                        "id": format!("trk{idx:019}"),
                        "uri": format!("spotify:track:trk{idx:019}"),
                        "name": format!("Track {idx}"),
                        "track_number": idx,
                        "duration_ms": 1000,
                        "is_playable": true,
                        "artists": [{"id": "art1", "uri": "spotify:artist:art1", "name": "A"}]
                    })
                })
                .collect()
        };

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "next": null,
                "items": [
                    {
                        "added_at": "2026-11-18T12:00:00Z",
                        "album": {
                            "id": "alb0000000000000000001",
                            "uri": "spotify:album:alb0000000000000000001",
                            "name": "Large 1",
                            "total_tracks": 60,
                            "artists": [],
                            "images": [],
                            "tracks": { "total": 60, "items": make_tracks(1, 50) }
                        }
                    },
                    {
                        "added_at": "2026-11-18T12:00:00Z",
                        "album": {
                            "id": "alb0000000000000000002",
                            "uri": "spotify:album:alb0000000000000000002",
                            "name": "Large 2",
                            "total_tracks": 55,
                            "artists": [],
                            "images": [],
                            "tracks": { "total": 55, "items": make_tracks(1, 50) }
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        for (alb_id, count) in [
            ("alb0000000000000000001", 60u32),
            ("alb0000000000000000002", 55u32),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/v1/albums/{alb_id}/tracks")))
                .and(query_param("offset", "50"))
                .and(query_param("limit", "50"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "total": count,
                    "next": null,
                    "items": make_tracks(51, count)
                })))
                .expect(1)
                .mount(&server)
                .await;
        }

        let albums = client
            .my_albums(
                MetadataWriteMode::MergeExisting,
                AlbumHydration::HydrateLargeAlbums,
                SortOrder::NaturalOrder,
                50,
            )
            .await
            .expect("my_albums concurrent hydration must succeed");
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].tracks.len(), 60);
        assert_eq!(albums[1].tracks.len(), 55);
    }
}
