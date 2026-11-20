//! Concurrent 3-way artist overview builder (`OPML.pm:artist`).

use std::collections::BTreeMap;

use spotty_api::NormalizedAlbum;
use spotty_api::NormalizedArtist;
use spotty_api::SpotifyClient;
use spotty_api::SPOTIFY_LIMIT;
use spotty_types::AlbumScope;
use spotty_types::ExplicitFilterMode;
use spotty_types::Result;
use spotty_types::SortOrder;

use crate::formatters::format_album_list;
use crate::formatters::format_track_list;
use crate::formatters::OpmlItemKind;
use crate::formatters::OpmlNode;
use crate::formatters::TrackListOptions;
use crate::formatters::DEFAULT_SORT_ARTICLES;
use crate::home::IMG_ALBUM;
use crate::home::IMG_ARTIST;
use crate::home::IMG_COMPILATION;
use crate::home::IMG_FOLLOW_ARTIST;
use crate::home::IMG_RADIO;
use crate::home::IMG_SONG;
use crate::home::IMG_TOP_TRACKS;

const MAX_ARTIST_ALBUMS: usize = 500;

/// Fetches an artist profile, top tracks, and discography concurrently via
/// `tokio::try_join!` and partitions albums into `Albums`, `Singles & EPs`,
/// and `Compilations` alongside `Top Tracks`, `Artist Radio`,
/// `Related Artists`, and `Follow artist` action nodes.
///
/// Mirrors `OPML.pm:artist` and `_gotArtistData` without mutable
/// `$client->pluginData` barriers.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError::InvalidUri`] when `artist_id` is not a
/// valid Spotify artist ID or URI, or [`spotty_types::SpottyError`] if any of
/// the concurrent Spotify Web API calls fails.
pub async fn fetch_artist_overview(
    client: &SpotifyClient,
    artist_id: &str,
    album_scope: AlbumScope,
    explicit_filter: ExplicitFilterMode,
    sort_order: SortOrder,
) -> Result<OpmlNode> {
    let (artist, top_tracks, mut albums) = tokio::try_join!(
        client.artist(artist_id),
        client.artist_tracks(artist_id),
        client.artist_albums(artist_id, None, SPOTIFY_LIMIT, MAX_ARTIST_ALBUMS),
    )?;

    // Match OPML.pm:888 release-date descending default ordering before
    // optional alphabetical sorting inside format_album_list.
    albums.sort_by(|lhs, rhs| rhs.release_date.cmp(&lhs.release_date));

    if album_scope == AlbumScope::MyAlbumsOnly && !albums.is_empty() {
        let mut album_ids = Vec::with_capacity(albums.len());
        for album in &albums {
            album_ids.push(album.id.as_str());
        }
        let saved = client.is_in_my_albums(&album_ids).await?;
        albums.retain(|album| saved.get(&album.id).copied().unwrap_or(false));
    }

    let (regular_albums, singles, compilations) = partition_albums(&artist, albums);

    let mut items = Vec::new();
    push_album_section(&mut items, "Albums", IMG_ALBUM, &regular_albums, sort_order);
    push_album_section(&mut items, "Singles & EPs", IMG_SONG, &singles, sort_order);
    push_album_section(
        &mut items,
        "Compilations",
        IMG_COMPILATION,
        &compilations,
        sort_order,
    );

    let formatted_tracks =
        format_track_list(&top_tracks, explicit_filter, &TrackListOptions::default());
    if !formatted_tracks.is_empty() {
        let mut top_node = OpmlNode::new("Top Tracks", OpmlItemKind::Outline);
        top_node.image = Some(String::from(IMG_TOP_TRACKS));
        top_node.items = formatted_tracks;
        items.push(top_node);
    }

    items.push(build_artist_action_node(
        "Artist Radio",
        OpmlItemKind::Playlist,
        IMG_RADIO,
        "artist_radio",
        &artist,
    ));
    items.push(build_artist_action_node(
        "Related Artists",
        OpmlItemKind::Link,
        IMG_ARTIST,
        "related_artists",
        &artist,
    ));
    items.push(build_follow_artist_node(&artist));

    let mut root = OpmlNode::new(artist.name.clone(), OpmlItemKind::Outline);
    root.url = Some(artist.uri.clone());
    root.image = if artist.image.is_empty() {
        Some(String::from(IMG_ARTIST))
    } else {
        Some(artist.image.clone())
    };
    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("uri"), artist.uri);
    passthrough.insert(String::from("id"), artist.id);
    root.passthrough = passthrough;
    root.items = items;
    Ok(root)
}

