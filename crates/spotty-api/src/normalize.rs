//! Normalization of Spotify Web API v1 JSON payloads into typed models.
//!
//! Ports `Plugins::Spotty::API::Cache` normalization, HTML stripping, leading
//! article removal, artwork selection, and multi-tier `DualCache` persistence.

use std::time::Duration;

use serde_json::{Map, Value};
use spotty_cache::{
    cleanup_tags, largest_artwork_url, ArtworkImage, DualCache, MetadataWriteMode,
    EPISODE_ARTIST_TTL_SECS, PLAYLIST_OWNER_TTL_SECS,
};
use spotty_types::{Result, SpottyError, TagCleanupPolicy};

use crate::models::{
    ArtistSummary, BrowseCategory, LinkedFrom, MinAlbum, NormalizedAlbum, NormalizedArtist,
    NormalizedEpisode, NormalizedPlaylist, NormalizedShow, NormalizedTrack, PlayerContextState,
    PlayerDevice, SearchResults, UserProfile,
};

/// Removes HTML tags (`<.+?>`) from Spotify description strings.
///
/// Matches `API/Cache.pm` (`s/<.+?>//g`) without matching empty `<>` or
/// spanning across newline boundaries.
#[must_use]
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut idx = 0;

    while let Some(ch) = s[idx..].chars().next() {
        if ch == '<' {
            let after_open = idx + ch.len_utf8();
            if let Some(close_rel) = find_tag_close(&s[after_open..]) {
                idx = after_open + close_rel + 1;
                continue;
            }
        }
        out.push(ch);
        idx += ch.len_utf8();
    }

    out
}

fn find_tag_close(rest: &str) -> Option<usize> {
    for (offset, ch) in rest.char_indices() {
        if ch == '\n' {
            return None;
        }
        if ch == '>' {
            if offset > 0 {
                return Some(offset);
            }
            return None;
        }
    }
    None
}

/// Strips a matching leading article from `name` to compute artist `sortname`.
///
/// Mirrors `Slim::Utils::Text::ignoreArticles` case-insensitively on char
/// boundaries, requiring whitespace after non-apostrophe articles.
#[must_use]
pub fn strip_leading_articles(name: &str, articles: &[&str]) -> String {
    let trimmed = name.trim();
    for raw_article in articles {
        let article = raw_article.trim();
        if article.is_empty() {
            continue;
        }
        if let Some(remainder) = strip_article_prefix(trimmed, article) {
            return remainder.trim_start().to_owned();
        }
    }
    trimmed.to_owned()
}

fn strip_article_prefix<'a>(text: &'a str, article: &str) -> Option<&'a str> {
    let mut text_chars = text.char_indices();
    for art_ch in article.chars() {
        let (_, txt_ch) = text_chars.next()?;
        if !txt_ch.to_lowercase().eq(art_ch.to_lowercase()) {
            return None;
        }
    }

    let rest = match text_chars.next() {
        Some((idx, next_ch)) => {
            if !article.ends_with('\'') && !next_ch.is_whitespace() {
                return None;
            }
            &text[idx..]
        }
        None => return None,
    };

    if rest.trim().is_empty() {
        return None;
    }
    Some(rest)
}

/// Normalizes Spotify Web API v1 JSON payloads and updates [`DualCache`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntityNormalizer {
    /// Policy governing `(Remastered)` / `[Deluxe Edition]` suffix cleanup.
    pub tag_cleanup: TagCleanupPolicy,
}

impl EntityNormalizer {
    /// Creates an [`EntityNormalizer`] with the given [`TagCleanupPolicy`].
    #[must_use]
    pub fn new(tag_cleanup: TagCleanupPolicy) -> Self {
        Self { tag_cleanup }
    }

    /// Applies the configured [`TagCleanupPolicy`] to an album or track title.
    #[must_use]
    pub fn clean_title(&self, title: &str) -> String {
        match self.tag_cleanup {
            TagCleanupPolicy::StripSuffixes => cleanup_tags(title),
            TagCleanupPolicy::PreserveRaw => title.to_owned(),
            _ => title.to_owned(),
        }
    }

    /// Normalizes a Spotify album object, injects `minAlbum` onto child tracks,
    /// and persists album and track records for 90 days.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a valid album object, or
    /// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
    pub fn normalize_album(
        &self,
        raw: &Value,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
    ) -> Result<NormalizedAlbum> {
        let obj = expect_object(raw, "album")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "album", &id);
        let name = self.clean_title(&string_field(obj, "name"));
        let artists = parse_artists(obj.get("artists"));
        let artist = non_empty_string(obj, "artist")
            .or_else(|| artists.first().map(|first| first.name.clone()))
            .unwrap_or_default();
        let image = extract_image(obj, "images");
        let release_date = string_field(obj, "release_date");
        let album_type = string_field(obj, "album_type");
        let total_tracks = u32_field(obj, "total_tracks");
        let added_at = optional_string_field(obj, "added_at");

        let min_album = MinAlbum {
            id: id.clone(),
            uri: uri.clone(),
            name: name.clone(),
            artists: artists.clone(),
            image: image.clone(),
            release_date: release_date.clone(),
            album_type: album_type.clone(),
            added_at: added_at.clone(),
        };

        let raw_tracks = extract_album_tracks_slice(obj);
        let mut tracks = Vec::with_capacity(raw_tracks.len());
        for track_val in raw_tracks {
            let normalized =
                self.normalize_track(track_val, Some(&min_album), cache, write_mode)?;
            tracks.push(normalized);
        }

        let mut album = NormalizedAlbum {
            id,
            uri,
            name,
            artist,
            artists,
            image,
            release_date,
            album_type,
            total_tracks,
            tracks,
            added_at,
        };

        if !album.uri.is_empty() {
            let payload = serialize_for_cache(&album)?;
            cache.store_metadata(&album.uri, None, &payload, write_mode)?;
            if write_mode == MetadataWriteMode::MergeExisting {
                if let Some(cached) = cache.metadata_value(&album.uri)? {
                    if let Ok(merged_album) = serde_json::from_value::<NormalizedAlbum>(cached) {
                        album = merged_album;
                    }
                }
            }
        }

