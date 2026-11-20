//! End-to-end 6-journey LMS parity and live Perl-to-Rust RPC verification.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use lms_spotty::{read_endpoint_file, DaemonConfig, SpottyDaemon, SPOTTY_VERSION};
use reqwest::Client;
use rusqlite::Connection;
use serde_json::{json, Value};
use spotty_api::{
    Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
};
use spotty_cache::{AccountId, AccountStore, DualCache, AUTHENTICATE_DIR};
use spotty_importer::{
    online_library_needs_update, ImporterRunner, PlaylistSyncPolicy, ScanScope, SqliteLibrarySink,
};
use spotty_integrations::{
    resolve_home_extra_menu, DstmRecommender, LastMixService, SeedTrackInput, HOME_EXTRAS,
};
use spotty_opml::{top_chart_uri, OpmlItemKind, OpmlNode, OpmlRoute, OpmlRouter};
use spotty_playlist_folders::FolderDiscovery;
use spotty_types::{GlobalPrefs, IconCode, PlayerPrefs};
use spotty_web::{
    AuthLandingController, PkceCallbackController, DEFAULT_PLUGIN_ID, LMS_PLUGIN_ID_HEADER,
};
use tempfile::TempDir;
use url::Url;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_USER_ID: &str = "alice_spotty";
const TEST_TOKEN: &str = "e2e-access-token";
const STARTUP_EPOCH: u64 = 1_700_000_000;

fn build_mock_client(
    server_uri: &str,
    temp: &TempDir,
) -> (Arc<DualCache>, Arc<AccountStore>, SpotifyClient) {
    let spotty_dir = temp.path().join("spotty");
    fs::create_dir_all(&spotty_dir).expect("create spotty dir");
    let cache = Arc::new(DualCache::open(spotty_dir.join("cache.redb")).expect("open DualCache"));
    let accounts = Arc::new(AccountStore::new(temp.path()));
    let tokens = Arc::new(TokenManager::with_http(
        format!("{server_uri}/api/token"),
        Client::new(),
        Arc::clone(&cache),
        Arc::clone(&accounts),
        STARTUP_EPOCH,
    ));

    let client_id = IconCode::default();
    tokens
        .cache_tokens(
            &client_id,
            TEST_USER_ID,
            TEST_TOKEN,
            3600,
            Some("e2e-refresh-token"),
        )
        .expect("seed OAuth token in cache");

    let gate = Arc::new(RateLimitGate::new());
    let transport = Arc::new(
        SpotifyTransport::with_base_url(
            format!("{server_uri}/v1"),
            tokens,
            Arc::clone(&cache),
            gate,
            client_id.clone(),
            TEST_USER_ID,
        )
        .expect("build SpotifyTransport"),
    );
    let pipeline = Pipeline::new(Arc::clone(&transport));
    let client = SpotifyClient::new(
        transport,
        pipeline,
        Arc::clone(&cache),
        client_id,
        TEST_USER_ID,
        "US",
        RateLimitPolicy::FailFastServer,
    );

    (cache, accounts, client)
}

fn sample_artist(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "uri": format!("spotify:artist:{id}"),
        "name": name,
        "followers": { "total": 12_345 },
        "images": [{ "url": "https://img.example/artist.jpg" }]
    })
}

fn sample_track(id: &str, name: &str, artist_id: &str, artist_name: &str, album_id: &str) -> Value {
    json!({
        "id": id,
        "uri": format!("spotify:track:{id}"),
        "name": name,
        "duration_ms": 210_000,
        "disc_number": 1,
        "track_number": 1,
        "explicit": false,
        "is_playable": true,
        "artists": [{
            "id": artist_id,
            "uri": format!("spotify:artist:{artist_id}"),
            "name": artist_name
        }],
        "album": {
            "id": album_id,
            "uri": format!("spotify:album:{album_id}"),
            "name": "Sample Album",
            "album_type": "album",
            "release_date": "2024-05-01",
            "total_tracks": 1,
            "images": [{ "url": "https://img.example/album.jpg" }],
            "artists": [{
                "id": artist_id,
                "uri": format!("spotify:artist:{artist_id}"),
                "name": artist_name
            }]
        }
    })
}

