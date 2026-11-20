//! Library scanners for saved albums, followed artists, and playlists.
//!
//! Mirrors `Plugins::Spotty::Importer` (`scanAlbums`, `scanArtists`,
//! `scanPlaylists`, `Importer.pm:74-303`).

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use spotty_api::{
    strip_leading_articles, AlbumHydration, ArtistsScope, NormalizedAlbum, NormalizedTrack,
    PlaylistTracksOptions, SpotifyClient, SPOTIFY_PLAYLIST_MAX_ITEMS,
};
use spotty_cache::{DualCache, MetadataWriteMode};
use spotty_types::{Result, SortOrder, TagCleanupPolicy};

use crate::schema_sink::LibrarySink;
use crate::track_prep::{prepare_lms_track, LmsTrackRow};

/// Retention TTL in seconds for scanner watermarks (`86_400 * 7`, 7 days).
pub const WATERMARK_TTL_SECS: u64 = 86_400 * 7;

/// Refresh interval in seconds for Spotify-owned editorial playlists (24h).
pub const SPOTIFY_EDITORIAL_REFRESH_SECS: u64 = 86_400;

const ALBUM_STORE_CHUNK_SIZE: usize = 200;
const TEN_DIGIT_SENTINEL_LEN: usize = 10;

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Scans saved Spotify albums for `user_id`, hydrates missing artwork, stores
/// prepared LMS tracks into `sink`, and persists the 7-day album watermark.
///
/// Watermark key `spotty_latest_album_update{user_id}` is formatted as
/// `"{total}:{last_added}"`.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if Spotify API requests, cache
/// operations, or `SQLite` sink writes fail.
pub async fn scan_enabled_albums(
    api: &SpotifyClient,
    user_id: &str,
    account_tag: Option<&str>,
    cache: &DualCache,
    sink: &mut dyn LibrarySink,
    split_sep: char,
    tag_cleanup: TagCleanupPolicy,
) -> Result<usize> {
    scan_enabled_albums_at(
        api,
        user_id,
        account_tag,
        cache,
        sink,
        split_sep,
        tag_cleanup,
        current_epoch_secs(),
    )
    .await
}