        Ok(album)
    }

    /// Normalizes a Spotify track object, applies `fallback_album` when
    /// embedded `album.name` is missing, and persists both `uri` and
    /// `linked_from.uri` in the 90-day cache.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a JSON object, or
    /// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
    pub fn normalize_track(
        &self,
        raw: &Value,
        fallback_album: Option<&MinAlbum>,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
    ) -> Result<NormalizedTrack> {
        let obj = expect_object(raw, "track")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "track", &id);
        let name = self.clean_title(&string_field(obj, "name"));
        let artists = parse_artists(obj.get("artists"));
        let album = self.parse_track_album(obj.get("album"), fallback_album);
        let mut image = extract_image(obj, "images");
        if image.is_empty() {
            image = album.image.clone();
        }

        let duration_ms = u64_field(obj, "duration_ms");
        let track_number = u32_field(obj, "track_number");
        let disc_number = u32_field(obj, "disc_number");
        let explicit = bool_field(obj, "explicit", false);
        let is_local = bool_field(obj, "is_local", false);
        let is_playable = !is_local && bool_field(obj, "is_playable", true);
        let linked_from = parse_linked_from(obj.get("linked_from"));
        let added_at = optional_string_field(obj, "added_at");

        let mut track = NormalizedTrack {
            id,
            uri,
            name,
            artists,
            album,
            duration_ms,
            track_number,
            disc_number,
            explicit,
            is_playable,
            image,
            linked_from,
            added_at,
        };

        if !track.uri.is_empty() {
            let linked_uri = track
                .linked_from
                .as_ref()
                .map(|lf| lf.uri.as_str())
                .filter(|u| !u.is_empty());
            let payload = serialize_for_cache(&track)?;
            cache.store_metadata(&track.uri, linked_uri, &payload, write_mode)?;
            if write_mode == MetadataWriteMode::MergeExisting {
                if let Some(cached) = cache.metadata_value(&track.uri)? {
                    if let Ok(merged_track) = serde_json::from_value::<NormalizedTrack>(cached) {
                        track = merged_track;
                    }
                }
            }
        }

        Ok(track)
    }

    /// Normalizes a Spotify artist object, computes `sortname`, and backfills or
    /// persists `spotify_artist_image_<id>` in the 7-day ephemeral cache.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not an object, or
    /// [`SpottyError::Cache`] on cache failure.
    pub fn normalize_artist(
        &self,
        raw: &Value,
        cache: &DualCache,
        articles: &[&str],
    ) -> Result<NormalizedArtist> {
        let obj = expect_object(raw, "artist")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "artist", &id);
        let name = string_field(obj, "name");
        let sortname = strip_leading_articles(&name, articles);
        let mut image = extract_image(obj, "images");

        if !id.is_empty() {
            let cache_key = format!("spotify_artist_image_{id}");
            if image.is_empty() {
                if let Some(Value::String(cached_img)) = cache.ephemeral_value(&cache_key)? {
                    image = cached_img;
                }
            } else {
                let ttl = Duration::from_secs(EPISODE_ARTIST_TTL_SECS);
                cache.put_ephemeral(&cache_key, &Value::String(image.clone()), ttl)?;
            }
        }

        let followers_total = obj
            .get("followers")
            .and_then(Value::as_object)
            .and_then(|followers| followers.get("total"))
            .and_then(Value::as_u64)
            .or_else(|| obj.get("followers_total").and_then(Value::as_u64))
            .unwrap_or(0);

        let genres = parse_string_list(obj.get("genres"));

        Ok(NormalizedArtist {
            id,
            uri,
            name,
            sortname,
            image,
            followers_total,
            genres,
        })
    }

    /// Normalizes a Spotify playlist object, strips HTML from `description`,
    /// and caches `playlist_owner_<id>` -> `creator` for 30 days.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not an object, or
    /// [`SpottyError::Cache`] on cache failure.
    pub fn normalize_playlist(&self, raw: &Value, cache: &DualCache) -> Result<NormalizedPlaylist> {
        let obj = expect_object(raw, "playlist")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "playlist", &id);
        let name = string_field(obj, "name");
        let description = strip_html_tags(&string_field(obj, "description"));
        let image = extract_image(obj, "images");
        let snapshot_id = string_field(obj, "snapshot_id");
        let collaborative = bool_field(obj, "collaborative", false);

        let (creator, owner_display_name) = parse_playlist_owner(obj);
        if !id.is_empty() && !creator.is_empty() {
            let cache_key = format!("playlist_owner_{id}");
            let needs_write = match cache.ephemeral_value(&cache_key)? {
                Some(Value::String(existing)) => existing != creator,
                _ => true,
            };
            if needs_write {
                let ttl = Duration::from_secs(PLAYLIST_OWNER_TTL_SECS);
                cache.put_ephemeral(&cache_key, &Value::String(creator.clone()), ttl)?;
            }
        }

        let tracks_total = obj
            .get("tracks")
            .and_then(Value::as_object)
            .and_then(|tracks| tracks.get("total"))
            .and_then(Value::as_u64)
            .or_else(|| obj.get("tracks_total").and_then(Value::as_u64))
            .map_or(0, clamp_u64_to_u32);

        Ok(NormalizedPlaylist {
            id,
            uri,
            name,
            creator,
            owner_display_name,
            description,
            image,
            snapshot_id,
            collaborative,
            tracks_total,
        })
    }

    /// Normalizes a Spotify podcast show and all embedded episodes.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not an object, or
    /// [`SpottyError::Cache`] on cache failure.
    pub fn normalize_show(
        &self,
        raw: &Value,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
    ) -> Result<NormalizedShow> {
        let obj = expect_object(raw, "show")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "show", &id);
        let name = string_field(obj, "name");
        let publisher = string_field(obj, "publisher");
        let description = strip_html_tags(&string_field(obj, "description"));
        let image = extract_image(obj, "images");
        let total_episodes = u32_field(obj, "total_episodes");
        let media_type = string_field(obj, "media_type");

        let show_stub = NormalizedShow {
            id: id.clone(),
            uri: uri.clone(),
            name: name.clone(),
            publisher: publisher.clone(),
            description: description.clone(),
            image: image.clone(),
            total_episodes,
            media_type: media_type.clone(),
            episodes: Vec::new(),
        };

        let raw_episodes = extract_show_episodes_slice(obj);
        let mut episodes = Vec::with_capacity(raw_episodes.len());
        for ep_val in raw_episodes {
            let normalized = self.normalize_episode(ep_val, Some(&show_stub), cache, write_mode)?;
            episodes.push(normalized);
        }

        Ok(NormalizedShow {
            id,
            uri,
            name,
            publisher,
            description,
            image,
            total_episodes,
            media_type,
            episodes,
        })
    }

    /// Normalizes a Spotify podcast episode, maps show/publisher metadata,
    /// strips HTML from `description`, and caches `spotify:episode:<id>` with a
    /// 7-day TTL.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not an object, or
    /// [`SpottyError::Cache`] on cache failure.
    pub fn normalize_episode(
        &self,
        raw: &Value,
        fallback_show: Option<&NormalizedShow>,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
    ) -> Result<NormalizedEpisode> {
        let obj = expect_object(raw, "episode")?;
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "episode", &id);
        let name = string_field(obj, "name");
        let description = strip_html_tags(&string_field(obj, "description"));
        let duration_ms = u64_field(obj, "duration_ms");
        let release_date = string_field(obj, "release_date");
        let explicit = bool_field(obj, "explicit", false);
        let is_playable = resolve_episode_playable(obj, &uri);

        let show_obj = obj.get("show").and_then(Value::as_object);
        let show_name = show_obj
            .and_then(|s| non_empty_string(s, "name"))
            .or_else(|| non_empty_string(obj, "show_name"))
            .or_else(|| fallback_show.map(|s| s.name.clone()))
            .unwrap_or_default();
        let show_uri = show_obj
            .and_then(|s| non_empty_string(s, "uri"))
            .or_else(|| non_empty_string(obj, "show_uri"))
            .or_else(|| fallback_show.map(|s| s.uri.clone()))
            .unwrap_or_default();
        let publisher = non_empty_string(obj, "publisher")
            .or_else(|| show_obj.and_then(|s| non_empty_string(s, "publisher")))
            .or_else(|| fallback_show.map(|s| s.publisher.clone()))
            .unwrap_or_default();

        let mut image = extract_image(obj, "images");
        if image.is_empty() {
            if let Some(show) = show_obj {
                image = extract_image(show, "images");
            }
        }
        if image.is_empty() {
            if let Some(show) = fallback_show {
                image = show.image.clone();
            }
        }

        let resume_position_ms = obj
            .get("resume_point")
            .and_then(Value::as_object)
            .and_then(|rp| rp.get("resume_position_ms"))
            .and_then(Value::as_u64)
            .or_else(|| obj.get("resume_position_ms").and_then(Value::as_u64));

        let episode = NormalizedEpisode {
            id,
            uri,
            name,
            description,
            duration_ms,
            release_date,
            explicit,
            is_playable,
            image,
            show_name,
            show_uri,
            publisher,
            resume_position_ms,
        };

        if !episode.uri.is_empty() {
            let should_write = match write_mode {
                MetadataWriteMode::FastSkipIfPresent => {
                    cache.ephemeral_value(&episode.uri)?.is_none()
                }
                _ => true,
            };
            if should_write {
                let cache_val = build_episode_cache_value(&episode)?;
                let ttl = Duration::from_secs(EPISODE_ARTIST_TTL_SECS);
                cache.put_ephemeral(&episode.uri, &cache_val, ttl)?;
            }
        }

        Ok(episode)
    }

    /// Normalizes a Spotify user profile (`/v1/me` or `/v1/users/{id}`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a JSON object.
    pub fn normalize_user_profile(&self, raw: &Value) -> Result<UserProfile> {
        normalize_user_profile(raw)
    }

    /// Normalizes `/v1/me/player` context state and any embedded track/episode.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a JSON object, or
    /// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
    pub fn normalize_player_state(
        &self,
        raw: &Value,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
    ) -> Result<PlayerContextState> {
        let obj = expect_object(raw, "player")?;
        let is_playing = bool_field(obj, "is_playing", false);
        let progress_ms = obj.get("progress_ms").and_then(Value::as_u64);
        let timestamp = obj.get("timestamp").and_then(Value::as_u64);
        let shuffle_state = bool_field(obj, "shuffle_state", false);
        let repeat_state = string_field(obj, "repeat_state");
        let currently_playing_type = string_field(obj, "currently_playing_type");
        let device = parse_player_device(obj.get("device"));
        let context_uri = obj
            .get("context")
            .and_then(Value::as_object)
            .and_then(|ctx| non_empty_string(ctx, "uri"));

        let mut track = None;
        let mut episode = None;
        if let Some(item_val) = obj.get("item").filter(|v| v.is_object()) {
            let item_type = item_val
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(&currently_playing_type);
            if item_type == "episode" {
                episode = Some(self.normalize_episode(item_val, None, cache, write_mode)?);
            } else {
                track = Some(self.normalize_track(item_val, None, cache, write_mode)?);
            }
        }

        Ok(PlayerContextState {
            is_playing,
            progress_ms,
            timestamp,
            shuffle_state,
            repeat_state,
            currently_playing_type,
            device,
            context_uri,
            track,
            episode,
        })
    }

    /// Normalizes a Spotify browse category object (`/v1/browse/categories`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a JSON object.
    pub fn normalize_browse_category(&self, raw: &Value) -> Result<BrowseCategory> {
        normalize_browse_category(raw)
    }

    /// Normalizes a `/v1/search` response across all entity buckets.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `raw` is not a JSON object, or
    /// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
    pub fn normalize_search_results(
        &self,
        raw: &Value,
        cache: &DualCache,
        write_mode: MetadataWriteMode,
        articles: &[&str],
    ) -> Result<SearchResults> {
        let obj = expect_object(raw, "search")?;
        let mut results = SearchResults::default();

        if let Some(tracks_bucket) = obj.get("tracks").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, tracks_bucket);
            for item in bucket_items(tracks_bucket) {
                results
                    .tracks
                    .push(self.normalize_track(item, None, cache, write_mode)?);
            }
        }
        if let Some(albums_bucket) = obj.get("albums").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, albums_bucket);
            for item in bucket_items(albums_bucket) {
                results
                    .albums
                    .push(self.normalize_album(item, cache, write_mode)?);
            }
        }
        if let Some(artists_bucket) = obj.get("artists").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, artists_bucket);
            for item in bucket_items(artists_bucket) {
                results
                    .artists
                    .push(self.normalize_artist(item, cache, articles)?);
            }
        }
        if let Some(playlists_bucket) = obj.get("playlists").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, playlists_bucket);
            for item in bucket_items(playlists_bucket) {
                results
                    .playlists
                    .push(self.normalize_playlist(item, cache)?);
            }
        }
        if let Some(shows_bucket) = obj.get("shows").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, shows_bucket);
            for item in bucket_items(shows_bucket) {
                results
                    .shows
                    .push(self.normalize_show(item, cache, write_mode)?);
            }
        }
        if let Some(episodes_bucket) = obj.get("episodes").and_then(Value::as_object) {
            record_bucket_pagination(&mut results, episodes_bucket);
            for item in bucket_items(episodes_bucket) {
                results
                    .episodes
                    .push(self.normalize_episode(item, None, cache, write_mode)?);
            }
        }

        Ok(results)
    }

    fn parse_track_album(
        &self,
        raw_album: Option<&Value>,
        fallback_album: Option<&MinAlbum>,
    ) -> MinAlbum {
        let Some(album_obj) = raw_album.and_then(Value::as_object) else {
            return fallback_album.cloned().unwrap_or_default();
        };

        let raw_name = string_field(album_obj, "name");
        if raw_name.is_empty() {
            return fallback_album.cloned().unwrap_or_default();
        }

        let id = string_field(album_obj, "id");
        let uri = resolve_uri(album_obj, "album", &id);
        let name = self.clean_title(&raw_name);
        let artists = parse_artists(album_obj.get("artists"));
        let mut image = extract_image(album_obj, "images");
        if image.is_empty() {
            if let Some(fb) = fallback_album {
                image = fb.image.clone();
            }
        }
        let release_date = string_field(album_obj, "release_date");
        let album_type = string_field(album_obj, "album_type");
        let added_at = optional_string_field(album_obj, "added_at");

        MinAlbum {
            id,
            uri,
            name,
            artists,
            image,
            release_date,
            album_type,
            added_at,
        }
    }
}