fn sample_playlist(id: &str, name: &str, owner_id: &str, snapshot_id: &str) -> Value {
    json!({
        "id": id,
        "uri": format!("spotify:playlist:{id}"),
        "name": name,
        "description": format!("{name} description"),
        "snapshot_id": snapshot_id,
        "collaborative": false,
        "public": true,
        "owner": {
            "id": owner_id,
            "display_name": owner_id
        },
        "images": [{ "url": "https://img.example/pl.jpg" }],
        "tracks": { "total": 1 }
    })
}

#[tokio::test]
async fn journey_1_pkce_oauth_and_credential_promotion() {
    let server = MockServer::start().await;
    let temp = TempDir::new().expect("create tempdir");
    let spotty_dir = temp.path().join("spotty");
    fs::create_dir_all(&spotty_dir).expect("create spotty dir");

    let cache = Arc::new(DualCache::open(spotty_dir.join("cache.redb")).expect("open DualCache"));
    let accounts = Arc::new(AccountStore::new(temp.path()));
    let tokens = Arc::new(TokenManager::with_http(
        format!("{}/api/token", server.uri()),
        Client::new(),
        Arc::clone(&cache),
        Arc::clone(&accounts),
        STARTUP_EPOCH,
    ));

    Mock::given(method("POST"))
        .and(path("/auth/prepare"))
        .and(header(LMS_PLUGIN_ID_HEADER, DEFAULT_PLUGIN_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "nonce": "e2e-relay-nonce"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": TEST_TOKEN,
            "refresh_token": "e2e-refresh-token",
            "expires_in": 3600,
            "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": TEST_USER_ID,
            "display_name": "Alice Spotty",
            "product": "premium",
            "country": "US"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let controller = PkceCallbackController::with_endpoints(
        format!("{}/auth/prepare", server.uri()),
        format!("{}/auth/callback", server.uri()),
        format!("{}/authorize", server.uri()),
        format!("{}/v1", server.uri()),
        Client::new(),
        Arc::clone(&cache),
        Arc::clone(&accounts),
        Arc::clone(&tokens),
    );

    let client_id = IconCode::default();
    let redirect_url = controller
        .prepare_oauth_redirect(
            &client_id,
            "http://lms.local:9000/plugins/Spotty/settings/callback",
            "LMS/9.0",
        )
        .await
        .expect("prepare_oauth_redirect should succeed");

    let parsed = Url::parse(&redirect_url).expect("parse redirect URL");
    let query: HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256"),
        "PKCE challenge method must be S256: {query:?}"
    );
    assert!(
        query.get("code_challenge").is_some_and(|v| !v.is_empty()),
        "PKCE redirect must include code_challenge: {query:?}"
    );

    let before_cb = AuthLandingController::check_authenticate_status(&accounts);
    assert!(
        !before_cb.has_credentials,
        "authenticate status must be false before callback: {before_cb:?}"
    );

    let callback_vm = controller
        .handle_oauth_callback(&client_id, Some("oauth-code-123"))
        .await
        .expect("handle_oauth_callback should succeed");
    assert!(
        callback_vm.auth_error.is_none(),
        "callback should not report auth_error: {callback_vm:?}"
    );
    assert!(
        !callback_vm.free_product,
        "premium profile must set free_product = false: {callback_vm:?}"
    );

    let ready = AuthLandingController::check_authenticate_status(&accounts);
    assert!(
        ready.has_credentials,
        "authenticate status must be true after callback: {ready:?}"
    );

    let promoted = AuthLandingController::cleanup_and_promote(&accounts)
        .expect("cleanup_and_promote should succeed")
        .expect("promoted AccountId should be returned");
    let expected_id = AccountId::from_username(TEST_USER_ID);
    assert_eq!(
        promoted, expected_id,
        "promoted ID must match 8-hex username hash: {promoted:?}"
    );
    assert_eq!(
        promoted.as_str().len(),
        8,
        "promoted directory must be 8 hex chars: {promoted:?}"
    );

    let promoted_creds = accounts
        .credentials(&promoted)
        .expect("read promoted credentials")
        .expect("promoted credentials.json must exist");
    assert_eq!(
        promoted_creds.username, TEST_USER_ID,
        "promoted credentials must match authenticated user"
    );

    let auth_dir = accounts.spotty_dir().join(AUTHENTICATE_DIR);
    assert!(
        !auth_dir.exists(),
        "__AUTHENTICATE__ staging folder must be removed after promotion"
    );
}

#[tokio::test]
async fn journey_2_opml_browsing_artist_view_and_playlist_folders() {
    let server = MockServer::start().await;
    let temp = TempDir::new().expect("create tempdir");
    let (cache, _accounts, client) = build_mock_client(&server.uri(), &temp);

    let featured_pl = sample_playlist("feat111111111111111111", "Weekend Buzz", "spotify", "s0");
    Mock::given(method("GET"))
        .and(path("/v1/browse/featured-playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "Editor Picks",
            "playlists": {
                "items": [featured_pl],
                "total": 1,
                "next": null
            }
        })))
        .mount(&server)
        .await;

    let discover = sample_playlist("home111111111111111111", "Discover Weekly", "spotify", "h1");
    let daily = sample_playlist("home222222222222222222", "Daily Mix 1", "spotify", "h2");
    let radar = sample_playlist("home333333333333333333", "Release Radar", "spotify", "h3");
    Mock::given(method("GET"))
        .and(path(
            "/v1/browse/categories/0JQ5DAt0tbjZptfcdMSKl3/playlists",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "playlists": {
                "items": [discover, radar, daily],
                "total": 3,
                "next": null
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/artists/art123"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sample_artist("art123", "Test Artist")),
        )
        .mount(&server)
        .await;

    let top_track = sample_track("trk111", "Artist Hit", "art123", "Test Artist", "alb111");
    Mock::given(method("GET"))
        .and(path("/v1/artists/art123/top-tracks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tracks": [top_track]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/artists/art123/albums"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {
                    "id": "alb1",
                    "uri": "spotify:album:alb1",
                    "name": "Studio LP",
                    "album_type": "album",
                    "release_date": "2024-06-01",
                    "total_tracks": 10,
                    "images": [{ "url": "https://img.example/lp.jpg" }],
                    "artists": [{ "id": "art123", "name": "Test Artist" }]
                },
                {
                    "id": "sng1",
                    "uri": "spotify:album:sng1",
                    "name": "Lead Single",
                    "album_type": "single",
                    "release_date": "2024-05-01",
                    "total_tracks": 1,
                    "images": [{ "url": "https://img.example/sng.jpg" }],
                    "artists": [{ "id": "art123", "name": "Test Artist" }]
                },
                {
                    "id": "cmp1",
                    "uri": "spotify:album:cmp1",
                    "name": "Summer Hits",
                    "album_type": "compilation",
                    "release_date": "2023-07-01",
                    "total_tracks": 20,
                    "images": [{ "url": "https://img.example/cmp.jpg" }],
                    "artists": [{ "id": "va1", "name": "Various Artists" }]
                }
            ],
            "total": 3,
            "next": null
        })))
        .mount(&server)
        .await;

    let chart_id = top_chart_uri("US")
        .rsplit(':')
        .next()
        .expect("chart URI must have ID");
    let chart_track = sample_track("chart1", "Global #1", "art123", "Test Artist", "alb111");
    Mock::given(method("GET"))
        .and(path(format!("/v1/playlists/{chart_id}/tracks")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{ "added_at": "2026-01-01T00:00:00Z", "track": chart_track }],
            "total": 1,
            "next": null
        })))
        .mount(&server)
        .await;

    let pl_ids = [
        "pl11111111111111111111",
        "pl22222222222222222222",
        "pl33333333333333333333",
        "pl44444444444444444444",
        "pl55555555555555555555",
    ];
    let mut playlist_items = Vec::with_capacity(pl_ids.len());
    let mut playlist_uris = Vec::with_capacity(pl_ids.len());
    for (idx, id) in pl_ids.iter().enumerate() {
        playlist_items.push(sample_playlist(
            id,
            &format!("Playlist {}", idx + 1),
            TEST_USER_ID,
            &format!("snap{idx}"),
        ));
        playlist_uris.push(format!("spotify:playlist:{id}"));
    }
    Mock::given(method("GET"))
        .and(path("/v1/me/playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": playlist_items,
            "total": 5,
            "next": null
        })))
        .mount(&server)
        .await;

    let storage_dir = temp.path().join("desktop_storage");
    fs::create_dir_all(&storage_dir).expect("create desktop storage dir");
    let blob = format!(
        "header\rspotify:start-group:fld01:Favorites\r\
         spotify:playlist:{0}\rspotify:playlist:{1}\r\
         spotify:end-group:fld01\r\
         spotify:playlist:{2}\rspotify:playlist:{3}\rspotify:playlist:{4}\r",
        pl_ids[0], pl_ids[1], pl_ids[2], pl_ids[3], pl_ids[4]
    );
    fs::write(storage_dir.join("prefs.file"), blob.as_bytes()).expect("write folder blob");

    let candidates = FolderDiscovery::scan_candidate_files(&storage_dir, None, &cache)
        .expect("scan candidate folder files");
    assert_eq!(
        candidates.len(),
        1,
        "expected 1 candidate folder file: {candidates:?}"
    );
    let hierarchy = FolderDiscovery::find_best_hierarchy(&playlist_uris, &candidates, &cache)
        .expect("score folder hierarchy")
        .expect("5/5 playlists = 1.0 > 0.8 threshold must match");

    let router = OpmlRouter::new(client, GlobalPrefs::default(), PlayerPrefs::default())
        .with_folder_hierarchy(hierarchy);

    let root = router
        .handle_feed(OpmlRoute::Root)
        .await
        .expect("Root feed");
    assert!(
        node_names(&root.items).contains(&"Home") && node_names(&root.items).contains(&"Playlists"),
        "Root OPML must contain Home and Playlists: {:?}",
        node_names(&root.items)
    );

    let home = router
        .handle_feed(OpmlRoute::Home)
        .await
        .expect("Home feed");
    let home_names = node_names(&home.items);
    assert_eq!(
        home_names,
        vec!["Daily Mix 1", "Release Radar", "Discover Weekly"],
        "Home items must sort Daily Mix < Release Radar < Discover Weekly: {home_names:?}"
    );

    let artist_feed = router
        .handle_feed(OpmlRoute::Artist {
            uri_or_id: String::from("spotify:artist:art123"),
            album_scope: None,
        })
        .await
        .expect("Artist 3-way view feed");
    let artist_sections = node_names(&artist_feed.items);
    for required in [
        "Albums",
        "Singles & EPs",
        "Compilations",
        "Top Tracks",
        "Artist Radio",
        "Related Artists",
    ] {
        assert!(
            artist_sections.contains(&required),
            "Artist view missing '{required}': {artist_sections:?}"
        );
    }

    let top_tracks = router
        .handle_feed(OpmlRoute::TopTracks)
        .await
        .expect("TopTracks feed");
    assert_eq!(
        top_tracks.items.len(),
        1,
        "TopTracks must format 1 chart track: {:?}",
        top_tracks.items
    );
    assert_eq!(
        top_tracks.items[0].line1.as_deref(),
        Some("Global #1"),
        "TopTracks track line1 must match track title: {:?}",
        top_tracks.items[0]
    );

    let playlists_feed = router
        .handle_feed(OpmlRoute::MyPlaylists { user_id: None })
        .await
        .expect("MyPlaylists feed");
    let folder = playlists_feed
        .items
        .iter()
        .find(|node| node.name == "Favorites")
        .expect("Favorites folder must appear in MyPlaylists");
    assert_eq!(
        folder.kind,
        OpmlItemKind::Outline,
        "Folder node must be an outline: {folder:?}"
    );
    assert_eq!(
        folder.items.len(),
        2,
        "Favorites folder must contain 2 playlists: {:?}",
        folder.items
    );
    assert_eq!(
        playlists_feed.items.len(),
        4,
        "MyPlaylists root should have 1 folder + 3 root playlists: {:?}",
        node_names(&playlists_feed.items)
    );
}

