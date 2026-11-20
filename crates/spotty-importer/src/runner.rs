//! Full online-library scan orchestrator and artist picture lookup.
//!
//! Mirrors `Plugins::Spotty::Importer::{startScan, getArtistPicture}`
//! (`Importer.pm:49-72, 307-319`).

use std::collections::HashSet;
use std::sync::Arc;

use spotty_api::SpotifyClient;
use spotty_cache::DualCache;
use spotty_types::{Result, TagCleanupPolicy};

use crate::needs_update::online_library_needs_update;
use crate::scanners::{scan_enabled_albums, scan_enabled_artists, scan_enabled_playlists};
use crate::schema_sink::LibrarySink;

const DEFAULT_ARTICLES: [&str; 3] = ["The", "A", "An"];

/// Policy controlling whether Spotify playlists are included in sync and
/// change-detection operations.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaylistSyncPolicy {
    /// Sync and check playlists alongside saved albums and followed artists.
    #[default]
    IncludePlaylists,
    /// Skip playlists during import and change detection.
    IgnorePlaylists,
}

/// Scope controlling which library sections [`ImporterRunner::run_scan`] runs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanScope {
    /// Scan saved albums, followed artists, and playlists.
    #[default]
    FullLibrary,
    /// Scan only Spotify playlists.
    PlaylistsOnly,
    /// Scan saved albums and followed artists while skipping playlists.
    LibraryWithoutPlaylists,
}

/// Summary counts produced by [`ImporterRunner::run_scan`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanSummary {
    /// Number of enabled accounts processed.
    pub accounts_scanned: usize,
    /// Number of album track rows stored into the sink.
    pub tracks_stored: usize,
    /// Number of artist contributor records upserted.
    pub artists_upserted: usize,
    /// Number of stale Spotify playlist rows purged before playlist import.
    pub playlists_purged: usize,
    /// Number of Spotify playlists upserted.
    pub playlists_upserted: usize,
    /// Number of stale Spotify track rows pruned after scan completion.
    pub tracks_pruned: usize,
}

/// Orchestrates multi-account Spotify online-library imports into an LMS
/// [`LibrarySink`].
#[derive(Debug, Clone)]
pub struct ImporterRunner {
    accounts: Vec<(String, Arc<SpotifyClient>)>,
    cache: Arc<DualCache>,
    dont_import_accounts: HashSet<String>,
    split_sep: char,
    tag_cleanup: TagCleanupPolicy,
    leading_articles: Vec<String>,
}

impl ImporterRunner {
    /// Creates an [`ImporterRunner`] with default `';'` artist separator,
    /// [`TagCleanupPolicy::StripSuffixes`], and `"The"/"A"/"An"` sort articles.
    #[must_use]
    pub fn new(accounts: Vec<(String, Arc<SpotifyClient>)>, cache: Arc<DualCache>) -> Self {
        let mut leading_articles = Vec::with_capacity(DEFAULT_ARTICLES.len());
        for article in DEFAULT_ARTICLES {
            leading_articles.push(String::from(article));
        }
        Self {
            accounts,
            cache,
            dont_import_accounts: HashSet::new(),
            split_sep: ';',
            tag_cleanup: TagCleanupPolicy::StripSuffixes,
            leading_articles,
        }
    }

    /// Excludes account IDs present in `dont_import_accounts` from scanning.
    #[must_use]
    pub fn with_dont_import_accounts(mut self, dont_import_accounts: HashSet<String>) -> Self {
        self.dont_import_accounts = dont_import_accounts;
        self
    }

    /// Sets the LMS `splitList` separator character used for joined artists.
    #[must_use]
    pub fn with_split_sep(mut self, split_sep: char) -> Self {
        self.split_sep = split_sep;
        self
    }

    /// Sets the [`TagCleanupPolicy`] applied to prepared track and album names.
    #[must_use]
    pub fn with_tag_cleanup(mut self, tag_cleanup: TagCleanupPolicy) -> Self {
        self.tag_cleanup = tag_cleanup;
        self
    }

    /// Overrides the leading articles stripped from artist sort names.
    #[must_use]
    pub fn with_leading_articles(mut self, articles: Vec<String>) -> Self {
        self.leading_articles = articles;
        self
    }

    /// Returns the enabled `(user_id, client)` pairs after applying the
    /// `dont_import_accounts` exclusion filter.
    #[must_use]
    pub fn enabled_accounts(&self) -> Vec<(String, Arc<SpotifyClient>)> {
        let mut enabled = Vec::with_capacity(self.accounts.len());
        for (user_id, client) in &self.accounts {
            if !self.dont_import_accounts.contains(user_id) {
                enabled.push((user_id.clone(), Arc::clone(client)));
            }
        }
        enabled
    }