/// Scans saved Spotify albums using an explicit `now_epoch_secs` clock.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if Spotify API requests, cache
/// operations, or `SQLite` sink writes fail.
#[allow(clippy::too_many_arguments)]
pub async fn scan_enabled_albums_at(
    api: &SpotifyClient,
    user_id: &str,
    account_tag: Option<&str>,
    cache: &DualCache,
    sink: &mut dyn LibrarySink,
    split_sep: char,
    tag_cleanup: TagCleanupPolicy,
    now_epoch_secs: u64,
) -> Result<usize> {
    let meta = api.my_albums_meta().await?;
    let mut albums = api
        .my_albums(
            MetadataWriteMode::MergeExisting,
            AlbumHydration::HydrateLargeAlbums,
            SortOrder::NaturalOrder,
            SPOTIFY_PLAYLIST_MAX_ITEMS,
        )
        .await?;

    let mut missing_ids = Vec::new();
    let mut seen_missing = HashSet::new();
    for album in &albums {
        if album.id.is_empty() {
            continue;
        }
        let needs_hydration = if album.image.is_empty() {
            true
        } else {
            match cache.metadata_value_at(&album.uri, now_epoch_secs)? {
                Some(cached) => !has_non_empty_image(&cached),
                None => true,
            }
        };
        if needs_hydration && seen_missing.insert(album.id.clone()) {
            missing_ids.push(album.id.as_str());
        }
    }

    if !missing_ids.is_empty() {
        let hydrated = api.albums_batch(&missing_ids).await?;
        merge_hydrated_albums(&mut albums, hydrated);
    }

    let last_added = meta
        .latest_added_at
        .as_deref()
        .or_else(|| albums.first().and_then(|album| album.added_at.as_deref()))
        .unwrap_or("");
    let watermark = format!("{}:{last_added}", meta.total);
    let watermark_key = format!("spotty_latest_album_update{user_id}");
    cache.put_ephemeral_at(
        &watermark_key,
        &Value::String(watermark),
        WATERMARK_TTL_SECS,
        now_epoch_secs,
    )?;

    let mut total_stored = 0_usize;
    let mut batch: Vec<LmsTrackRow> = Vec::with_capacity(ALBUM_STORE_CHUNK_SIZE);
    let mut cached_tracks: HashMap<String, Option<NormalizedTrack>> = HashMap::new();

    for album in &albums {
        if album.tracks.is_empty() {
            continue;
        }
        let mut max_disc = 1_u32;
        for track in &album.tracks {
            if track.disc_number > max_disc {
                max_disc = track.disc_number;
            }
        }
        let album_track_count = if album.total_tracks > 0 {
            album.total_tracks
        } else {
            u32::try_from(album.tracks.len()).unwrap_or(u32::MAX)
        };

        for track in &album.tracks {
            if !track.is_playable {
                continue;
            }
            let cached_track = if let Some(cached) = cached_tracks.get(&track.uri) {
                cached.as_ref()
            } else {
                let loaded = load_cached_track(cache, &track.uri, album)?;
                cached_tracks.insert(track.uri.clone(), loaded);
                cached_tracks.get(&track.uri).and_then(Option::as_ref)
            };
            let row = prepare_lms_track(
                track,
                cached_track,
                max_disc,
                album_track_count,
                split_sep,
                tag_cleanup,
                account_tag,
            );
            batch.push(row);
            if batch.len() >= ALBUM_STORE_CHUNK_SIZE {
                total_stored = total_stored.saturating_add(sink.store_tracks(&batch)?);
                batch.clear();
            }
        }
    }

    if !batch.is_empty() {
        total_stored = total_stored.saturating_add(sink.store_tracks(&batch)?);
    }

    Ok(total_stored)
}

/// Scans followed Spotify artists for `user_id`, upserts contributors into
/// `sink` with `extid = artist.uri`, and persists the 7-day MD5 watermark.
///
/// Watermark key `spotty_latest_artists_update{user_id}` is formatted as
/// `"{total}:{md5_hash}"`.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if Spotify API requests, cache
/// operations, or `SQLite` contributor upserts fail.
pub async fn scan_enabled_artists(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    sink: &mut dyn LibrarySink,
    articles: &[&str],
) -> Result<usize> {
    scan_enabled_artists_at(api, user_id, cache, sink, articles, current_epoch_secs()).await
}

/// Scans followed Spotify artists using an explicit `now_epoch_secs` clock.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if Spotify API requests, cache
/// operations, or `SQLite` contributor upserts fail.
pub async fn scan_enabled_artists_at(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    sink: &mut dyn LibrarySink,
    articles: &[&str],
    now_epoch_secs: u64,
) -> Result<usize> {
    let artists_lib = api
        .my_artists(ArtistsScope::FollowedOnly, SortOrder::NaturalOrder)
        .await?;

    let watermark = format!("{}:{}", artists_lib.artists.len(), artists_lib.md5_hash);
    let watermark_key = format!("spotty_latest_artists_update{user_id}");
    cache.put_ephemeral_at(
        &watermark_key,
        &Value::String(watermark),
        WATERMARK_TTL_SECS,
        now_epoch_secs,
    )?;

    let mut owned = Vec::with_capacity(artists_lib.artists.len());
    for artist in &artists_lib.artists {
        let normalized_sort = strip_leading_articles(&artist.name, articles);
        owned.push((artist.name.as_str(), normalized_sort, artist.uri.as_str()));
    }
    let batch: Vec<(&str, &str, &str)> = owned
        .iter()
        .map(|(name, sort, uri)| (*name, sort.as_str(), *uri))
        .collect();
    sink.upsert_contributors_batch(&batch)?;

    Ok(artists_lib.artists.len())
}