fn node_names(nodes: &[OpmlNode]) -> Vec<&str> {
    let mut names = Vec::with_capacity(nodes.len());
    for node in nodes {
        names.push(node.name.as_str());
    }
    names
}

#[tokio::test]
async fn journey_3_importer_scan_sqlite_and_24h_editorial_sentinel() {
    let server = MockServer::start().await;
    let temp = TempDir::new().expect("create tempdir");
    let (cache, _accounts, client) = build_mock_client(&server.uri(), &temp);

    let alb_track = sample_track("trkalb111", "Album Song", "art111", "The Band", "alb111");
    let album_json = json!({
        "id": "alb111",
        "uri": "spotify:album:alb111",
        "name": "First Record",
        "album_type": "album",
        "release_date": "2024-03-10",
        "total_tracks": 1,
        "images": [{ "url": "https://img.example/alb1.jpg" }],
        "artists": [{
            "id": "art111",
            "uri": "spotify:artist:art111",
            "name": "The Band"
        }],
        "tracks": {
            "items": [alb_track],
            "total": 1,
            "next": null
        }
    });

    Mock::given(method("GET"))
        .and(path("/v1/me/albums"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "added_at": "2026-01-15T12:00:00Z",
                "album": album_json
            }],
            "total": 1,
            "next": null
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/me/following"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "artists": {
                "items": [sample_artist("art111", "The Band")],
                "cursors": { "after": null }
            }
        })))
        .mount(&server)
        .await;

    let editorial = sample_playlist(
        "pled111111111111111111",
        "Today Top Hits",
        "spotify",
        "snaped",
    );
    let user_pl = sample_playlist(
        "plusr11111111111111111",
        "Road Trip",
        TEST_USER_ID,
        "snapusr",
    );
    Mock::given(method("GET"))
        .and(path("/v1/me/playlists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [editorial, user_pl],
            "total": 2,
            "next": null
        })))
        .mount(&server)
        .await;

    let ed_track = sample_track("trked111", "Editorial Hit", "art111", "The Band", "alb111");
    Mock::given(method("GET"))
        .and(path("/v1/playlists/pled111111111111111111/tracks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{ "added_at": "2026-01-15T12:00:00Z", "track": ed_track }],
            "total": 1,
            "next": null
        })))
        .mount(&server)
        .await;

    let usr_track = sample_track(
        "trkusr111",
        "User Mix Track",
        "art111",
        "The Band",
        "alb111",
    );
    Mock::given(method("GET"))
        .and(path("/v1/playlists/plusr11111111111111111/tracks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{ "added_at": "2026-01-15T12:00:00Z", "track": usr_track }],
            "total": 1,
            "next": null
        })))
        .mount(&server)
        .await;

    let lms_db = temp.path().join("library.db");
    let accounts_arg = vec![(String::from(TEST_USER_ID), Arc::new(client))];
    let runner = ImporterRunner::new(accounts_arg.clone(), Arc::clone(&cache));
    let mut sink = SqliteLibrarySink::open(&lms_db).expect("open SQLite library.db");

    let t0 = 1_750_000_000_u64;
    let summary = runner
        .run_scan(ScanScope::FullLibrary, &mut sink, t0)
        .await
        .expect("ImporterRunner::run_scan should succeed");
    assert_eq!(summary.accounts_scanned, 1, "must scan 1 account");
    assert_eq!(summary.tracks_stored, 1, "must store 1 album track");
    assert_eq!(summary.artists_upserted, 1, "must upsert 1 contributor");
    assert_eq!(summary.playlists_upserted, 2, "must upsert 2 playlists");

    drop(sink);
    let conn = Connection::open(&lms_db).expect("reopen library.db for read-back");
    let track_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tracks WHERE url LIKE 'spotify:track:%'",
            [],
            |row| row.get(0),
        )
        .expect("query tracks table");
    assert!(
        track_count >= 1,
        "library.db must contain scanned spotify:track rows: {track_count}"
    );

    let contributor_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contributors WHERE extid = 'spotify:artist:art111'",
            [],
            |row| row.get(0),
        )
        .expect("query contributors table");
    assert_eq!(
        contributor_count, 1,
        "library.db must contain contributor row for The Band"
    );

    let pl_track_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM playlist_track", [], |row| row.get(0))
        .expect("query playlist_track table");
    assert_eq!(
        pl_track_count, 2,
        "playlist_track must link 2 playlist track entries"
    );

    let before_expiry = online_library_needs_update(
        &accounts_arg,
        &cache,
        PlaylistSyncPolicy::IncludePlaylists,
        t0 + 3600,
    )
    .await
    .expect("needs_update before 24h expiry");
    assert!(
        !before_expiry,
        "online_library_needs_update must be false before 24h sentinel expiry"
    );

    let after_expiry = online_library_needs_update(
        &accounts_arg,
        &cache,
        PlaylistSyncPolicy::IncludePlaylists,
        t0 + 86_401,
    )
    .await
    .expect("needs_update after 24h expiry");
    assert!(
        after_expiry,
        "online_library_needs_update must be true after 24h sentinel expiry"
    );

    // Also verify `lms-spotty scan` CLI subcommand runs cleanly on an empty cache.
    let cli_temp = TempDir::new().expect("create CLI tempdir");
    let cli_db = cli_temp.path().join("cli_library.db");
    let out = Command::new(env!("CARGO_BIN_EXE_lms-spotty"))
        .args([
            "scan",
            "--cache-dir",
            &cli_temp.path().to_string_lossy(),
            "--lms-db",
            &cli_db.to_string_lossy(),
        ])
        .output()
        .expect("run lms-spotty scan CLI");
    assert!(
        out.status.success(),
        "lms-spotty scan CLI failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let summary_json: Value =
        serde_json::from_slice(&out.stdout).expect("parse lms-spotty scan JSON stdout");
    assert_eq!(
        summary_json["accounts_scanned"],
        json!(0),
        "empty cache CLI scan should report 0 accounts: {summary_json}"
    );
}