fn primary_artist_name(album: &NormalizedAlbum) -> &str {
    if !album.artist.is_empty() {
        album.artist.as_str()
    } else if let Some(first) = album.artists.first() {
        first.name.as_str()
    } else {
        ""
    }
}

fn partition_albums(
    artist: &NormalizedArtist,
    albums: Vec<NormalizedAlbum>,
) -> (
    Vec<NormalizedAlbum>,
    Vec<NormalizedAlbum>,
    Vec<NormalizedAlbum>,
) {
    let mut regular = Vec::new();
    let mut singles = Vec::new();
    let mut compilations = Vec::new();

    for album in albums {
        if album.album_type.eq_ignore_ascii_case("single") {
            singles.push(album);
        } else if album.album_type.eq_ignore_ascii_case("compilation")
            || !primary_artist_name(&album).eq_ignore_ascii_case(&artist.name)
        {
            compilations.push(album);
        } else {
            regular.push(album);
        }
    }

    (regular, singles, compilations)
}

fn push_album_section(
    out: &mut Vec<OpmlNode>,
    title: &str,
    icon: &str,
    albums: &[NormalizedAlbum],
    sort_order: SortOrder,
) {
    if albums.is_empty() {
        return;
    }
    let formatted = format_album_list(albums, sort_order, DEFAULT_SORT_ARTICLES, "by");
    let mut section = OpmlNode::new(title, OpmlItemKind::Outline);
    section.image = Some(String::from(icon));
    section.items = formatted.items;
    section.index_list = formatted.index_list;
    out.push(section);
}

fn build_artist_action_node(
    name: &str,
    kind: OpmlItemKind,
    icon: &str,
    route: &str,
    artist: &NormalizedArtist,
) -> OpmlNode {
    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("route"), String::from(route));
    passthrough.insert(String::from("uri"), artist.uri.clone());

    let mut node = OpmlNode::new(name, kind);
    node.url = Some(artist.uri.clone());
    node.image = Some(String::from(icon));
    node.passthrough = passthrough;
    node
}