/// Normalizes a Spotify album using [`TagCleanupPolicy::PreserveRaw`].
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not an album object, or
/// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
pub fn normalize_album(
    raw: &Value,
    cache: &DualCache,
    write_mode: MetadataWriteMode,
) -> Result<NormalizedAlbum> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_album(raw, cache, write_mode)
}

/// Normalizes a Spotify track using [`TagCleanupPolicy::PreserveRaw`].
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not a track object, or
/// [`SpottyError::Cache`] / [`SpottyError::InvalidUri`] on cache failure.
pub fn normalize_track(
    raw: &Value,
    fallback_album: Option<&MinAlbum>,
    cache: &DualCache,
    write_mode: MetadataWriteMode,
) -> Result<NormalizedTrack> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_track(
        raw,
        fallback_album,
        cache,
        write_mode,
    )
}

/// Normalizes a Spotify artist and backfills or persists its 7-day image key.
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not an artist object, or
/// [`SpottyError::Cache`] on cache failure.
pub fn normalize_artist(
    raw: &Value,
    cache: &DualCache,
    articles: &[&str],
) -> Result<NormalizedArtist> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_artist(raw, cache, articles)
}

/// Normalizes a Spotify playlist and caches `playlist_owner_<id>` for 30 days.
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not a playlist object, or
/// [`SpottyError::Cache`] on cache failure.
pub fn normalize_playlist(raw: &Value, cache: &DualCache) -> Result<NormalizedPlaylist> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_playlist(raw, cache)
}