#[tokio::test]
async fn journey_4_lms_integrations_dstm_home_extras_and_last_mix() {
    let server = MockServer::start().await;
    let temp = TempDir::new().expect("create tempdir");
    let (_cache, _accounts, client) = build_mock_client(&server.uri(), &temp);

    let rec_track = sample_track("rec999", "Recommended Jam", "art9", "Rec Artist", "alb9");
    Mock::given(method("GET"))
        .and(path("/v1/recommendations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tracks": [rec_track]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let seeds = vec![
        SeedTrackInput::from_url("spotify:track:seed1"),
        SeedTrackInput::from_url("spotify:track:seed2"),
        SeedTrackInput::from_url("spotify:track:seed3"),
        SeedTrackInput::from_url("spotify:track:seed4"),
        SeedTrackInput::from_url("spotify:track:seed5"),
        SeedTrackInput::from_url("spotify:track:seed6"),
        SeedTrackInput::from_artist_title("Fallback Artist", "Fallback Song"),
    ];
    let mix = DstmRecommender::generate_mix(&client, &seeds)
        .await
        .expect("DstmRecommender::generate_mix should succeed");
    assert_eq!(
        mix,
        vec![String::from("spotify://track:rec999")],
        "DSTM must return spotify://track:<id> URIs: {mix:?}"
    );

    let requests = server
        .received_requests()
        .await
        .expect("inspect wiremock received requests");
    let rec_req = requests
        .iter()
        .find(|req| req.url.path() == "/v1/recommendations")
        .expect("recommendations request must be recorded");
    let seed_tracks_param = rec_req
        .url
        .query_pairs()
        .find(|(k, _)| k == "seed_tracks")
        .map(|(_, v)| v.into_owned())
        .expect("seed_tracks query parameter must be sent");
    let seed_count = seed_tracks_param.split(',').count();
    assert!(
        seed_count <= 5,
        "DSTM must cap combined seeds to <= 5, got {seed_count}: {seed_tracks_param}"
    );

    assert_eq!(
        HOME_EXTRAS.len(),
        5,
        "HOME_EXTRAS must expose all 5 MaterialSkin descriptors"
    );
    for extra in HOME_EXTRAS {
        assert_eq!(
            resolve_home_extra_menu(extra.tag),
            Some(extra.menu_param),
            "resolve_home_extra_menu must map tag {}",
            extra.tag
        );
    }

    let lm_track = sample_track("lm001", "Digital Love", "dp1", "Daft Punk", "disc1");
    Mock::given(method("GET"))
        .and(path("/v1/search"))
        .and(query_param("q", "artist:Daft Punk track:Digital Love"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tracks": {
                "items": [lm_track],
                "total": 1,
                "next": null
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/search"))
        .and(query_param("q", "artist:Nobody track:Nothing"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tracks": {
                "items": [],
                "total": 0,
                "next": null
            }
        })))
        .mount(&server)
        .await;

    let best = LastMixService::best_match(&client, "Daft Punk", "Digital Love")
        .await
        .expect("LastMixService::best_match hit")
        .expect("expected Some(LastMixCandidate)");
    assert_eq!(best.url, "spotify:track:lm001");
    assert_eq!(best.artist, "Daft Punk");
    assert_eq!(best.title, "Digital Love");

    let empty = LastMixService::best_match(&client, "Nobody", "Nothing")
        .await
        .expect("LastMixService::best_match empty search must not error");
    assert!(
        empty.is_none(),
        "empty search must return Ok(None), got {empty:?}"
    );
}

#[test]
fn journey_5_drop_in_helper_check_handshake() {
    let bin = env!("CARGO_BIN_EXE_lms-spotty");
    let output = Command::new(bin)
        .args(["-n", "Spotty", "--check"])
        .output()
        .expect("execute lms-spotty -n Spotty --check");
    assert!(
        output.status.success(),
        "--check exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout must be valid UTF-8");
    let mut lines = stdout.lines();
    let first_line = lines.next().expect("stdout must have version line 1");
    let expected_prefix = format!("ok spotty v{SPOTTY_VERSION}");
    assert!(
        first_line.starts_with(&expected_prefix),
        "line 1 must start with '{expected_prefix}', got '{first_line}'"
    );

    let rest: Vec<&str> = lines.collect();
    let json_body = rest.join("\n");
    let caps: Value =
        serde_json::from_str(&json_body).expect("lines 2+ must decode as capability JSON");
    assert_eq!(
        caps["autoplay"],
        json!(true),
        "capability JSON must advertise autoplay=true: {caps}"
    );
    assert_eq!(
        caps["oauth"],
        json!(true),
        "capability JSON must advertise oauth=true: {caps}"
    );
    assert_eq!(
        caps["podcasts"],
        json!(true),
        "capability JSON must advertise podcasts=true: {caps}"
    );
    assert_eq!(
        caps["volume-normalisation"],
        json!(true),
        "capability JSON must advertise volume-normalisation=true: {caps}"
    );
    assert_eq!(
        caps["zeroconf-port"],
        json!(true),
        "capability JSON must advertise zeroconf-port=true: {caps}"
    );
}

#[tokio::test]
async fn journey_6_live_perl_to_rust_daemon_bridge() {
    let perl_exe = Path::new(r"C:\Strawberry\perl\bin\perl.exe");
    if !perl_exe.is_file() {
        return;
    }

    let temp = TempDir::new().expect("create tempdir");
    let config = DaemonConfig::new(temp.path()).with_sweep_interval(Duration::from_secs(60));
    let endpoint_path = config.endpoint_file.clone();

    let daemon = SpottyDaemon::bind(config)
        .await
        .expect("bind SpottyDaemon for Perl bridge test");
    let endpoint = read_endpoint_file(&endpoint_path).expect("read daemon.endpoint");
    assert_eq!(endpoint.version, SPOTTY_VERSION);

    let server_handle = tokio::spawn(async move { daemon.serve().await });

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("resolve workspace root");
    let bridge_pm = repo_root.join("SpottyBridge.pm");
    assert!(
        bridge_pm.is_file(),
        "SpottyBridge.pm must exist at {}",
        bridge_pm.display()
    );

    let perl_script = r#"
use strict;
use warnings;
use JSON::PP qw(encode_json);
my ($bridge_pm, $cache_dir) = @ARGV;
do $bridge_pm or die "Failed loading $bridge_pm: " . ($@ || $!);
my $ping = Plugins::Spotty::SpottyBridge->call('system.ping', {}, cache_dir => $cache_dir);
my $init = Plugins::Spotty::SpottyBridge->call('plugin.init', {
    disabled_formats => ['aac'],
    arch => 'MSWin32-x64-multi-thread',
    os => 'MSWin32',
    helper_bin => 'lms-spotty',
}, cache_dir => $cache_dir);
my $extras = Plugins::Spotty::SpottyBridge->call(
    'integrations.home_extras',
    { tag => 'home' },
    cache_dir => $cache_dir,
);
my $shut = Plugins::Spotty::SpottyBridge->call('system.shutdown', {}, cache_dir => $cache_dir);
print encode_json({
    ping => $ping,
    init => $init,
    extras => $extras,
    shutdown => $shut,
}) . "\n";
"#;

    let bridge_arg = bridge_pm.to_string_lossy().into_owned();
    let cache_arg = temp.path().to_string_lossy().into_owned();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(perl_exe)
            .args(["-e", perl_script, &bridge_arg, &cache_arg])
            .output()
            .expect("spawn Strawberry Perl")
    })
    .await
    .expect("join blocking Perl invocation");

    assert!(
        output.status.success(),
        "Perl SpottyBridge round-trip failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let decoded: Value =
        serde_json::from_slice(&output.stdout).expect("decode Perl JSON stdout output");
    assert_eq!(decoded["ping"]["status"], json!("ok"));
    assert_eq!(decoded["ping"]["version"], json!(SPOTTY_VERSION));
    assert_eq!(decoded["init"]["version"], json!(SPOTTY_VERSION));
    assert!(
        decoded["init"]["custom_convert_conf"]
            .as_str()
            .is_some_and(|conf| conf.contains("[lms-spotty]")),
        "plugin.init must return custom_convert_conf: {decoded}"
    );
    assert_eq!(
        decoded["extras"]["resolved_menu"],
        json!("home_heroes_home")
    );
    assert_eq!(
        decoded["extras"]["extras"]
            .as_array()
            .expect("extras array")
            .len(),
        5
    );
    assert_eq!(decoded["shutdown"]["status"], json!("shutting_down"));

    server_handle
        .await
        .expect("join SpottyDaemon serve task")
        .expect("SpottyDaemon should exit cleanly after system.shutdown");
    assert!(
        !endpoint_path.exists(),
        "daemon.endpoint must be removed on shutdown"
    );
}
