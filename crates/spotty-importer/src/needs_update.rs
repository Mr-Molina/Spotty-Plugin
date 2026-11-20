//! Online library change detection for LMS incremental rescan triggers.
//!
//! Mirrors `Plugins::Spotty::Importer::needsUpdate` (`Importer.pm:323-442`).

use std::sync::Arc;

use serde_json::Value;
use spotty_api::{ArtistsScope, SpotifyClient, SPOTIFY_PLAYLIST_MAX_ITEMS};
use spotty_cache::DualCache;
use spotty_types::{Result, SortOrder};

use crate::runner::PlaylistSyncPolicy;

const TEN_DIGIT_SENTINEL_LEN: usize = 10;

/// Checks whether any enabled account requires an online-library rescan.
///
/// Short-circuits on the first account needing update across:
/// 1. Playlist `snapshot_id` mismatch or 10-digit epoch sentinel
///    `< now_epoch_secs` (when `playlist_policy` is
///    [`PlaylistSyncPolicy::IncludePlaylists`]),
/// 2. `my_albums_meta` watermark (`spotty_latest_album_update{user_id}`)
///    mismatch,
/// 3. `my_artists` MD5 watermark (`spotty_latest_artists_update{user_id}`)
///    mismatch.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if cache reads or Spotify API requests
/// fail.
pub async fn online_library_needs_update(
    accounts: &[(String, Arc<SpotifyClient>)],
    cache: &DualCache,
    playlist_policy: PlaylistSyncPolicy,
    now_epoch_secs: u64,
) -> Result<bool> {
    for (user_id, api) in accounts {
        if playlist_policy == PlaylistSyncPolicy::IncludePlaylists
            && playlists_need_update(api, user_id, cache, now_epoch_secs).await?
        {
            return Ok(true);
        }

        if albums_need_update(api, user_id, cache, now_epoch_secs).await? {
            return Ok(true);
        }

        if artists_need_update(api, user_id, cache, now_epoch_secs).await? {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn playlists_need_update(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    now_epoch_secs: u64,
) -> Result<bool> {
    let snap_key = format!("spotty_snapshot_ids{user_id}");
    let cached_snapshots = cache.ephemeral_value_at(&snap_key, now_epoch_secs)?;
    let snapshot_map = cached_snapshots.as_ref().and_then(Value::as_object);

    let playlists = api.my_playlists(SPOTIFY_PLAYLIST_MAX_ITEMS).await?;
    for playlist in &playlists {
        if playlist.id.is_empty() {
            continue;
        }
        let stored = snapshot_map
            .and_then(|map| map.get(&playlist.id))
            .and_then(Value::as_str);

        let Some(stored_id) = stored else {
            return Ok(true);
        };
        if stored_id.is_empty() {
            return Ok(true);
        }

        if is_ten_digit_sentinel(stored_id) {
            if let Ok(deadline_secs) = stored_id.parse::<u64>() {
                if deadline_secs < now_epoch_secs {
                    return Ok(true);
                }
            }
        } else if stored_id != playlist.snapshot_id {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn albums_need_update(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    now_epoch_secs: u64,
) -> Result<bool> {
    let key = format!("spotty_latest_album_update{user_id}");
    let last_update = cache
        .ephemeral_value_at(&key, now_epoch_secs)?
        .and_then(|val| val.as_str().map(str::to_owned))
        .unwrap_or_default();

    let meta = api.my_albums_meta().await?;
    let latest_added = meta.latest_added_at.as_deref().unwrap_or("");
    let latest = format!("{}:{latest_added}", meta.total);

    Ok(latest != last_update)
}

async fn artists_need_update(
    api: &SpotifyClient,
    user_id: &str,
    cache: &DualCache,
    now_epoch_secs: u64,
) -> Result<bool> {
    let key = format!("spotty_latest_artists_update{user_id}");
    let last_update = cache
        .ephemeral_value_at(&key, now_epoch_secs)?
        .and_then(|val| val.as_str().map(str::to_owned))
        .unwrap_or_default();

    let artists_lib = api
        .my_artists(ArtistsScope::FollowedOnly, SortOrder::NaturalOrder)
        .await?;
    let latest = format!("{}:{}", artists_lib.artists.len(), artists_lib.md5_hash);

    Ok(latest != last_update)
}

fn is_ten_digit_sentinel(value: &str) -> bool {
    value.len() == TEN_DIGIT_SENTINEL_LEN && value.bytes().all(|byte| byte.is_ascii_digit())
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
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::online_library_needs_update;
    use crate::runner::PlaylistSyncPolicy;
    use crate::scanners::{SPOTIFY_EDITORIAL_REFRESH_SECS, WATERMARK_TTL_SECS};

    fn artist_with_id(id: &str) -> NormalizedArtist {
        let mut artist = NormalizedArtist::default();
        artist.id = String::from(id);
        artist
    }

    fn test_client(mock_uri: &str, tmp: &TempDir, user_id: &str) -> Arc<SpotifyClient> {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("http_cache.redb")).expect("open DualCache"));
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
        Arc::new(SpotifyClient::new(
            transport,
            pipeline,
            cache,
            client_id,
            user_id,
            "US",
            RateLimitPolicy::FailFastServer,
        ))
    }

    fn seed_matching_watermarks(
        watermark_cache: &DualCache,
        user_id: &str,
        editorial_deadline: u64,
        user_snapshot: &str,
        album_watermark: &str,
        artist_watermark: &str,
        now_epoch_secs: u64,
    ) {
        watermark_cache
            .put_ephemeral_at(
                &format!("spotty_snapshot_ids{user_id}"),
                &json!({
                    "plEd000000000000000001": editorial_deadline.to_string(),
                    "plUser0000000000000001": user_snapshot,
                }),
                WATERMARK_TTL_SECS,
                now_epoch_secs,
            )
            .expect("seed snapshot map");
        watermark_cache
            .put_ephemeral_at(
                &format!("spotty_latest_album_update{user_id}"),
                &json!(album_watermark),
                WATERMARK_TTL_SECS,
                now_epoch_secs,
            )
            .expect("seed album watermark");
        watermark_cache
            .put_ephemeral_at(
                &format!("spotty_latest_artists_update{user_id}"),
                &json!(artist_watermark),
                WATERMARK_TTL_SECS,
                now_epoch_secs,
            )
            .expect("seed artist watermark");
    }

    async fn mount_standard_endpoints(
        server: &MockServer,
        editorial_snap: &str,
        user_snap: &str,
        album_total: u64,
        album_added_at: &str,
        artist_id: &str,
    ) {
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
                        "snapshot_id": editorial_snap,
                        "images": [],
                        "tracks": {"total": 1}
                    },
                    {
                        "id": "plUser0000000000000001",
                        "uri": "spotify:playlist:plUser0000000000000001",
                        "name": "My List",
                        "owner": {"id": "alice"},
                        "snapshot_id": user_snap,
                        "images": [],
                        "tracks": {"total": 1}
                    }
                ]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": album_total,
                "items": [{
                    "added_at": album_added_at,
                    "album": {"id": "alb1"}
                }]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "items": [{
                        "id": artist_id,
                        "uri": format!("spotify:artist:{artist_id}"),
                        "name": "Artist"
                    }],
                    "cursors": {"after": null},
                    "next": null
                }
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn editorial_24h_sentinel_before_and_after_expiry_pins_review_focus_6() {
        let server = MockServer::start().await;
        let tmp_client = TempDir::new().expect("create client tempdir");
        let tmp_wm = TempDir::new().expect("create watermark tempdir");
        let wm_cache = DualCache::open(tmp_wm.path().join("wm.redb")).expect("open wm cache");
        let client = test_client(&server.uri(), &tmp_client, "alice");

        let t0 = 1_700_000_000_u64;
        let deadline = t0 + SPOTIFY_EDITORIAL_REFRESH_SECS;
        let artist_hash = compute_artists_md5_hash(&[artist_with_id("art1")]);
        seed_matching_watermarks(
            &wm_cache,
            "alice",
            deadline,
            "snap-user-1",
            "5:2026-11-18T12:00:00Z",
            &format!("1:{artist_hash}"),
            t0,
        );

        // Editorial snapshot_id changed on Spotify, yet before 24h deadline
        // the 10-digit sentinel suppresses unnecessary rescans.
        mount_standard_endpoints(
            &server,
            "snap-ed-changed",
            "snap-user-1",
            5,
            "2026-11-18T12:00:00Z",
            "art1",
        )
        .await;

        let accounts = vec![(String::from("alice"), client)];

        let before_expiry = online_library_needs_update(
            &accounts,
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            deadline - 1,
        )
        .await
        .expect("check before expiry");
        assert!(
            !before_expiry,
            "editorial 24h sentinel must stay clean before expiry"
        );

        let at_boundary = online_library_needs_update(
            &accounts,
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            deadline,
        )
        .await
        .expect("check at exact deadline");
        assert!(
            !at_boundary,
            "editorial 24h sentinel must stay clean when deadline == now"
        );

        let after_expiry = online_library_needs_update(
            &accounts,
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            deadline + 1,
        )
        .await
        .expect("check after expiry");
        assert!(
            after_expiry,
            "editorial 24h sentinel must trigger update once now > deadline"
        );
    }

    #[tokio::test]
    async fn snapshot_mismatch_short_circuits_and_ignore_playlists_skips_it() {
        let server = MockServer::start().await;
        let tmp_client = TempDir::new().expect("create client tempdir");
        let tmp_wm = TempDir::new().expect("create watermark tempdir");
        let wm_cache = DualCache::open(tmp_wm.path().join("wm.redb")).expect("open wm cache");
        let client = test_client(&server.uri(), &tmp_client, "alice");

        let t0 = 1_700_000_000_u64;
        let artist_hash = compute_artists_md5_hash(&[artist_with_id("art1")]);
        seed_matching_watermarks(
            &wm_cache,
            "alice",
            t0 + SPOTIFY_EDITORIAL_REFRESH_SECS,
            "snap-user-1",
            "5:2026-11-18T12:00:00Z",
            &format!("1:{artist_hash}"),
            t0,
        );

        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "id": "plUser0000000000000001",
                    "uri": "spotify:playlist:plUser0000000000000001",
                    "name": "My List",
                    "owner": {"id": "alice"},
                    "snapshot_id": "snap-user-2",
                    "images": [],
                    "tracks": {"total": 1}
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let accounts = vec![(String::from("alice"), Arc::clone(&client))];
        let needs = online_library_needs_update(
            &accounts,
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            t0 + 10,
        )
        .await
        .expect("check user playlist mismatch");
        assert!(needs, "user snapshot mismatch must trigger update");

        // Now mount album + artist endpoints matching watermarks and verify
        // IgnorePlaylists returns false without failing on playlist drift.
        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 5,
                "items": [{"added_at": "2026-11-18T12:00:00Z", "album": {"id": "alb1"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "items": [{"id": "art1", "uri": "spotify:artist:art1", "name": "Artist"}],
                    "cursors": {"after": null},
                    "next": null
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ignored = online_library_needs_update(
            &accounts,
            &wm_cache,
            PlaylistSyncPolicy::IgnorePlaylists,
            t0 + 10,
        )
        .await
        .expect("check IgnorePlaylists");
        assert!(
            !ignored,
            "IgnorePlaylists must skip playlist snapshot check"
        );
    }

    #[tokio::test]
    async fn album_meta_and_artist_md5_drift_trigger_update() {
        let server_album = MockServer::start().await;
        let tmp_client1 = TempDir::new().expect("create tempdir 1");
        let tmp_wm = TempDir::new().expect("create wm tempdir");
        let wm_cache = DualCache::open(tmp_wm.path().join("wm.redb")).expect("open wm cache");
        let client1 = test_client(&server_album.uri(), &tmp_client1, "alice");

        let t0 = 1_700_000_000_u64;
        let artist_hash = compute_artists_md5_hash(&[artist_with_id("art1")]);
        seed_matching_watermarks(
            &wm_cache,
            "alice",
            t0 + SPOTIFY_EDITORIAL_REFRESH_SECS,
            "snap-user-1",
            "5:2026-11-18T12:00:00Z",
            &format!("1:{artist_hash}"),
            t0,
        );

        mount_standard_endpoints(
            &server_album,
            "snap-ed-1",
            "snap-user-1",
            6,
            "2026-11-18T12:00:00Z",
            "art1",
        )
        .await;

        let album_drift = online_library_needs_update(
            &[(String::from("alice"), client1)],
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            t0 + 10,
        )
        .await
        .expect("check album drift");
        assert!(album_drift, "album total change must trigger update");

        let server_artist = MockServer::start().await;
        let tmp_client2 = TempDir::new().expect("create tempdir 2");
        let client2 = test_client(&server_artist.uri(), &tmp_client2, "alice");
        mount_standard_endpoints(
            &server_artist,
            "snap-ed-1",
            "snap-user-1",
            5,
            "2026-11-18T12:00:00Z",
            "art2",
        )
        .await;

        let artist_drift = online_library_needs_update(
            &[(String::from("alice"), client2)],
            &wm_cache,
            PlaylistSyncPolicy::IncludePlaylists,
            t0 + 10,
        )
        .await
        .expect("check artist drift");
        assert!(artist_drift, "artist MD5 change must trigger update");
    }
}