/// Scans Spotify playlists for `user_id`, hydrates uncached track metadata,
/// upserts playlists into `sink`, and persists snapshot deadlines.
///
/// Playlists whose `creator == "spotify"` record the 10-digit epoch deadline
/// sentinel `(now_epoch_secs + 86_400).to_string()` instead of `snapshot_id`
/// under `spotty_snapshot_ids{user_id}` (`WATERMARK_TTL_SECS` TTL), and set
/// `spotty_initialized_{user_id}` = `"1"`.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if Spotify API requests, cache
/// operations, or `SQLite` playlist upserts fail.
pub async fn scan_enabled_playlists(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    sink: &mut dyn LibrarySink,
    now_epoch_secs: u64,
) -> Result<usize> {
    let playlists = api.my_playlists(SPOTIFY_PLAYLIST_MAX_ITEMS).await?;
    let init_key = format!("spotty_initialized_{user_id}");
    let is_initialized = cache
        .metadata_value_at(&init_key, now_epoch_secs)?
        .is_some()
        || cache
            .ephemeral_value_at(&init_key, now_epoch_secs)?
            .is_some();

    let snapshot_key = format!("spotty_snapshot_ids{user_id}");
    let cached_snapshots = cache.ephemeral_value_at(&snapshot_key, now_epoch_secs)?;
    let snapshot_map = cached_snapshots.as_ref().and_then(Value::as_object);

    let mut playlist_tracks_by_id: Vec<(usize, Vec<String>)> = Vec::with_capacity(playlists.len());
    let mut track_has_image: HashMap<String, bool> = HashMap::new();
    let mut missing_uris: Vec<String> = Vec::new();

    for (idx, playlist) in playlists.iter().enumerate() {
        if playlist.id.is_empty() {
            continue;
        }
        let cached_uris_key = format!("spotty_playlist_tracks_{}", playlist.id);
        if is_initialized && playlist_snapshot_unchanged(playlist, snapshot_map, now_epoch_secs) {
            if let Some(cached_uris) =
                load_cached_playlist_uris(cache, &cached_uris_key, now_epoch_secs)?
            {
                playlist_tracks_by_id.push((idx, cached_uris));
                continue;
            }
        }

        let uris = if is_initialized {
            api.playlist_track_uris(&playlist.id).await?
        } else {
            let opts = PlaylistTracksOptions::new();
            let full = api.playlist_tracks(&playlist.id, &opts).await?;
            let mut extracted = Vec::with_capacity(full.len());
            for track in full {
                if !track.uri.is_empty() {
                    extracted.push(track.uri);
                }
            }
            extracted
        };

        let mut uri_values = Vec::with_capacity(uris.len());
        for uri in &uris {
            uri_values.push(Value::String(uri.clone()));
            if track_has_image.contains_key(uri) {
                continue;
            }
            let has_image = cache_entry_has_image(cache, uri, now_epoch_secs)?;
            track_has_image.insert(uri.clone(), has_image);
            if !has_image {
                missing_uris.push(uri.clone());
            }
        }
        cache.put_ephemeral_at(
            &cached_uris_key,
            &Value::Array(uri_values),
            WATERMARK_TTL_SECS,
            now_epoch_secs,
        )?;
        playlist_tracks_by_id.push((idx, uris));
    }

    if !missing_uris.is_empty() {
        let mut uri_refs = Vec::with_capacity(missing_uris.len());
        for uri in &missing_uris {
            uri_refs.push(uri.as_str());
        }
        let _hydrated = api.tracks_and_episodes(&uri_refs).await?;
    }

    let upserted_count = playlist_tracks_by_id.len();
    let batch: Vec<(&str, &str, &str, &[String])> = playlist_tracks_by_id
        .iter()
        .map(|(idx, track_uris)| {
            let playlist = &playlists[*idx];
            (
                playlist.uri.as_str(),
                playlist.name.as_str(),
                playlist.image.as_str(),
                track_uris.as_slice(),
            )
        })
        .collect();
    sink.upsert_playlists_batch(&batch)?;

    let editorial_deadline = now_epoch_secs
        .saturating_add(SPOTIFY_EDITORIAL_REFRESH_SECS)
        .to_string();
    let mut snapshot_map = Map::with_capacity(playlists.len());
    for playlist in &playlists {
        if playlist.id.is_empty() {
            continue;
        }
        let stored_value = if playlist.creator == "spotify" {
            editorial_deadline.clone()
        } else {
            playlist.snapshot_id.clone()
        };
        snapshot_map.insert(playlist.id.clone(), Value::String(stored_value));
    }

    cache.put_ephemeral_at(
        &snapshot_key,
        &Value::Object(snapshot_map),
        WATERMARK_TTL_SECS,
        now_epoch_secs,
    )?;
    cache.store_metadata_at(
        &init_key,
        None,
        &Value::String(String::from("1")),
        MetadataWriteMode::OverwriteDirect,
        now_epoch_secs,
    )?;

    Ok(upserted_count)
}