fn build_follow_artist_node(artist: &NormalizedArtist) -> OpmlNode {
    let mut node = build_artist_action_node(
        "Follow artist",
        OpmlItemKind::Link,
        IMG_FOLLOW_ARTIST,
        "follow_artist",
        artist,
    );
    node.passthrough
        .insert(String::from("name"), artist.name.clone());
    node
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use spotty_api::Pipeline;
    use spotty_api::RateLimitGate;
    use spotty_api::RateLimitPolicy;
    use spotty_api::SpotifyClient;
    use spotty_api::SpotifyTransport;
    use spotty_api::TokenManager;
    use spotty_cache::AccountStore;
    use spotty_cache::DualCache;
    use spotty_types::IconCode;
    use tempfile::TempDir;

    pub(crate) fn build_test_client(mock_uri: &str, tmp: &TempDir) -> SpotifyClient {
        build_test_client_for_user(mock_uri, tmp, "alice")
    }

    pub(crate) fn build_test_client_for_user(
        mock_uri: &str,
        tmp: &TempDir,
        user_id: &str,
    ) -> SpotifyClient {
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
            .cache_tokens(&client_id, user_id, "tok-test", 3600, None)
            .expect("seed access token");

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
            .expect("build SpotifyTransport"),
        );
        let pipeline = Pipeline::new(Arc::clone(&transport));

        SpotifyClient::new(
            transport,
            pipeline,
            cache,
            client_id,
            user_id,
            "US",
            RateLimitPolicy::FailFastServer,
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use super::test_support::build_test_client;
    use super::*;

    const ARTIST_ID: &str = "1111111111111111111111";

    async fn mount_artist_fixtures(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{ARTIST_ID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": ARTIST_ID,
                "uri": format!("spotify:artist:{ARTIST_ID}"),
                "name": "Miles Davis",
                "images": [{"url": "https://img/miles.jpg", "width": 640, "height": 640}],
                "followers": {"total": 500000},
                "genres": ["jazz"]
            })))
            .mount(server)
            .await;

        let artist_json = json!({
            "id": ARTIST_ID,
            "uri": format!("spotify:artist:{ARTIST_ID}"),
            "name": "Miles Davis"
        });
        let va_json = json!({
            "id": "va11111111111111111111",
            "uri": "spotify:artist:va11111111111111111111",
            "name": "Various Artists"
        });

        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{ARTIST_ID}/top-tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [
                    {
                        "id": "t111111111111111111111",
                        "uri": "spotify:track:t111111111111111111111",
                        "name": "So What",
                        "artists": [artist_json],
                        "album": {
                            "id": "a111111111111111111111",
                            "uri": "spotify:album:a111111111111111111111",
                            "name": "Kind of Blue",
                            "album_type": "album",
                            "release_date": "1959-08-17",
                            "images": []
                        },
                        "duration_ms": 545000,
                        "track_number": 1,
                        "disc_number": 1,
                        "explicit": false,
                        "is_playable": true
                    },
                    {
                        "id": "t222222222222222222222",
                        "uri": "spotify:track:t222222222222222222222",
                        "name": "Explicit Jam",
                        "artists": [artist_json],
                        "album": {
                            "id": "a111111111111111111111",
                            "uri": "spotify:album:a111111111111111111111",
                            "name": "Kind of Blue",
                            "album_type": "album",
                            "release_date": "1959-08-17",
                            "images": []
                        },
                        "duration_ms": 200000,
                        "track_number": 2,
                        "disc_number": 1,
                        "explicit": true,
                        "is_playable": true
                    }
                ]
            })))
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{ARTIST_ID}/albums")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 4,
                "next": null,
                "items": [
                    {
                        "id": "alb1111111111111111111",
                        "uri": "spotify:album:alb1111111111111111111",
                        "name": "Kind of Blue",
                        "album_type": "album",
                        "release_date": "1959-08-17",
                        "artists": [artist_json],
                        "images": []
                    },
                    {
                        "id": "alb2222222222222222222",
                        "uri": "spotify:album:alb2222222222222222222",
                        "name": "Bitches Brew",
                        "album_type": "album",
                        "release_date": "1970-03-30",
                        "artists": [artist_json],
                        "images": []
                    },
                    {
                        "id": "sng1111111111111111111",
                        "uri": "spotify:album:sng1111111111111111111",
                        "name": "Blue in Green (Single)",
                        "album_type": "single",
                        "release_date": "1960-01-01",
                        "artists": [artist_json],
                        "images": []
                    },
                    {
                        "id": "cmp1111111111111111111",
                        "uri": "spotify:album:cmp1111111111111111111",
                        "name": "Jazz Classics",
                        "album_type": "album",
                        "release_date": "1995-05-01",
                        "artists": [va_json],
                        "images": []
                    }
                ]
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn fetch_artist_overview_partitions_albums_and_renders_actions() {
        let server = MockServer::start().await;
        mount_artist_fixtures(&server).await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let overview = fetch_artist_overview(
            &client,
            &format!("spotify:artist:{ARTIST_ID}"),
            AlbumScope::FullArtistCatalog,
            ExplicitFilterMode::HideExplicit,
            SortOrder::NaturalOrder,
        )
        .await
        .expect("3-way artist overview should succeed");

        assert_eq!(overview.name, "Miles Davis", "root name mismatch");
        assert_eq!(
            overview.items.len(),
            7,
            "expected 7 artist sections/actions"
        );

        let albums = &overview.items[0];
        assert_eq!(albums.name, "Albums", "section 0 should be Albums");
        assert_eq!(albums.items.len(), 2, "should contain 2 regular albums");
        // NaturalOrder keeps release-date descending order (1970 before 1959).
        assert_eq!(
            albums.items[0].line1.as_deref(),
            Some("Bitches Brew"),
            "newest album should come first"
        );
        assert_eq!(albums.items[1].line1.as_deref(), Some("Kind of Blue"));

        let singles = &overview.items[1];
        assert_eq!(singles.name, "Singles & EPs");
        assert_eq!(singles.items.len(), 1);

        let comps = &overview.items[2];
        assert_eq!(comps.name, "Compilations");
        assert_eq!(comps.items.len(), 1);

        let top_tracks = &overview.items[3];
        assert_eq!(top_tracks.name, "Top Tracks");
        assert_eq!(
            top_tracks.items.len(),
            1,
            "HideExplicit should filter explicit top track"
        );

        assert_eq!(overview.items[4].name, "Artist Radio");
        assert_eq!(
            overview.items[4]
                .passthrough
                .get("route")
                .map(String::as_str),
            Some("artist_radio")
        );
        assert_eq!(overview.items[5].name, "Related Artists");
        assert_eq!(
            overview.items[5]
                .passthrough
                .get("route")
                .map(String::as_str),
            Some("related_artists")
        );
        assert_eq!(overview.items[6].name, "Follow artist");
        assert_eq!(
            overview.items[6]
                .passthrough
                .get("route")
                .map(String::as_str),
            Some("follow_artist")
        );
    }

    #[tokio::test]
    async fn fetch_artist_overview_filters_saved_albums_in_my_albums_only_mode() {
        let server = MockServer::start().await;
        mount_artist_fixtures(&server).await;

        let expected_ids = concat!(
            "cmp1111111111111111111,",
            "alb2222222222222222222,",
            "sng1111111111111111111,",
            "alb1111111111111111111"
        );
        Mock::given(method("GET"))
            .and(path("/v1/me/albums/contains"))
            .and(query_param("ids", expected_ids))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([false, true, false, false])),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let overview = fetch_artist_overview(
            &client,
            ARTIST_ID,
            AlbumScope::MyAlbumsOnly,
            ExplicitFilterMode::AllowAll,
            SortOrder::Alphabetical,
        )
        .await
        .expect("MyAlbumsOnly artist overview should succeed");

        assert_eq!(
            overview.items[0].name, "Albums",
            "only saved Albums section should remain"
        );
        assert_eq!(overview.items[0].items.len(), 1);
        assert_eq!(
            overview.items[0].items[0].line1.as_deref(),
            Some("Bitches Brew")
        );
        // Unsaved Singles & Compilations are omitted; Top Tracks + 3 actions follow.
        assert_eq!(overview.items[1].name, "Top Tracks");
        assert_eq!(overview.items[1].items.len(), 2);
    }

    #[test]
    fn partition_albums_matches_artist_name_case_insensitively() {
        let mut artist = NormalizedArtist::default();
        artist.id = String::from("a1");
        artist.uri = String::from("spotify:artist:a1");
        artist.name = String::from("Miles Davis");

        let mut album = NormalizedAlbum::default();
        album.id = String::from("alb1");
        album.uri = String::from("spotify:album:alb1");
        album.name = String::from("Kind of Blue");
        album.artist = String::from("miles davis");
        album.album_type = String::from("album");
        album.release_date = String::from("1959-08-17");

        let (regular, singles, compilations) = partition_albums(&artist, vec![album]);
        assert_eq!(regular.len(), 1);
        assert!(singles.is_empty());
        assert!(compilations.is_empty());
    }
}