/// Normalizes a Spotify podcast show and all embedded episodes.
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not a show object, or
/// [`SpottyError::Cache`] on cache failure.
pub fn normalize_show(
    raw: &Value,
    cache: &DualCache,
    write_mode: MetadataWriteMode,
) -> Result<NormalizedShow> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_show(raw, cache, write_mode)
}

/// Normalizes a Spotify podcast episode and caches `spotify:episode:<id>` for 7
/// days.
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not an episode object, or
/// [`SpottyError::Cache`] on cache failure.
pub fn normalize_episode(
    raw: &Value,
    fallback_show: Option<&NormalizedShow>,
    cache: &DualCache,
    write_mode: MetadataWriteMode,
) -> Result<NormalizedEpisode> {
    EntityNormalizer::new(TagCleanupPolicy::PreserveRaw).normalize_episode(
        raw,
        fallback_show,
        cache,
        write_mode,
    )
}

/// Normalizes a Spotify user profile (`/v1/me` or `/v1/users/{id}`).
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not a JSON object.
pub fn normalize_user_profile(raw: &Value) -> Result<UserProfile> {
    let obj = expect_object(raw, "user")?;
    let id = string_field(obj, "id");
    let uri = resolve_uri(obj, "user", &id);
    let display_name = optional_string_field(obj, "display_name");
    let country = optional_string_field(obj, "country");
    let product = optional_string_field(obj, "product");
    let email = optional_string_field(obj, "email");
    let image = extract_image(obj, "images");
    let followers_total = obj
        .get("followers")
        .and_then(Value::as_object)
        .and_then(|followers| followers.get("total"))
        .and_then(Value::as_u64)
        .or_else(|| obj.get("followers_total").and_then(Value::as_u64));

    Ok(UserProfile {
        id,
        uri,
        display_name,
        country,
        product,
        email,
        image,
        followers_total,
    })
}

/// Normalizes a Spotify browse category (`/v1/browse/categories`).
///
/// # Errors
///
/// Returns [`SpottyError::Json`] if `raw` is not a JSON object.
pub fn normalize_browse_category(raw: &Value) -> Result<BrowseCategory> {
    let obj = expect_object(raw, "category")?;
    let id = string_field(obj, "id");
    let name = string_field(obj, "name");
    let image = extract_image(obj, "icons");

    Ok(BrowseCategory { id, name, image })
}

fn expect_object<'a>(raw: &'a Value, entity_name: &str) -> Result<&'a Map<String, Value>> {
    raw.as_object()
        .ok_or_else(|| SpottyError::Json(format!("expected JSON object for Spotify {entity_name}")))
}

fn string_field(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn non_empty_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|val| !val.is_empty())
        .map(str::to_owned)
}