fn playlist_snapshot_unchanged(
    playlist: &spotty_api::NormalizedPlaylist,
    snapshot_map: Option<&Map<String, Value>>,
    now_epoch_secs: u64,
) -> bool {
    let Some(stored) = snapshot_map
        .and_then(|map| map.get(&playlist.id))
        .and_then(Value::as_str)
    else {
        return false;
    };
    if stored.is_empty() {
        return false;
    }
    if playlist.creator == "spotify" {
        return stored.len() == TEN_DIGIT_SENTINEL_LEN
            && stored.bytes().all(|b| b.is_ascii_digit())
            && stored
                .parse::<u64>()
                .is_ok_and(|deadline| deadline >= now_epoch_secs);
    }
    stored == playlist.snapshot_id
}

fn load_cached_playlist_uris(
    cache: &DualCache,
    cached_uris_key: &str,
    now_epoch_secs: u64,
) -> Result<Option<Vec<String>>> {
    let Some(Value::Array(items)) = cache.ephemeral_value_at(cached_uris_key, now_epoch_secs)?
    else {
        return Ok(None);
    };
    let mut uris = Vec::with_capacity(items.len());
    for item in items {
        let Some(uri) = item.as_str() else {
            return Ok(None);
        };
        uris.push(uri.to_owned());
    }
    Ok(Some(uris))
}

fn has_non_empty_image(val: &Value) -> bool {
    val.get("image")
        .and_then(Value::as_str)
        .is_some_and(|img| !img.is_empty())
}