    /// Checks whether any enabled account requires a library rescan.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if cache reads or Spotify API
    /// requests fail.
    pub async fn needs_update(
        &self,
        playlist_policy: PlaylistSyncPolicy,
        now_epoch_secs: u64,
    ) -> Result<bool> {
        let enabled = self.enabled_accounts();
        if enabled.is_empty() {
            return Ok(false);
        }
        online_library_needs_update(&enabled, &self.cache, playlist_policy, now_epoch_secs).await
    }

    /// Executes the full online-library import pipeline across enabled accounts.
    ///
    /// Pipeline order matches `Importer.pm:startScan`:
    /// `init_online_tracks_table` -> albums & artists (unless
    /// [`ScanScope::PlaylistsOnly`]) -> `purge_spotify_playlists` + playlists
    /// (unless [`ScanScope::LibraryWithoutPlaylists`]) ->
    /// `delete_removed_tracks`.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if any Spotify API call, cache
    /// write, or `SQLite` sink operation fails.
    pub async fn run_scan(
        &self,
        scope: ScanScope,
        sink: &mut dyn LibrarySink,
        now_epoch_secs: u64,
    ) -> Result<ScanSummary> {
        let enabled = self.enabled_accounts();
        if enabled.is_empty() {
            return Ok(ScanSummary::default());
        }

        sink.init_online_tracks_table()?;

        let mut summary = ScanSummary {
            accounts_scanned: enabled.len(),
            ..ScanSummary::default()
        };
        let multi_account = enabled.len() > 1;

        if scope != ScanScope::PlaylistsOnly {
            for (user_id, api) in &enabled {
                let account_tag = if multi_account {
                    Some(format!("spotify:{user_id}"))
                } else {
                    None
                };
                let stored = scan_enabled_albums(
                    api,
                    user_id,
                    account_tag.as_deref(),
                    &self.cache,
                    sink,
                    self.split_sep,
                    self.tag_cleanup,
                )
                .await?;
                summary.tracks_stored = summary.tracks_stored.saturating_add(stored);
            }

            let mut article_refs = Vec::with_capacity(self.leading_articles.len());
            for article in &self.leading_articles {
                article_refs.push(article.as_str());
            }
            for (user_id, api) in &enabled {
                let upserted =
                    scan_enabled_artists(api, user_id, &self.cache, sink, &article_refs).await?;
                summary.artists_upserted = summary.artists_upserted.saturating_add(upserted);
            }
        }

        if scope != ScanScope::LibraryWithoutPlaylists {
            summary.playlists_purged = sink.purge_spotify_playlists()?;
            for (user_id, api) in &enabled {
                let playlists =
                    scan_enabled_playlists(api, user_id, &self.cache, sink, now_epoch_secs).await?;
                summary.playlists_upserted = summary.playlists_upserted.saturating_add(playlists);
            }
        }

        summary.tracks_pruned = sink.delete_removed_tracks()?;
        Ok(summary)
    }
}