fn optional_string_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn u64_field(obj: &Map<String, Value>, key: &str) -> u64 {
    obj.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn u32_field(obj: &Map<String, Value>, key: &str) -> u32 {
    obj.get(key)
        .and_then(Value::as_u64)
        .map_or(0, clamp_u64_to_u32)
}

fn bool_field(obj: &Map<String, Value>, key: &str, default: bool) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn clamp_u64_to_u32(val: u64) -> u32 {
    u32::try_from(val).unwrap_or(u32::MAX)
}

fn resolve_uri(obj: &Map<String, Value>, kind: &str, id: &str) -> String {
    if let Some(uri) = non_empty_string(obj, "uri") {
        return uri;
    }
    if !id.is_empty() {
        return format!("spotify:{kind}:{id}");
    }
    String::new()
}

fn extract_image(obj: &Map<String, Value>, array_field: &str) -> String {
    if let Some(items) = obj.get(array_field).and_then(Value::as_array) {
        let mut parsed = Vec::with_capacity(items.len());
        for entry in items {
            let Some(entry_obj) = entry.as_object() else {
                continue;
            };
            let Some(url) = non_empty_string(entry_obj, "url") else {
                continue;
            };
            let height = entry_obj
                .get("height")
                .and_then(Value::as_u64)
                .map(clamp_u64_to_u32);
            let width = entry_obj
                .get("width")
                .and_then(Value::as_u64)
                .map(clamp_u64_to_u32);
            parsed.push(ArtworkImage::new(url, height, width));
        }
        let best = largest_artwork_url(&parsed);
        if !best.is_empty() {
            return best.to_owned();
        }
    }

    non_empty_string(obj, "image").unwrap_or_default()
}

fn parse_artists(raw: Option<&Value>) -> Vec<ArtistSummary> {
    let Some(items) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for entry in items {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let id = string_field(obj, "id");
        let uri = resolve_uri(obj, "artist", &id);
        let name = string_field(obj, "name");
        out.push(ArtistSummary::new(id, uri, name));
    }
    out
}

fn parse_linked_from(raw: Option<&Value>) -> Option<LinkedFrom> {
    let obj = raw.and_then(Value::as_object)?;
    let id = string_field(obj, "id");
    let uri = resolve_uri(obj, "track", &id);
    if id.is_empty() && uri.is_empty() {
        return None;
    }
    Some(LinkedFrom::new(id, uri))
}

fn parse_string_list(raw: Option<&Value>) -> Vec<String> {
    let Some(items) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(items.len());
    for entry in items {
        if let Some(s) = entry.as_str() {
            out.push(s.to_owned());
        }
    }
    out
}

fn parse_playlist_owner(obj: &Map<String, Value>) -> (String, Option<String>) {
    if let Some(owner) = obj.get("owner").and_then(Value::as_object) {
        let creator = string_field(owner, "id");
        let display_name = optional_string_field(owner, "display_name");
        return (creator, display_name);
    }
    let creator = string_field(obj, "creator");
    let display_name = optional_string_field(obj, "owner_display_name");
    (creator, display_name)
}

fn parse_player_device(raw: Option<&Value>) -> Option<PlayerDevice> {
    let obj = raw.and_then(Value::as_object)?;
    let id = optional_string_field(obj, "id");
    let name = string_field(obj, "name");
    let device_type = non_empty_string(obj, "type")
        .or_else(|| non_empty_string(obj, "device_type"))
        .unwrap_or_default();
    let is_active = bool_field(obj, "is_active", false);
    let volume_percent = obj
        .get("volume_percent")
        .and_then(Value::as_u64)
        .map(clamp_u64_to_u32);

    Some(PlayerDevice {
        id,
        name,
        device_type,
        is_active,
        volume_percent,
    })
}

fn extract_album_tracks_slice(obj: &Map<String, Value>) -> &[Value] {
    match obj.get("tracks") {
        Some(Value::Object(tracks_obj)) => tracks_obj
            .get("items")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    }
}

fn extract_show_episodes_slice(obj: &Map<String, Value>) -> &[Value] {
    match obj.get("episodes") {
        Some(Value::Object(ep_obj)) => ep_obj
            .get("items")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice),
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    }
}