fn cache_entry_has_image(cache: &DualCache, uri: &str, now_epoch_secs: u64) -> Result<bool> {
    if let Some(meta) = cache.metadata_value_at(uri, now_epoch_secs)? {
        if has_non_empty_image(&meta) {
            return Ok(true);
        }
    }
    if let Some(ephemeral) = cache.ephemeral_value_at(uri, now_epoch_secs)? {
        if has_non_empty_image(&ephemeral) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn merge_hydrated_albums(albums: &mut [NormalizedAlbum], hydrated: Vec<NormalizedAlbum>) {
    let mut by_id = HashMap::with_capacity(hydrated.len());
    for album in hydrated {
        if !album.id.is_empty() {
            by_id.insert(album.id.clone(), album);
        }
    }

    for album in albums {
        let Some(fresh) = by_id.get(&album.id) else {
            continue;
        };
        let preserved_added_at = album.added_at.clone();
        if album.image.is_empty() && !fresh.image.is_empty() {
            album.image = fresh.image.clone();
        }
        if album.tracks.is_empty() && !fresh.tracks.is_empty() {
            album.tracks = fresh.tracks.clone();
        } else {
            for track in &mut album.tracks {
                if track.album.image.is_empty() && !fresh.image.is_empty() {
                    track.album.image = fresh.image.clone();
                }
                if track.image.is_empty() && !fresh.image.is_empty() {
                    track.image = fresh.image.clone();
                }
            }
        }
        if album.added_at.is_none() {
            album.added_at = preserved_added_at;
        }
        for track in &mut album.tracks {
            if track.album.added_at.is_none() {
                track.album.added_at = album.added_at.clone();
            }
        }
    }
}

fn load_cached_track(
    cache: &DualCache,
    uri: &str,
    album: &NormalizedAlbum,
) -> Result<Option<NormalizedTrack>> {
    let Some(raw) = cache.metadata_value(uri)? else {
        return Ok(None);
    };
    let Ok(mut parsed) = serde_json::from_value::<NormalizedTrack>(raw) else {
        return Ok(None);
    };
    if parsed.album.added_at.is_none() {
        parsed.album.added_at = album.added_at.clone();
    }
    if parsed.album.image.is_empty() && !album.image.is_empty() {
        parsed.album.image = album.image.clone();
    }
    if parsed.image.is_empty() && !album.image.is_empty() {
        parsed.image = album.image.clone();
    }
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use spotty_api::{
        compute_artists_md5_hash, NormalizedArtist, Pipeline, RateLimitGate, RateLimitPolicy,
        SpotifyClient, SpotifyTransport, TokenManager,
    };
    use spotty_cache::{AccountStore, DualCache};
    use spotty_types::{IconCode, TagCleanupPolicy};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        scan_enabled_albums, scan_enabled_artists, scan_enabled_playlists,
        SPOTIFY_EDITORIAL_REFRESH_SECS,
    };
    use crate::schema_sink::{LibrarySink, SqliteLibrarySink};

    fn artist_with_id(id: &str) -> NormalizedArtist {
        let mut artist = NormalizedArtist::default();
        artist.id = String::from(id);
        artist
    }

    fn test_client(
        mock_uri: &str,
        tmp: &TempDir,
        user_id: &str,
    ) -> (SpotifyClient, Arc<DualCache>) {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let accounts = Arc::new(AccountStore::new(tmp.path()));
        let tokens =
            Arc::new(TokenManager::new(Arc::clone(&cache), accounts).expect("create TokenManager"));
        let client_id = IconCode::default();
        tokens
            .cache_tokens(&client_id, user_id, "tok-test", 3600, None)
            .expect("seed token");

        let gate = Arc::new(RateLimitGate::new());
        let transport = Arc::new(
            SpotifyTransport::with_base_url(
                format!("{mock_uri}/v1"),
                tokens,
                Arc::clone(&cache),
                gate,
                client_id.clone(),
                user_id,
            )
            .expect("create SpotifyTransport"),
        );
        let pipeline = Pipeline::new(Arc::clone(&transport));
        let client = SpotifyClient::new(
            transport,
            pipeline,
            Arc::clone(&cache),
            client_id,
            user_id,
            "US",
            RateLimitPolicy::FailFastServer,
        );
        (client, cache)
    }

    #[tokio::test]
    async fn scan_enabled_albums_hydrates_missing_artwork_and_writes_watermark() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let (client, cache) = test_client(&server.uri(), &tmp, "alice");
        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");
        sink.init_online_tracks_table().expect("init online_tracks");

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "items": [{
                    "added_at": "2026-11-18T12:00:00Z",
                    "album": { "id": "albMissingImg" }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

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
                            "id": "albMissingImg",
                            "uri": "spotify:album:albMissingImg",
                            "name": "No Cover Yet",
                            "album_type": "single",
                            "release_date": "2026-01-01",
                            "total_tracks": 3,
                            "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "A1"}],
                            "images": [],
                            "tracks": {
                                "items": [
                                    {
                                        "id": "t1",
                                        "uri": "spotify:track:t1",
                                        "name": "Track One (Remastered)",
                                        "duration_ms": 120000,
                                        "track_number": 1,
                                        "disc_number": 1,
                                        "is_playable": true,
                                        "artists": [{
                                            "id": "a1",
                                            "uri": "spotify:artist:a1",
                                            "name": "A1"
                                        }]
                                    }
                                ]
                            }
                        }
                    },
                    {
                        "added_at": "2026-11-10T00:00:00Z",
                        "album": {
                            "id": "albWithImg",
                            "uri": "spotify:album:albWithImg",
                            "name": "Has Cover",
                            "album_type": "album",
                            "release_date": "2025-05-01",
                            "total_tracks": 1,
                            "artists": [{"id": "a2", "uri": "spotify:artist:a2", "name": "A2"}],
                            "images": [{
                                "url": "https://img.example/has_cover.jpg",
                                "width": 640,
                                "height": 640
                            }],
                            "tracks": {
                                "items": [
                                    {
                                        "id": "t2",
                                        "uri": "spotify:track:t2",
                                        "name": "Track Two",
                                        "duration_ms": 180000,
                                        "track_number": 1,
                                        "disc_number": 1,
                                        "is_playable": true,
                                        "artists": [{
                                            "id": "a2",
                                            "uri": "spotify:artist:a2",
                                            "name": "A2"
                                        }]
                                    }
                                ]
                            }
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/albums"))
            .and(query_param("ids", "albMissingImg"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "albums": [
                    {
                        "id": "albMissingImg",
                        "uri": "spotify:album:albMissingImg",
                        "name": "No Cover Yet",
                        "album_type": "single",
                        "release_date": "2026-01-01",
                        "total_tracks": 3,
                        "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "A1"}],
                        "images": [{
                            "url": "https://img.example/hydrated.jpg",
                            "width": 640,
                            "height": 640
                        }],
                        "tracks": {
                            "items": [
                                {
                                    "id": "t1",
                                    "uri": "spotify:track:t1",
                                    "name": "Track One (Remastered)",
                                    "duration_ms": 120000,
                                    "track_number": 1,
                                    "disc_number": 1,
                                    "is_playable": true,
                                    "artists": [{
                                        "id": "a1",
                                        "uri": "spotify:artist:a1",
                                        "name": "A1"
                                    }]
                                }
                            ]
                        }
                    }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let stored = scan_enabled_albums(
            &client,
            "alice",
            Some("spotify:alice"),
            &cache,
            &mut sink,
            ';',
            TagCleanupPolicy::StripSuffixes,
        )
        .await
        .expect("scan_enabled_albums succeeds");
        assert_eq!(stored, 2, "expected 2 tracks stored, got {stored}");

        let watermark = cache
            .ephemeral_value("spotty_latest_album_updatealice")
            .expect("read album watermark")
            .expect("album watermark present");
        assert_eq!(watermark.as_str(), Some("2:2026-11-18T12:00:00Z"));

        let row: (String, String, String, String) = sink.with_connection(|conn| {
            conn.query_row(
                "SELECT title, cover, release_type, account_tag
                 FROM tracks WHERE url = 'spotify:track:t1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("query track t1")
        });
        assert_eq!(row.0, "Track One");
        assert_eq!(row.1, "https://img.example/hydrated.jpg");
        assert_eq!(row.2, "EP");
        assert_eq!(row.3, "spotify:alice");
    }

    #[tokio::test]
    async fn scan_enabled_artists_upserts_contributors_and_writes_md5_watermark() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let (client, cache) = test_client(&server.uri(), &tmp, "alice");
        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "items": [
                        {
                            "id": "artB",
                            "uri": "spotify:artist:artB",
                            "name": "The Beta"
                        },
                        {
                            "id": "artA",
                            "uri": "spotify:artist:artA",
                            "name": "Alpha"
                        }
                    ],
                    "cursors": {"after": null},
                    "next": null
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let count = scan_enabled_artists(&client, "alice", &cache, &mut sink, &["The", "A", "An"])
            .await
            .expect("scan_enabled_artists succeeds");
        assert_eq!(count, 2);

        let expected_hash =
            compute_artists_md5_hash(&[artist_with_id("artB"), artist_with_id("artA")]);
        let watermark = cache
            .ephemeral_value("spotty_latest_artists_updatealice")
            .expect("read artist watermark")
            .expect("artist watermark present");
        assert_eq!(
            watermark.as_str(),
            Some(format!("2:{expected_hash}").as_str())
        );

        let namesort: String = sink.with_connection(|conn| {
            conn.query_row(
                "SELECT namesort FROM contributors WHERE extid = 'spotify:artist:artB'",
                [],
                |r| r.get(0),
            )
            .expect("query contributor")
        });
        assert_eq!(namesort, "Beta");
    }

    #[tokio::test]
    async fn scan_enabled_playlists_sets_24h_sentinel_and_uses_fast_projection_on_rerun() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let (client, cache) = test_client(&server.uri(), &tmp, "alice");
        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");
        let t0 = 1_700_000_000_u64;

        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "next": null,
                "items": [
                    {
                        "id": "plEd000000000000000001",
                        "uri": "spotify:playlist:plEd000000000000000001",
                        "name": "Discover Weekly",
                        "owner": {"id": "spotify"},
                        "snapshot_id": "snap-ed-1",
                        "images": [{"url": "https://img.example/ed.jpg", "width": 640, "height": 640}],
                        "tracks": {"total": 1}
                    },
                    {
                        "id": "plUser0000000000000001",
                        "uri": "spotify:playlist:plUser0000000000000001",
                        "name": "Road Trip",
                        "owner": {"id": "alice"},
                        "snapshot_id": "snap-user-1",
                        "images": [{"url": "https://img.example/user.jpg", "width": 640, "height": 640}],
                        "tracks": {"total": 1}
                    }
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/playlists/plEd000000000000000001/tracks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "track": {
                        "id": "trk1111111111111111111",
                        "uri": "spotify:track:trk1111111111111111111",
                        "name": "Editorial Song",
                        "is_playable": true,
                        "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "A1"}],
                        "album": {
                            "id": "al1",
                            "uri": "spotify:album:al1",
                            "name": "Al1",
                            "images": [{
                                "url": "https://img.example/al1.jpg",
                                "width": 640,
                                "height": 640
                            }]
                        }
                    }
                }]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/playlists/plUser0000000000000001/tracks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "track": {
                        "id": "ep11111111111111111111",
                        "uri": "spotify:episode:ep11111111111111111111",
                        "type": "episode",
                        "name": "Podcast Ep",
                        "is_playable": true,
                        "show": {
                            "id": "sh1",
                            "uri": "spotify:show:sh1",
                            "name": "Show 1",
                            "publisher": "Pub",
                            "images": []
                        }
                    }
                }]
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/episodes"))
            .and(query_param("ids", "ep11111111111111111111"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "episodes": [{
                    "id": "ep11111111111111111111",
                    "uri": "spotify:episode:ep11111111111111111111",
                    "name": "Podcast Ep",
                    "duration_ms": 600000,
                    "is_playable": true,
                    "show": {
                        "id": "sh1",
                        "uri": "spotify:show:sh1",
                        "name": "Show 1",
                        "publisher": "Pub",
                        "images": [{
                            "url": "https://img.example/sh1.jpg",
                            "width": 640,
                            "height": 640
                        }]
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let count = scan_enabled_playlists(&client, "alice", &cache, &mut sink, t0)
            .await
            .expect("first playlist scan succeeds");
        assert_eq!(count, 2);

        let init_flag = cache
            .metadata_value_at("spotty_initialized_alice", t0)
            .expect("read init flag")
            .expect("init flag set");
        assert_eq!(init_flag.as_str(), Some("1"));

        let snap_obj = cache
            .ephemeral_value_at("spotty_snapshot_idsalice", t0)
            .expect("read snapshot map")
            .expect("snapshot map set");
        let expected_deadline = (t0 + SPOTIFY_EDITORIAL_REFRESH_SECS).to_string();
        assert_eq!(
            snap_obj
                .get("plEd000000000000000001")
                .and_then(|v| v.as_str()),
            Some(expected_deadline.as_str())
        );
        assert_eq!(
            snap_obj
                .get("plUser0000000000000001")
                .and_then(|v| v.as_str()),
            Some("snap-user-1")
        );

        let playlist_title: String = sink.with_connection(|conn| {
            conn.query_row(
                "SELECT title FROM tracks WHERE url = 'spotify:playlist:plEd000000000000000001'",
                [],
                |r| r.get(0),
            )
            .expect("query playlist title")
        });
        assert_eq!(playlist_title, "Spotify: Discover Weekly");

        // Second scan must use fast fields projection now that initialized=1.
        let fast_server = MockServer::start().await;
        let fast_tmp = TempDir::new().expect("create fast tempdir");
        let (fast_client, _) = test_client(&fast_server.uri(), &fast_tmp, "alice");
        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "id": "plEd000000000000000001",
                    "uri": "spotify:playlist:plEd000000000000000001",
                    "name": "Discover Weekly",
                    "owner": {"id": "spotify"},
                    "snapshot_id": "snap-ed-2",
                    "images": [],
                    "tracks": {"total": 1}
                }]
            })))
            .expect(1)
            .mount(&fast_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/playlists/plEd000000000000000001/tracks"))
            .and(query_param(
                "fields",
                "next,total,items(track(uri,restrictions,is_local))",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{"track": {"uri": "spotify:track:trk1111111111111111111"}}]
            })))
            .expect(1)
            .mount(&fast_server)
            .await;

        let rerun = scan_enabled_playlists(
            &fast_client,
            "alice",
            &cache,
            &mut sink,
            t0 + SPOTIFY_EDITORIAL_REFRESH_SECS + 1,
        )
        .await
        .expect("fast rerun succeeds");
        assert_eq!(rerun, 1);
    }

    #[tokio::test]
    async fn scan_enabled_playlists_skips_unchanged_snapshot_fetches() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let (client, cache) = test_client(&server.uri(), &tmp, "alice");
        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");
        let t0 = 1_750_000_000_u64;

        cache
            .store_metadata_at(
                "spotty_initialized_alice",
                None,
                &json!("1"),
                super::MetadataWriteMode::OverwriteDirect,
                t0,
            )
            .expect("seed init flag");
        cache
            .put_ephemeral_at(
                "spotty_snapshot_idsalice",
                &json!({ "plUser1": "snap-1" }),
                super::WATERMARK_TTL_SECS,
                t0,
            )
            .expect("seed snapshot");
        cache
            .put_ephemeral_at(
                "spotty_playlist_tracks_plUser1",
                &json!(["spotify:track:trk1"]),
                super::WATERMARK_TTL_SECS,
                t0,
            )
            .expect("seed cached uris");

        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "id": "plUser1",
                    "uri": "spotify:playlist:plUser1",
                    "name": "Unchanged Mix",
                    "owner": {"id": "alice"},
                    "snapshot_id": "snap-1",
                    "images": [],
                    "tracks": {"total": 1}
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/playlists/plUser1/tracks"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let count = scan_enabled_playlists(&client, "alice", &cache, &mut sink, t0 + 10)
            .await
            .expect("unchanged playlist scan succeeds");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn scan_enabled_albums_persists_meta_total_in_watermark() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let (client, cache) = test_client(&server.uri(), &tmp, "alice");
        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");
        sink.init_online_tracks_table().expect("init online_tracks");

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 42,
                "items": [{
                    "added_at": "2026-11-18T12:00:00Z",
                    "album": { "id": "alb1" }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let t0 = 1_750_000_000_u64;
        super::scan_enabled_albums_at(
            &client,
            "alice",
            None,
            &cache,
            &mut sink,
            ';',
            TagCleanupPolicy::StripSuffixes,
            t0,
        )
        .await
        .expect("scan_enabled_albums_at succeeds");

        let wm = cache
            .ephemeral_value_at("spotty_latest_album_updatealice", t0 + 5)
            .expect("read album watermark")
            .expect("album watermark present");
        assert_eq!(wm.as_str(), Some("42:2026-11-18T12:00:00Z"));
    }
}