/// Resolves the first non-empty artist picture URL from a comma-separated list
/// of Spotify artist IDs or URIs (`Importer.pm:getArtistPicture`).
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if a non-empty artist ID is invalid or
/// the Spotify API request fails.
pub async fn artist_picture_url(
    api: &SpotifyClient,
    comma_separated_ids: &str,
) -> Result<Option<String>> {
    for raw_id in comma_separated_ids.split(',') {
        let trimmed = raw_id.trim();
        if trimmed.is_empty() {
            continue;
        }
        let artist = api.artist(trimmed).await?;
        if !artist.image.is_empty() {
            return Ok(Some(artist.image));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use serde_json::json;
    use spotty_api::{
        Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
    };
    use spotty_cache::{AccountStore, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{artist_picture_url, ImporterRunner, ScanScope};
    use crate::schema_sink::SqliteLibrarySink;

    fn test_client(mock_uri: &str, tmp: &TempDir, user_id: &str) -> Arc<SpotifyClient> {
        let user_dir = tmp.path().join(user_id);
        std::fs::create_dir_all(&user_dir).expect("create user cache dir");
        let cache =
            Arc::new(DualCache::open(user_dir.join("cache.redb")).expect("open user DualCache"));
        let accounts = Arc::new(AccountStore::new(&user_dir));
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

    async fn mount_account_library(
        server: &MockServer,
        track_id: &str,
        artist_id: &str,
        pl_id: &str,
    ) {
        let album_id = format!("alb{track_id}");
        let album_json = json!({
            "id": album_id,
            "uri": format!("spotify:album:{album_id}"),
            "name": format!("Album {track_id}"),
            "album_type": "album",
            "release_date": "2025-01-01",
            "total_tracks": 1,
            "artists": [{
                "id": artist_id,
                "uri": format!("spotify:artist:{artist_id}"),
                "name": format!("Artist {artist_id}")
            }],
            "images": [{
                "url": "https://img.example/alb.jpg",
                "width": 640,
                "height": 640
            }],
            "tracks": {
                "items": [{
                    "id": track_id,
                    "uri": format!("spotify:track:{track_id}"),
                    "name": format!("Song {track_id}"),
                    "duration_ms": 180000,
                    "track_number": 1,
                    "disc_number": 1,
                    "is_playable": true,
                    "artists": [{
                        "id": artist_id,
                        "uri": format!("spotify:artist:{artist_id}"),
                        "name": format!("Artist {artist_id}")
                    }]
                }]
            }
        });

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "added_at": "2026-11-18T12:00:00Z",
                    "album": album_json.clone()
                }]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/albums"))
            .and(query_param("ids", album_id.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "albums": [album_json]
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
                        "name": format!("Artist {artist_id}")
                    }],
                    "cursors": {"after": null},
                    "next": null
                }
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "id": pl_id,
                    "uri": format!("spotify:playlist:{pl_id}"),
                    "name": format!("List {pl_id}"),
                    "owner": {"id": "user"},
                    "snapshot_id": "snap-1",
                    "images": [{"url": "https://img.example/pl.jpg", "width": 640, "height": 640}],
                    "tracks": {"total": 1}
                }]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/playlists/{pl_id}/tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "track": {
                        "id": track_id,
                        "uri": format!("spotify:track:{track_id}"),
                        "name": format!("Song {track_id}"),
                        "is_playable": true,
                        "artists": [{
                            "id": artist_id,
                            "uri": format!("spotify:artist:{artist_id}"),
                            "name": format!("Artist {artist_id}")
                        }],
                        "album": {
                            "id": album_id,
                            "uri": format!("spotify:album:{album_id}"),
                            "name": format!("Album {track_id}"),
                            "images": [{
                                "url": "https://img.example/alb.jpg",
                                "width": 640,
                                "height": 640
                            }]
                        }
                    }
                }]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/tracks"))
            .and(query_param("ids", track_id))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [{
                    "id": track_id,
                    "uri": format!("spotify:track:{track_id}"),
                    "name": format!("Song {track_id}"),
                    "duration_ms": 180000,
                    "is_playable": true,
                    "artists": [{
                        "id": artist_id,
                        "uri": format!("spotify:artist:{artist_id}"),
                        "name": format!("Artist {artist_id}")
                    }],
                    "album": {
                        "id": album_id,
                        "uri": format!("spotify:album:{album_id}"),
                        "name": format!("Album {track_id}"),
                        "images": [{
                            "url": "https://img.example/alb.jpg",
                            "width": 640,
                            "height": 640
                        }]
                    }
                }]
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_scan_multi_account_tags_filters_excluded_and_prunes_stale_tracks() {
        let server_alice = MockServer::start().await;
        let server_bob = MockServer::start().await;
        let server_charlie = MockServer::start().await;

        mount_account_library(&server_alice, "trkA", "artA", "plA0000000000000000001").await;
        mount_account_library(&server_bob, "trkB", "artB", "plB0000000000000000001").await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server_charlie)
            .await;

        let tmp = TempDir::new().expect("create tempdir");
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let alice = test_client(&server_alice.uri(), &tmp, "alice");
        let bob = test_client(&server_bob.uri(), &tmp, "bob");
        let charlie = test_client(&server_charlie.uri(), &tmp, "charlie");

        let mut sink = SqliteLibrarySink::open_in_memory().expect("open sqlite sink");
        sink.with_connection(|conn| {
            conn.execute(
                "INSERT INTO tracks (url, title, content_type, extid)
                 VALUES ('spotify:track:stale1', 'Removed Track', 'spt', 'spotify:track:stale1')",
                [],
            )
            .expect("seed stale spotify track");
            conn.execute(
                "INSERT INTO tracks (url, title, content_type, extid)
                 VALUES ('file:///music/local.mp3', 'Local Track', 'mp3', 'local-1')",
                [],
            )
            .expect("seed local track");
            conn.execute(
                "INSERT INTO tracks (url, title, content_type, extid)
                 VALUES ('spotify:playlist:old', 'Spotify: Old', 'ssp', 'spotify:playlist:old')",
                [],
            )
            .expect("seed old spotify playlist");
        });

        let mut excluded = HashSet::new();
        excluded.insert(String::from("charlie"));
        let runner = ImporterRunner::new(
            vec![
                (String::from("alice"), alice),
                (String::from("bob"), bob),
                (String::from("charlie"), charlie),
            ],
            cache,
        )
        .with_dont_import_accounts(excluded);

        let summary = runner
            .run_scan(ScanScope::FullLibrary, &mut sink, 1_700_000_000)
            .await
            .expect("run_scan FullLibrary succeeds");

        assert_eq!(summary.accounts_scanned, 2);
        assert_eq!(summary.tracks_stored, 2);
        assert_eq!(summary.artists_upserted, 2);
        assert_eq!(summary.playlists_purged, 1);
        assert_eq!(summary.playlists_upserted, 2);
        assert_eq!(summary.tracks_pruned, 1);

        let tags: Vec<(String, Option<String>)> = sink.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT url, account_tag FROM tracks ORDER BY url")
                .expect("prepare track query");
            let mut rows = stmt.query([]).expect("query tracks");
            let mut out = Vec::new();
            while let Some(r) = rows.next().expect("read row") {
                out.push((r.get(0).expect("url"), r.get(1).expect("account_tag")));
            }
            out
        });

        assert_eq!(
            tags,
            vec![
                (String::from("file:///music/local.mp3"), None),
                (
                    String::from("spotify:playlist:plA0000000000000000001"),
                    None
                ),
                (
                    String::from("spotify:playlist:plB0000000000000000001"),
                    None
                ),
                (
                    String::from("spotify:track:trkA"),
                    Some(String::from("spotify:alice"))
                ),
                (
                    String::from("spotify:track:trkB"),
                    Some(String::from("spotify:bob"))
                ),
            ]
        );
    }

    #[tokio::test]
    async fn run_scan_respects_library_without_playlists_and_playlists_only_scopes() {
        let server = MockServer::start().await;
        mount_account_library(&server, "trk1", "art1", "pl10000000000000000001").await;

        let tmp = TempDir::new().expect("create tempdir");
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let alice = test_client(&server.uri(), &tmp, "alice");
        let runner = ImporterRunner::new(vec![(String::from("alice"), alice)], cache);

        let mut sink_no_pl = SqliteLibrarySink::open_in_memory().expect("open sink 1");
        let no_pl = runner
            .run_scan(
                ScanScope::LibraryWithoutPlaylists,
                &mut sink_no_pl,
                1_700_000_000,
            )
            .await
            .expect("run LibraryWithoutPlaylists");
        assert_eq!(no_pl.tracks_stored, 1);
        assert_eq!(no_pl.artists_upserted, 1);
        assert_eq!(no_pl.playlists_purged, 0);
        assert_eq!(no_pl.playlists_upserted, 0);

        let mut sink_pl_only = SqliteLibrarySink::open_in_memory().expect("open sink 2");
        let pl_only = runner
            .run_scan(ScanScope::PlaylistsOnly, &mut sink_pl_only, 1_700_000_000)
            .await
            .expect("run PlaylistsOnly");
        assert_eq!(pl_only.tracks_stored, 0);
        assert_eq!(pl_only.artists_upserted, 0);
        assert_eq!(pl_only.playlists_upserted, 1);
    }

    #[tokio::test]
    async fn artist_picture_url_resolves_first_non_empty_image_from_csv() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("create tempdir");
        let client = test_client(&server.uri(), &tmp, "alice");

        Mock::given(method("GET"))
            .and(path("/v1/artists/art1000000000000000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "art1000000000000000001",
                "uri": "spotify:artist:art1000000000000000001",
                "name": "No Image",
                "images": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/artists/art2000000000000000002"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "art2000000000000000002",
                "uri": "spotify:artist:art2000000000000000002",
                "name": "Has Image",
                "images": [{"url": "https://img.example/art2.jpg", "width": 640, "height": 640}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/artists/art3000000000000000003"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(0)
            .mount(&server)
            .await;

        let resolved = artist_picture_url(
            &client,
            "art1000000000000000001, art2000000000000000002, art3000000000000000003",
        )
        .await
        .expect("artist_picture_url succeeds");
        assert_eq!(resolved.as_deref(), Some("https://img.example/art2.jpg"));

        let empty = artist_picture_url(&client, " , ")
            .await
            .expect("empty csv succeeds");
        assert_eq!(empty, None);
    }
}