fn bucket_items(bucket: &Map<String, Value>) -> &[Value] {
    bucket
        .get("items")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn record_bucket_pagination(results: &mut SearchResults, bucket: &Map<String, Value>) {
    if results.total.is_none() {
        results.total = bucket
            .get("total")
            .and_then(Value::as_u64)
            .map(clamp_u64_to_u32);
    }
    if results.next.is_none() {
        results.next = non_empty_string(bucket, "next");
    }
}

fn resolve_episode_playable(obj: &Map<String, Value>, uri: &str) -> bool {
    let raw_playable = obj.get("is_playable").and_then(Value::as_bool);
    let raw_episode_flag = obj.get("episode").and_then(Value::as_bool);

    // Spotify playlists report `is_playable: false` and `episode: false` on
    // valid podcast episodes (`API.pm:1153`).
    if raw_playable == Some(false)
        && raw_episode_flag == Some(false)
        && uri.starts_with("spotify:episode:")
    {
        return true;
    }
    raw_playable.unwrap_or(true)
}

fn serialize_for_cache<T: serde::Serialize>(entity: &T) -> Result<Value> {
    let mut val = serde_json::to_value(entity)?;
    strip_empty_cache_fields(&mut val);
    Ok(val)
}

fn strip_empty_cache_fields(val: &mut Value) {
    match val {
        Value::Object(map) => {
            let mut empty_keys = Vec::new();
            for (key, child) in map.iter_mut() {
                strip_empty_cache_fields(child);
                let is_empty_optional = match child {
                    Value::Null => true,
                    Value::String(s) => {
                        s.is_empty()
                            && matches!(
                                key.as_str(),
                                "image" | "added_at" | "release_date" | "album_type"
                            )
                    }
                    _ => false,
                };
                if is_empty_optional {
                    empty_keys.push(key.clone());
                }
            }
            for key in empty_keys {
                map.remove(&key);
            }
        }
        Value::Array(items) => {
            for child in items {
                strip_empty_cache_fields(child);
            }
        }
        _ => {}
    }
}

fn build_episode_cache_value(episode: &NormalizedEpisode) -> Result<Value> {
    let mut val = serde_json::to_value(episode)?;
    let track_view = episode.to_track_view();
    if let Value::Object(ref mut map) = val {
        map.insert(
            String::from("album"),
            serde_json::to_value(&track_view.album)?,
        );
        map.insert(
            String::from("artists"),
            serde_json::to_value(&track_view.artists)?,
        );
    }
    Ok(val)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{json, Value};
    use spotty_cache::{
        DualCache, MetadataWriteMode, EPISODE_ARTIST_TTL_SECS, METADATA_TTL_SECS,
        PLAYLIST_OWNER_TTL_SECS,
    };
    use spotty_types::TagCleanupPolicy;
    use tempfile::TempDir;

    use super::{
        normalize_album, normalize_artist, normalize_browse_category, normalize_episode,
        normalize_playlist, normalize_show, normalize_track, normalize_user_profile,
        strip_html_tags, strip_leading_articles, EntityNormalizer,
    };
    use crate::models::MinAlbum;

    fn temp_cache() -> (TempDir, DualCache) {
        let dir = TempDir::new().expect("tempdir creation must succeed");
        let cache =
            DualCache::open(dir.path().join("spotty_api_test.redb")).expect("open test DualCache");
        (dir, cache)
    }

    #[test]
    fn strip_html_tags_removes_markup_and_preserves_plain_text() {
        assert_eq!(
            strip_html_tags("<p>Listen <a href=\"https://example.com\">now</a>!</p>"),
            "Listen now!"
        );
        assert_eq!(strip_html_tags("5 < 10"), "5 < 10");
        assert_eq!(strip_html_tags("<> kept"), "<> kept");
        assert_eq!(
            strip_html_tags("<b>Line 1</b>\n<i>Line 2</i>"),
            "Line 1\nLine 2"
        );
    }

    #[test]
    fn strip_leading_articles_computes_artist_sortname() {
        let articles = ["the", "a", "an", "l'"];
        assert_eq!(strip_leading_articles("The Beatles", &articles), "Beatles");
        assert_eq!(
            strip_leading_articles("  the   rolling stones ", &articles),
            "rolling stones"
        );
        assert_eq!(
            strip_leading_articles("A Perfect Circle", &articles),
            "Perfect Circle"
        );
        assert_eq!(
            strip_leading_articles("L'Impératrice", &articles),
            "Impératrice"
        );
        assert_eq!(strip_leading_articles("Them", &articles), "Them");
        assert_eq!(strip_leading_articles("The", &articles), "The");
    }

    #[test]
    fn normalize_album_unwraps_tracks_injects_min_album_and_caches_90d() {
        let (_dir, cache) = temp_cache();
        let raw_album = json!({
            "id": "4aawyAB9vmqN3uQ7FjRGTy",
            "uri": "spotify:album:4aawyAB9vmqN3uQ7FjRGTy",
            "name": "Abbey Road (2019 Mix - Deluxe Edition)",
            "album_type": "album",
            "release_date": "1969-09-26",
            "total_tracks": 17,
            "added_at": "2026-01-10T12:00:00Z",
            "available_markets": ["US", "GB"],
            "images": [
                {"url": "https://img/64.jpg", "height": 64, "width": 64},
                {"url": "https://img/640.jpg", "height": 640, "width": 640},
                {"url": "https://img/300.jpg", "height": 300, "width": 300}
            ],
            "artists": [{
                "id": "3WrFJ7ztbogyGnTHbHJFl2",
                "uri": "spotify:artist:3WrFJ7ztbogyGnTHbHJFl2",
                "name": "The Beatles"
            }],
            "tracks": {
                "items": [
                    {
                        "id": "2EqlS6tkEnglzr7tkKAAYD",
                        "uri": "spotify:track:2EqlS6tkEnglzr7tkKAAYD",
                        "name": "Come Together (2019 Mix)",
                        "duration_ms": 259946,
                        "track_number": 1,
                        "disc_number": 1,
                        "explicit": false,
                        "is_playable": true,
                        "artists": [{
                            "id": "3WrFJ7ztbogyGnTHbHJFl2",
                            "uri": "spotify:artist:3WrFJ7ztbogyGnTHbHJFl2",
                            "name": "The Beatles"
                        }]
                    }
                ]
            }
        });

        let normalizer = EntityNormalizer::new(TagCleanupPolicy::StripSuffixes);
        let album = normalizer
            .normalize_album(&raw_album, &cache, MetadataWriteMode::MergeExisting)
            .expect("album normalization succeeds");

        assert_eq!(album.name, "Abbey Road");
        assert_eq!(album.artist, "The Beatles");
        assert_eq!(album.image, "https://img/640.jpg");
        assert_eq!(album.tracks.len(), 1);

        let track = &album.tracks[0];
        assert_eq!(track.album.id, "4aawyAB9vmqN3uQ7FjRGTy");
        assert_eq!(track.album.name, "Abbey Road");
        assert_eq!(track.album.image, "https://img/640.jpg");
        assert_eq!(
            track.album.added_at.as_deref(),
            Some("2026-01-10T12:00:00Z")
        );
        assert_eq!(track.image, "https://img/640.jpg");

        let cached_album = cache
            .metadata_value("spotify:album:4aawyAB9vmqN3uQ7FjRGTy")
            .expect("cache read")
            .expect("album cached");
        assert_eq!(cached_album["image"], "https://img/640.jpg");

        let cached_track = cache
            .metadata_value("spotify:track:2EqlS6tkEnglzr7tkKAAYD")
            .expect("cache read")
            .expect("track cached");
        assert_eq!(cached_track["album"]["name"], "Abbey Road");
    }

    #[test]
    fn normalize_track_indexes_linked_from_and_preserves_raw_policy() {
        let (_dir, cache) = temp_cache();
        let raw_track = json!({
            "id": "canonical_id",
            "uri": "spotify:track:canonical_id",
            "name": "Hotel California - Live at The Forum",
            "duration_ms": 390000,
            "track_number": 1,
            "disc_number": 1,
            "explicit": false,
            "is_playable": true,
            "added_at": "2025-11-01T00:00:00Z",
            "linked_from": {
                "id": "original_id",
                "uri": "spotify:track:original_id"
            },
            "artists": [
                {"id": "art1", "uri": "spotify:artist:art1", "name": "Eagles"}
            ],
            "album": {
                "id": "alb1",
                "uri": "spotify:album:alb1",
                "name": "Hotel California (40th Anniversary Edition)",
                "release_date": "1976-12-08",
                "album_type": "album",
                "images": [
                    {"url": "https://img/hc640.jpg", "height": 640, "width": 640}
                ]
            }
        });

        let track = normalize_track(&raw_track, None, &cache, MetadataWriteMode::MergeExisting)
            .expect("track normalization succeeds");
        assert_eq!(track.name, "Hotel California - Live at The Forum");
        assert_eq!(track.image, "https://img/hc640.jpg");
        assert_eq!(
            track.linked_from.as_ref().map(|lf| lf.uri.as_str()),
            Some("spotify:track:original_id")
        );

        let canonical = cache
            .metadata_value("spotify:track:canonical_id")
            .expect("read canonical")
            .expect("canonical present");
        let linked = cache
            .metadata_value("spotify:track:original_id")
            .expect("read linked")
            .expect("linked present");
        assert_eq!(canonical, linked);

        // Partial update merges cleanly without wiping cached album image or added_at.
        let partial_update = json!({
            "id": "canonical_id",
            "uri": "spotify:track:canonical_id",
            "name": "Hotel California - Live at The Forum",
            "duration_ms": 390000,
            "track_number": 1,
            "disc_number": 1,
            "is_playable": true,
            "linked_from": {
                "id": "original_id",
                "uri": "spotify:track:original_id"
            },
            "album": {
                "id": "alb1",
                "uri": "spotify:album:alb1",
                "name": "Hotel California (40th Anniversary Edition)"
            }
        });
        let merged = normalize_track(
            &partial_update,
            None,
            &cache,
            MetadataWriteMode::MergeExisting,
        )
        .expect("merge update succeeds");
        assert_eq!(merged.album.image, "https://img/hc640.jpg");
        assert_eq!(merged.added_at.as_deref(), Some("2025-11-01T00:00:00Z"));
        assert_eq!(METADATA_TTL_SECS, 90 * 86_400);
    }

    #[test]
    fn normalize_track_uses_fallback_album_and_flags_local_unplayable() {
        let (_dir, cache) = temp_cache();
        let fallback = MinAlbum::new(
            "alb_fb",
            "spotify:album:alb_fb",
            "Fallback Album",
            Vec::new(),
            "https://img/fb.jpg",
            "2024",
            "album",
            None,
        );
        let raw_local = json!({
            "id": "local1",
            "uri": "spotify:local:artist:album:track:180",
            "name": "Local Song",
            "is_local": true,
            "duration_ms": 180000
        });

        let track = normalize_track(
            &raw_local,
            Some(&fallback),
            &cache,
            MetadataWriteMode::OverwriteDirect,
        )
        .expect("local track normalizes");
        assert!(!track.is_playable, "local track must not be playable");
        assert_eq!(track.album.name, "Fallback Album");
        assert_eq!(track.image, "https://img/fb.jpg");
    }

    #[test]
    fn normalize_artist_persists_and_backfills_7d_image_key() {
        let (_dir, cache) = temp_cache();
        let articles = ["the", "a", "an"];
        let full_artist = json!({
            "id": "artist123",
            "uri": "spotify:artist:artist123",
            "name": "The Cure",
            "followers": {"total": 5_000_000},
            "genres": ["post-punk", "gothic rock"],
            "images": [
                {"url": "https://img/cure_300.jpg", "height": 300, "width": 300},
                {"url": "https://img/cure_640.jpg", "height": 640, "width": 640}
            ]
        });

        let first = normalize_artist(&full_artist, &cache, &articles).expect("artist ok");
        assert_eq!(first.sortname, "Cure");
        assert_eq!(first.image, "https://img/cure_640.jpg");
        assert_eq!(first.followers_total, 5_000_000);
        assert_eq!(first.genres, vec!["post-punk", "gothic rock"]);

        // Slim payload without images backfills from `spotify_artist_image_<id>`.
        let slim_artist = json!({
            "id": "artist123",
            "uri": "spotify:artist:artist123",
            "name": "The Cure"
        });
        let backfilled = normalize_artist(&slim_artist, &cache, &articles).expect("backfill ok");
        assert_eq!(backfilled.image, "https://img/cure_640.jpg");

        // Verify 7-day TTL constant and expiration semantics via ephemeral_value_at.
        let t0 = 1_700_000_000_u64;
        cache
            .put_ephemeral_at(
                "spotify_artist_image_ttl_check",
                &Value::String(String::from("https://img/ttl.jpg")),
                EPISODE_ARTIST_TTL_SECS,
                t0,
            )
            .expect("seed ttl key");
        assert!(cache
            .ephemeral_value_at(
                "spotify_artist_image_ttl_check",
                t0 + EPISODE_ARTIST_TTL_SECS - 1
            )
            .expect("read before 7d")
            .is_some());
        assert!(cache
            .ephemeral_value_at(
                "spotify_artist_image_ttl_check",
                t0 + EPISODE_ARTIST_TTL_SECS
            )
            .expect("read at 7d")
            .is_none());
    }

    #[test]
    fn normalize_playlist_strips_html_and_caches_owner_30d() {
        let (_dir, cache) = temp_cache();
        let raw_playlist = json!({
            "id": "pl_42",
            "uri": "spotify:playlist:pl_42",
            "name": "Weekend Mix",
            "description": "Curated <b>beats</b> &amp; <a href=\"https://x\">more</a>",
            "snapshot_id": "snap_1",
            "collaborative": true,
            "owner": {
                "id": "alice_spotify",
                "display_name": "Alice"
            },
            "images": [
                {"url": "https://img/pl_640.jpg", "height": 640, "width": 640}
            ],
            "tracks": {"total": 85}
        });

        let playlist = normalize_playlist(&raw_playlist, &cache).expect("playlist ok");
        assert_eq!(playlist.creator, "alice_spotify");
        assert_eq!(playlist.owner_display_name.as_deref(), Some("Alice"));
        assert_eq!(playlist.description, "Curated beats &amp; more");
        assert_eq!(playlist.image, "https://img/pl_640.jpg");
        assert!(playlist.collaborative);
        assert_eq!(playlist.tracks_total, 85);

        let owner_val = cache
            .ephemeral_value("playlist_owner_pl_42")
            .expect("read owner")
            .expect("owner cached");
        assert_eq!(owner_val, Value::String(String::from("alice_spotify")));
        assert_eq!(PLAYLIST_OWNER_TTL_SECS, 30 * 86_400);
    }

    #[test]
    fn normalize_show_and_episode_map_track_view_and_cache_7d() {
        let (_dir, cache) = temp_cache();
        let raw_show = json!({
            "id": "show_1",
            "uri": "spotify:show:show_1",
            "name": "Rustacean Station",
            "publisher": "Rust Community",
            "description": "<p>A podcast about <em>Rust</em>.</p>",
            "total_episodes": 120,
            "media_type": "audio",
            "images": [
                {"url": "https://img/show_640.jpg", "height": 640, "width": 640}
            ],
            "episodes": {
                "items": [
                    {
                        "id": "ep_1",
                        "uri": "spotify:episode:ep_1",
                        "name": "Async Traits",
                        "description": "Deep dive into <b>AFIT</b>.",
                        "duration_ms": 1800000,
                        "release_date": "2026-02-01",
                        "explicit": false,
                        "is_playable": false,
                        "episode": false,
                        "resume_point": {"fully_played": false, "resume_position_ms": 45000}
                    }
                ]
            }
        });

        let show = normalize_show(&raw_show, &cache, MetadataWriteMode::MergeExisting)
            .expect("show normalizes");
        assert_eq!(show.description, "A podcast about Rust.");
        assert_eq!(show.episodes.len(), 1);

        let ep = &show.episodes[0];
        assert_eq!(ep.description, "Deep dive into AFIT.");
        assert!(
            ep.is_playable,
            "Spotify playlist episode bug (is_playable=false, episode=false) must stay playable"
        );
        assert_eq!(ep.show_name, "Rustacean Station");
        assert_eq!(ep.show_uri, "spotify:show:show_1");
        assert_eq!(ep.publisher, "Rust Community");
        assert_eq!(ep.image, "https://img/show_640.jpg");
        assert_eq!(ep.resume_position_ms, Some(45000));

        let track_view = ep.to_track_view();
        assert_eq!(track_view.album.name, "Rustacean Station");
        assert_eq!(track_view.album.uri, "spotify:show:show_1");
        assert_eq!(track_view.artists[0].name, "Rust Community");

        let cached = cache
            .ephemeral_value("spotify:episode:ep_1")
            .expect("read cached episode")
            .expect("episode cached");
        assert_eq!(cached["album"]["name"], "Rustacean Station");
        assert_eq!(cached["artists"][0]["name"], "Rust Community");

        // FastSkipIfPresent keeps existing episode entry until 7d expiry.
        let standalone_ep = json!({
            "id": "ep_1",
            "uri": "spotify:episode:ep_1",
            "name": "Overwritten Name",
            "description": "Updated"
        });
        let _skipped = normalize_episode(
            &standalone_ep,
            None,
            &cache,
            MetadataWriteMode::FastSkipIfPresent,
        )
        .expect("fast skip ok");
        let still_cached = cache
            .ephemeral_value("spotify:episode:ep_1")
            .expect("read skipped")
            .expect("present");
        assert_eq!(still_cached["name"], "Async Traits");
    }

    #[test]
    fn normalize_user_profile_player_category_and_search_results() {
        let (_dir, cache) = temp_cache();
        let profile = normalize_user_profile(&json!({
            "id": "jdoe",
            "uri": "spotify:user:jdoe",
            "display_name": "Jane Doe",
            "country": "US",
            "product": "premium",
            "followers": {"total": 12},
            "images": [{"url": "https://img/avatar.jpg", "height": 300, "width": 300}]
        }))
        .expect("profile ok");
        assert_eq!(profile.display_name.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.image, "https://img/avatar.jpg");
        assert_eq!(profile.followers_total, Some(12));

        let category = normalize_browse_category(&json!({
            "id": "jazz",
            "name": "Jazz",
            "icons": [{"url": "https://img/jazz.jpg", "height": 274, "width": 274}]
        }))
        .expect("category ok");
        assert_eq!(category.id, "jazz");
        assert_eq!(category.image, "https://img/jazz.jpg");

        let normalizer = EntityNormalizer::new(TagCleanupPolicy::PreserveRaw);
        let player = normalizer
            .normalize_player_state(
                &json!({
                    "is_playing": true,
                    "progress_ms": 12345,
                    "shuffle_state": true,
                    "repeat_state": "context",
                    "currently_playing_type": "track",
                    "device": {
                        "id": "dev1",
                        "name": "Living Room",
                        "type": "Speaker",
                        "is_active": true,
                        "volume_percent": 65
                    },
                    "context": {"uri": "spotify:album:a1"},
                    "item": {
                        "type": "track",
                        "id": "t1",
                        "uri": "spotify:track:t1",
                        "name": "Track 1",
                        "album": {
                            "id": "a1",
                            "uri": "spotify:album:a1",
                            "name": "Album 1"
                        }
                    }
                }),
                &cache,
                MetadataWriteMode::OverwriteDirect,
            )
            .expect("player ok");
        assert!(player.is_playing);
        assert_eq!(player.context_uri.as_deref(), Some("spotify:album:a1"));
        assert_eq!(
            player.track.as_ref().map(|t| t.name.as_str()),
            Some("Track 1")
        );

        let search = normalizer
            .normalize_search_results(
                &json!({
                    "tracks": {
                        "total": 1,
                        "next": null,
                        "items": [{
                            "id": "t_search",
                            "uri": "spotify:track:t_search",
                            "name": "Search Hit",
                            "album": {"id": "a_s", "uri": "spotify:album:a_s", "name": "Hit Album"}
                        }]
                    }
                }),
                &cache,
                MetadataWriteMode::MergeExisting,
                &["the"],
            )
            .expect("search ok");
        assert_eq!(search.total, Some(1));
        assert_eq!(search.tracks.len(), 1);
        assert_eq!(search.tracks[0].name, "Search Hit");

        // Verify `normalize_album` helper and 7-day `Duration` helper path.
        let _alb = normalize_album(
            &json!({"id": "a_s", "uri": "spotify:album:a_s", "name": "Hit Album"}),
            &cache,
            MetadataWriteMode::MergeExisting,
        )
        .expect("album helper ok");
        assert_eq!(
            Duration::from_secs(EPISODE_ARTIST_TTL_SECS).as_secs(),
            7 * 86_400
        );
    }
}
