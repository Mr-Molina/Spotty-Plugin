//! Context info menus, radio seeding, playlist actions, and playback transfer
//! (`OPML.pm:1045-1112 & 1479-1815`).

use std::collections::BTreeMap;
use std::collections::HashMap;

use spotty_api::ArtistSummary;
use spotty_api::NormalizedPlaylist;
use spotty_api::RecommendationSeeds;
use spotty_api::SearchKind;
use spotty_api::SpotifyClient;
use spotty_api::TrackCacheLookup;
use spotty_api::RECOMMENDATION_LIMIT;
use spotty_api::SPOTIFY_LIMIT;
use spotty_playlist_folders::build_folder_tree;
use spotty_playlist_folders::FolderEntry;
use spotty_playlist_folders::FolderTreeItem;
use spotty_types::AlbumScope;
use spotty_types::ClientId;
use spotty_types::ExplicitFilterMode;
use spotty_types::LmsHostCallbacks;
use spotty_types::PlaylistCommand;
use spotty_types::Result;
use spotty_types::SortOrder;
use spotty_types::SpotifyEntityKind;
use spotty_types::SpotifyUri;

use crate::artist_view::fetch_artist_overview;
use crate::formatters::format_artist_list;
use crate::formatters::format_track_list;
use crate::formatters::OpmlItemKind;
use crate::formatters::OpmlNode;
use crate::formatters::TrackListOptions;
use crate::formatters::DEFAULT_SORT_ARTICLES;
use crate::home::IMG_ACCOUNT;
use crate::home::IMG_ALBUM;
use crate::home::IMG_ARTIST;
use crate::home::IMG_COLLABORATIVE;
use crate::home::IMG_PLAYLIST;
use crate::home::IMG_PODCAST;
use crate::home::IMG_SONG;

const ON_SPOTIFY_PREFIX: &str = "On Spotify: ";
const MAX_TRANSFER_EPISODES: usize = 500;
const MAX_USER_PLAYLISTS: usize = 500;

/// Controls whether podcast search categories are included in search menus.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PodcastCapability {
    /// Include Shows and Episodes search buckets (`podcasts` capability).
    #[default]
    Enabled,
    /// Omit podcast search buckets.
    Disabled,
}

/// Seed entity kind for [`track_or_artist_radio`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioSeedKind {
    /// Seed recommendations from a Spotify track URI or ID.
    Track,
    /// Seed recommendations from a Spotify artist URI or ID.
    Artist,
}

/// Metadata descriptor passed to context info menu builders.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextMetadata {
    /// Primary artist display name when `artists` is empty.
    pub artist_name: Option<String>,
    /// Optional primary artist `spotify:artist:<id>` URI.
    pub artist_uri: Option<String>,
    /// Full artist credit list for tracks or albums.
    pub artists: Vec<ArtistSummary>,
    /// Album title.
    pub album_name: Option<String>,
    /// Optional album `spotify:album:<id>` URI.
    pub album_uri: Option<String>,
    /// Track title (used for non-Spotify search fallback).
    pub title: Option<String>,
    /// Optional external Spotify URI (`extid`).
    pub extid: Option<String>,
}

impl ContextMetadata {
    /// Creates an empty [`ContextMetadata`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Builds the LMS track context menu (`OPML.pm:trackInfoMenu`).
///
/// When `url` is a `spotify:` URI, normalizes slashes and checks
/// [`SpotifyClient::track_cached`] (`CacheOnly`) for full artist and album
/// credits; otherwise falls back to `fallback` metadata for cross-service
/// search links.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if reading from the local metadata
/// cache fails or `url` is a malformed `spotify:` URI.
pub async fn track_info_menu(
    client: &SpotifyClient,
    url: &str,
    fallback: Option<&ContextMetadata>,
) -> Result<Vec<OpmlNode>> {
    if url.starts_with("spotify:") {
        let normalized_uri = url.replace('/', "");
        let cached = client
            .track_cached(&normalized_uri, TrackCacheLookup::CacheOnly)
            .await?;

        let mut meta = ContextMetadata::new();
        meta.extid = Some(normalized_uri);
        if let Some(track) = cached {
            meta.artists = track.artists;
            if !track.album.name.is_empty() {
                meta.album_name = Some(track.album.name);
            }
            if !track.album.uri.is_empty() {
                meta.album_uri = Some(track.album.uri);
            }
        } else if let Some(fb) = fallback {
            meta.artists = fb.artists.clone();
            meta.artist_name = fb.artist_name.clone();
            meta.artist_uri = fb.artist_uri.clone();
            meta.album_name = fb.album_name.clone();
            meta.album_uri = fb.album_uri.clone();
        }
        return Ok(build_obj_info_menu(&meta));
    }

    let Some(fb) = fallback else {
        return Ok(Vec::new());
    };
    let has_identity = fb.title.as_deref().is_some_and(|t| !t.is_empty())
        || fb.extid.as_deref().is_some_and(|u| !u.is_empty());
    if !has_identity {
        return Ok(Vec::new());
    }
    Ok(build_obj_info_menu(fb))
}

/// Builds the LMS artist context menu (`OPML.pm:artistInfoMenu`).
#[must_use]
pub fn artist_info_menu(meta: &ContextMetadata) -> Vec<OpmlNode> {
    let mut normalized = ContextMetadata::new();
    normalized.artist_name = meta.artist_name.clone();
    normalized.artist_uri = meta.artist_uri.clone().or_else(|| meta.extid.clone());
    normalized.artists = meta.artists.clone();
    normalized.extid = meta.extid.clone().or_else(|| meta.artist_uri.clone());
    build_obj_info_menu(&normalized)
}

/// Builds the LMS album context menu (`OPML.pm:albumInfoMenu`).
#[must_use]
pub fn album_info_menu(meta: &ContextMetadata) -> Vec<OpmlNode> {
    let ext_album_uri = meta
        .extid
        .as_deref()
        .filter(|u| u.starts_with("spotify:"))
        .map(str::to_owned)
        .or_else(|| meta.album_uri.clone());

    let mut normalized = ContextMetadata::new();
    normalized.album_name = meta.album_name.clone();
    normalized.album_uri = ext_album_uri.clone();
    normalized.artists = meta.artists.clone();
    normalized.artist_name = meta.artist_name.clone();
    normalized.artist_uri = meta.artist_uri.clone();
    normalized.extid = ext_album_uri;

    let mut items = build_obj_info_menu(&normalized);
    if normalized
        .extid
        .as_deref()
        .is_some_and(|u| u.starts_with("spotify:album:"))
    {
        items.push(OpmlNode::new("Source: Spotify", OpmlItemKind::Text));
    }
    items
}

/// Resolves a local-library artist to a Spotify artist overview or candidate
/// list (`OPML.pm:browseArtistMenu`).
///
/// Checks `ext_ids` for a `spotify:artist:` URI first; otherwise searches
/// Spotify for `artist:"<artist_name>"` and returns the full artist overview on
/// a unique match.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if the Spotify search or artist
/// overview request fails.
pub async fn browse_artist_menu(
    client: &SpotifyClient,
    artist_name: &str,
    ext_ids: &[String],
    album_scope: AlbumScope,
    explicit_filter: ExplicitFilterMode,
    sort_order: SortOrder,
) -> Result<Vec<OpmlNode>> {
    for ext_id in ext_ids {
        if ext_id.starts_with("spotify:artist:") {
            let overview =
                fetch_artist_overview(client, ext_id, album_scope, explicit_filter, sort_order)
                    .await?;
            return Ok(overview.items);
        }
    }

    let trimmed = artist_name.trim();
    if trimmed.is_empty() {
        return Ok(vec![OpmlNode::new("Empty", OpmlItemKind::Text)]);
    }

    let query = format!("artist:\"{trimmed}\"");
    let results = client
        .search(&query, SearchKind::Artist, SPOTIFY_LIMIT, SPOTIFY_LIMIT)
        .await?;

    if results.artists.len() == 1 {
        let overview = fetch_artist_overview(
            client,
            &results.artists[0].uri,
            album_scope,
            explicit_filter,
            sort_order,
        )
        .await?;
        return Ok(overview.items);
    }

    let mut exact = Vec::new();
    for artist in results.artists {
        if artist.name.eq_ignore_ascii_case(trimmed) {
            exact.push(artist);
        }
    }

    if exact.len() == 1 {
        let overview = fetch_artist_overview(
            client,
            &exact[0].uri,
            album_scope,
            explicit_filter,
            sort_order,
        )
        .await?;
        return Ok(overview.items);
    }

    if exact.is_empty() {
        return Ok(vec![OpmlNode::new("Empty", OpmlItemKind::Text)]);
    }

    let list = format_artist_list(
        &exact,
        SortOrder::NaturalOrder,
        album_scope,
        DEFAULT_SORT_ARTICLES,
        "Followers:",
    );
    Ok(list.items)
}

/// Builds the LMS Global Search provider sub-menu (`OPML.pm:94-103`).
///
/// Strips `image` icons from category items to match LMS Global Search UI
/// expectations (`delete $_->{image}`).
#[must_use]
pub fn global_search_menu(query: &str, podcasts: PodcastCapability) -> OpmlNode {
    let mut items = search_category_nodes(query, podcasts);
    for item in &mut items {
        item.image = None;
    }

    let mut root = OpmlNode::new("Spotty", OpmlItemKind::Outline);
    root.items = items;
    root
}

/// Builds category sub-search links (`OPML.pm:_searchItems`).
#[must_use]
pub(crate) fn search_category_nodes(query: &str, podcasts: PodcastCapability) -> Vec<OpmlNode> {
    let mut specs: Vec<(&str, &str, &str)> = vec![
        ("Artists", "artist", IMG_ARTIST),
        ("Albums", "album", IMG_ALBUM),
        ("Playlists", "playlist", IMG_PLAYLIST),
        ("Songs", "track", IMG_SONG),
    ];
    if podcasts == PodcastCapability::Enabled {
        specs.push(("Podcasts", "show", IMG_PODCAST));
        specs.push(("Podcast Episodes", "episode", IMG_PODCAST));
    }
    specs.push(("Users", "user", IMG_ACCOUNT));

    let mut items = Vec::with_capacity(specs.len());
    for (label, search_type, icon) in specs {
        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("route"), String::from("search"));
        passthrough.insert(String::from("query"), String::from(query));
        passthrough.insert(String::from("type"), String::from(search_type));

        let mut node = OpmlNode::new(label, OpmlItemKind::Link);
        node.image = Some(String::from(icon));
        node.passthrough = passthrough;
        items.push(node);
    }
    items
}

/// Lists playlists owned by the authenticated user so a track can be added
/// (`OPML.pm:addTrackToPlaylist`).
///
/// Filters out playlists owned by other users and optionally groups results
/// using `hierarchy`.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if fetching user playlists fails.
pub async fn add_track_to_playlist_menu(
    client: &SpotifyClient,
    track_uri: &str,
    hierarchy: Option<&HashMap<String, FolderEntry>>,
) -> Result<Vec<OpmlNode>> {
    let user_id = client.user_id();
    let playlists = client.user_playlists(user_id, MAX_USER_PLAYLISTS).await?;

    let mut owned = Vec::new();
    for playlist in playlists {
        if !playlist.creator.is_empty() && playlist.creator != user_id {
            continue;
        }
        owned.push(playlist);
    }

    if let Some(folder_map) = hierarchy {
        if !folder_map.is_empty() {
            let tree = build_folder_tree(&owned, folder_map, |pl| pl.uri.as_str());
            return Ok(convert_add_folder_tree(&tree, track_uri));
        }
    }

    let mut items = Vec::with_capacity(owned.len());
    for playlist in &owned {
        items.push(build_add_playlist_target_node(playlist, track_uri));
    }
    Ok(items)
}

/// Appends `track_uri` to `playlist_uri_or_id` (`OPML.pm:_addTrackToPlaylist`).
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if adding the track to Spotify fails.
pub async fn add_track_to_playlist_action(
    client: &SpotifyClient,
    playlist_uri_or_id: &str,
    track_uri: &str,
) -> Result<Vec<OpmlNode>> {
    client
        .add_tracks_to_playlist(playlist_uri_or_id, &[track_uri])
        .await?;
    Ok(vec![OpmlNode::new(
        "Music has been added",
        OpmlItemKind::Text,
    )])
}

/// Fetches radio recommendations seeded by a single track or artist URI/ID
/// (`OPML.pm:trackRadio` / `artistRadio` / `_radio`).
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError::InvalidUri`] when the extracted seed is
/// empty, or [`spotty_types::SpottyError`] if `/v1/recommendations` fails.
pub async fn track_or_artist_radio(
    client: &SpotifyClient,
    seed_kind: RadioSeedKind,
    uri_or_id: &str,
    explicit_filter: ExplicitFilterMode,
) -> Result<Vec<OpmlNode>> {
    let seed_id = extract_trailing_id(uri_or_id);
    let seeds = match seed_kind {
        RadioSeedKind::Track => RecommendationSeeds::build(&[seed_id], &[], &[]),
        RadioSeedKind::Artist => RecommendationSeeds::build(&[], &[seed_id], &[]),
    };
    let tracks = client.recommendations(&seeds, RECOMMENDATION_LIMIT).await?;
    Ok(format_track_list(
        &tracks,
        explicit_filter,
        &TrackListOptions::default(),
    ))
}

/// Builds the Transfer Playback confirmation menu (`OPML.pm:transferPlaylist`).
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if querying `/v1/me/player` fails.
pub async fn transfer_playback_menu(client: &SpotifyClient) -> Result<Vec<OpmlNode>> {
    let Some(state) = client.player().await? else {
        return Ok(vec![OpmlNode::new(
            "No Spotify playing device was found.",
            OpmlItemKind::Textarea,
        )]);
    };

    let Some(context_uri) = state.context_uri else {
        return Ok(vec![OpmlNode::new(
            "No Spotify playing device was found.",
            OpmlItemKind::Textarea,
        )]);
    };

    let device_name = state
        .device
        .as_ref()
        .map(|d| d.name.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("Spotify Connect");

    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("route"), String::from("do_transfer_playlist"));
    passthrough.insert(String::from("context_uri"), context_uri);

    let mut action = OpmlNode::new(device_name, OpmlItemKind::Link);
    action.passthrough = passthrough;

    Ok(vec![
        OpmlNode::new(
            "You can transfer Spotify playback from below listed device to your Squeezebox. \
             Please note that it's not always possible to restore the exact same playlist.",
            OpmlItemKind::Textarea,
        ),
        action,
    ])
}

/// Transfers active Spotify Connect playback onto an LMS `player_id`
/// (`OPML.pm:_doTransferPlaylist`).
///
/// Queries `client.player()`, resolves the active context's playable track
/// URIs, and emits `PlaylistCommand::Clear`, `PlaylistCommand::PlayTracks`,
/// optional `PlaylistCommand::JumpIndex`, and optional
/// `PlaylistCommand::SeekSeconds(progress_ms / 1000)` through `host`.
///
/// Returns `Ok(true)` when playback commands were dispatched, or `Ok(false)`
/// when no active player context or playable tracks exist.
///
/// # Errors
///
/// Returns [`spotty_types::SpottyError`] if querying `/v1/me/player`, resolving
/// context tracks, or dispatching commands through [`LmsHostCallbacks`] fails.
pub async fn transfer_playback_from_spotify(
    client: &SpotifyClient,
    host: &dyn LmsHostCallbacks,
    player_id: &ClientId,
) -> Result<bool> {
    let Some(state) = client.player().await? else {
        return Ok(false);
    };
    let Some(context_uri) = state.context_uri.as_deref() else {
        return Ok(false);
    };

    let raw_uris = resolve_context_track_uris(client, context_uri).await?;
    if raw_uris.is_empty() {
        return Ok(false);
    }

    let current_uri = state
        .track
        .as_ref()
        .map(|t| t.uri.as_str())
        .or_else(|| state.episode.as_ref().map(|e| e.uri.as_str()));

    let mut jump_index: Option<usize> = None;
    let mut play_uris = Vec::with_capacity(raw_uris.len());

    for raw_uri in raw_uris {
        let Some(play_url) = to_play_protocol_url(&raw_uri) else {
            continue;
        };
        if jump_index.is_none() && current_uri == Some(raw_uri.as_str()) {
            jump_index = Some(play_uris.len());
        }
        play_uris.push(play_url);
    }

    if play_uris.is_empty() {
        return Ok(false);
    }

    host.execute_playlist_command(player_id, PlaylistCommand::Clear)?;
    host.execute_playlist_command(player_id, PlaylistCommand::PlayTracks(play_uris))?;
    if let Some(idx) = jump_index {
        if idx > 0 {
            host.execute_playlist_command(player_id, PlaylistCommand::JumpIndex(idx))?;
        }
    }
    if let Some(progress_ms) = state.progress_ms {
        let secs = progress_ms / 1000;
        if secs > 0 {
            host.execute_playlist_command(player_id, PlaylistCommand::SeekSeconds(secs))?;
        }
    }

    Ok(true)
}

async fn resolve_context_track_uris(
    client: &SpotifyClient,
    context_uri: &str,
) -> Result<Vec<String>> {
    use std::str::FromStr as _;

    let parsed = SpotifyUri::from_str(context_uri)?;
    match parsed.kind() {
        SpotifyEntityKind::Playlist => client.playlist_track_uris(context_uri).await,
        SpotifyEntityKind::Album => {
            let album = client.album(context_uri).await?;
            let mut uris = Vec::with_capacity(album.tracks.len());
            for track in album.tracks {
                if track.is_playable {
                    uris.push(track.uri);
                }
            }
            Ok(uris)
        }
        SpotifyEntityKind::Artist => {
            let tracks = client.artist_tracks(context_uri).await?;
            let mut uris = Vec::with_capacity(tracks.len());
            for track in tracks {
                if track.is_playable {
                    uris.push(track.uri);
                }
            }
            Ok(uris)
        }
        SpotifyEntityKind::Show => {
            let episodes = client
                .episodes(context_uri, SPOTIFY_LIMIT, MAX_TRANSFER_EPISODES)
                .await?;
            let mut uris = Vec::with_capacity(episodes.len());
            for episode in episodes {
                if episode.is_playable {
                    uris.push(episode.uri);
                }
            }
            Ok(uris)
        }
        _ => Ok(Vec::new()),
    }
}

fn to_play_protocol_url(raw_uri: &str) -> Option<String> {
    let suffix = raw_uri.strip_prefix("spotify:")?;
    if suffix.starts_with("track:") || suffix.starts_with("episode:") {
        Some(format!("spotify://{suffix}"))
    } else {
        None
    }
}

fn extract_trailing_id(uri_or_id: &str) -> &str {
    let trimmed = uri_or_id.trim();
    match trimmed.rsplit_once(':') {
        Some((_, tail)) => tail,
        None => trimmed,
    }
}

fn build_obj_info_menu(meta: &ContextMetadata) -> Vec<OpmlNode> {
    let mut items = Vec::new();
    let uri = meta.extid.as_deref();

    if let Some(spotify_uri) = uri.filter(|u| u.starts_with("spotify:")) {
        if spotify_uri.starts_with("spotify:track:") {
            let mut radio_pass = BTreeMap::new();
            radio_pass.insert(String::from("route"), String::from("track_radio"));
            radio_pass.insert(String::from("uri"), String::from(spotify_uri));
            let mut radio = OpmlNode::new("Title Radio", OpmlItemKind::Playlist);
            radio.url = Some(String::from(spotify_uri));
            radio.passthrough = radio_pass;
            items.push(radio);

            let mut add_pass = BTreeMap::new();
            add_pass.insert(String::from("route"), String::from("add_track_to_playlist"));
            add_pass.insert(String::from("uri"), String::from(spotify_uri));
            let mut add = OpmlNode::new("Add Track to Playlist", OpmlItemKind::Link);
            add.passthrough = add_pass;
            items.push(add);
        }

        let artists = collect_artists(meta);
        for artist in artists {
            if artist.uri.is_empty() {
                continue;
            }
            let mut passthrough = BTreeMap::new();
            passthrough.insert(String::from("route"), String::from("artist"));
            passthrough.insert(String::from("uri"), artist.uri.clone());
            let mut node = OpmlNode::new(
                format!("{ON_SPOTIFY_PREFIX}{}", artist.name),
                OpmlItemKind::Link,
            );
            node.url = Some(artist.uri);
            node.passthrough = passthrough;
            items.push(node);
        }

        if let (Some(album_name), Some(album_uri)) =
            (meta.album_name.as_deref(), meta.album_uri.as_deref())
        {
            if !album_uri.is_empty() {
                let mut passthrough = BTreeMap::new();
                passthrough.insert(String::from("route"), String::from("album"));
                passthrough.insert(String::from("uri"), String::from(album_uri));
                let mut node = OpmlNode::new(
                    format!("{ON_SPOTIFY_PREFIX}{album_name}"),
                    OpmlItemKind::Link,
                );
                node.url = Some(String::from(album_uri));
                node.passthrough = passthrough;
                items.push(node);
            }
        }
        return items;
    }

    for artist in collect_artists(meta) {
        if artist.name.is_empty() {
            continue;
        }
        items.push(build_context_search_node(
            &format!("{ON_SPOTIFY_PREFIX}{}", artist.name),
            &format!("artist:\"{}\"", artist.name),
        ));
    }

    if let Some(album_name) = meta.album_name.as_deref().filter(|n| !n.is_empty()) {
        items.push(build_context_search_node(
            &format!("{ON_SPOTIFY_PREFIX}{album_name}"),
            &format!("album:\"{album_name}\""),
        ));
    }

    if let Some(title) = meta.title.as_deref().filter(|t| !t.is_empty()) {
        items.push(build_context_search_node(
            &format!("{ON_SPOTIFY_PREFIX}{title}"),
            &format!("track:\"{title}\""),
        ));
    }

    items
}

fn collect_artists(meta: &ContextMetadata) -> Vec<ArtistSummary> {
    let mut out = meta.artists.clone();
    if let Some(name) = meta.artist_name.as_deref().filter(|n| !n.is_empty()) {
        let already_present = out.iter().any(|a| a.name == name);
        if !already_present {
            out.push(ArtistSummary::new(
                "",
                meta.artist_uri.as_deref().unwrap_or(""),
                name,
            ));
        }
    }
    out
}

fn build_context_search_node(label: &str, query: &str) -> OpmlNode {
    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("route"), String::from("search"));
    passthrough.insert(String::from("query"), String::from(query));
    passthrough.insert(String::from("type"), String::from("context"));

    let mut node = OpmlNode::new(label, OpmlItemKind::Link);
    node.passthrough = passthrough;
    node
}

fn convert_add_folder_tree(
    tree: &[FolderTreeItem<NormalizedPlaylist>],
    track_uri: &str,
) -> Vec<OpmlNode> {
    let mut nodes = Vec::with_capacity(tree.len());
    for entry in tree {
        match entry {
            FolderTreeItem::Folder {
                id, name, children, ..
            } => {
                let mut passthrough = BTreeMap::new();
                passthrough.insert(String::from("folder_id"), id.clone());
                let mut folder = OpmlNode::new(name.clone(), OpmlItemKind::Outline);
                folder.image = Some(String::from(IMG_PLAYLIST));
                folder.passthrough = passthrough;
                folder.items = convert_add_folder_tree(children, track_uri);
                nodes.push(folder);
            }
            FolderTreeItem::Leaf { item, .. } => {
                nodes.push(build_add_playlist_target_node(item, track_uri));
            }
            _ => {}
        }
    }
    nodes
}

fn build_add_playlist_target_node(playlist: &NormalizedPlaylist, track_uri: &str) -> OpmlNode {
    let fallback = if playlist.collaborative {
        IMG_COLLABORATIVE
    } else {
        IMG_PLAYLIST
    };
    let image = if playlist.image.is_empty() {
        String::from(fallback)
    } else {
        playlist.image.clone()
    };

    let mut passthrough = BTreeMap::new();
    passthrough.insert(
        String::from("route"),
        String::from("add_track_to_playlist_action"),
    );
    passthrough.insert(String::from("track"), String::from(track_uri));
    passthrough.insert(String::from("playlist"), playlist.uri.clone());

    let mut node = OpmlNode::new(playlist.name.clone(), OpmlItemKind::Link);
    node.image = Some(image);
    node.passthrough = passthrough;
    node
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use super::*;
    use crate::artist_view::test_support::build_test_client;

    #[derive(Default)]
    struct RecordingHost {
        commands: Mutex<Vec<PlaylistCommand>>,
    }

    impl RecordingHost {
        fn recorded(&self) -> Vec<PlaylistCommand> {
            self.commands
                .lock()
                .expect("recording mutex should not poison")
                .clone()
        }
    }

    impl LmsHostCallbacks for RecordingHost {
        fn notify_new_metadata(&self, _client: &ClientId) -> Result<()> {
            Ok(())
        }

        fn trigger_online_library_rescan(&self) -> Result<()> {
            Ok(())
        }

        fn execute_playlist_command(&self, _client: &ClientId, cmd: PlaylistCommand) -> Result<()> {
            self.commands
                .lock()
                .expect("recording mutex should not poison")
                .push(cmd);
            Ok(())
        }
    }

    #[tokio::test]
    async fn track_and_album_and_artist_info_menus_emit_expected_entries() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let mut non_spotify = ContextMetadata::new();
        non_spotify.artist_name = Some(String::from("Bill Evans"));
        non_spotify.album_name = Some(String::from("Waltz for Debby"));
        non_spotify.title = Some(String::from("My Foolish Heart"));

        let fallback_items =
            track_info_menu(&client, "http://example.com/1.flac", Some(&non_spotify))
                .await
                .expect("non-Spotify track info menu should succeed");
        assert_eq!(fallback_items.len(), 3);
        assert_eq!(fallback_items[0].name, "On Spotify: Bill Evans");
        assert_eq!(
            fallback_items[0]
                .passthrough
                .get("query")
                .map(String::as_str),
            Some("artist:\"Bill Evans\"")
        );
        assert_eq!(fallback_items[2].name, "On Spotify: My Foolish Heart");

        let mut spotify_meta = ContextMetadata::new();
        spotify_meta.artists = vec![ArtistSummary::new(
            "a1",
            "spotify:artist:1111111111111111111111",
            "Miles Davis",
        )];
        spotify_meta.album_name = Some(String::from("Kind of Blue"));
        spotify_meta.album_uri = Some(String::from("spotify:album:2222222222222222222222"));

        let spotify_items = track_info_menu(
            &client,
            "spotify://track:3333333333333333333333",
            Some(&spotify_meta),
        )
        .await
        .expect("Spotify track info menu should succeed");
        assert_eq!(spotify_items[0].name, "Title Radio");
        assert_eq!(spotify_items[1].name, "Add Track to Playlist");
        assert_eq!(spotify_items[2].name, "On Spotify: Miles Davis");
        assert_eq!(spotify_items[3].name, "On Spotify: Kind of Blue");

        let mut album_meta = spotify_meta.clone();
        album_meta.extid = Some(String::from("spotify:album:2222222222222222222222"));
        let album_items = album_info_menu(&album_meta);
        assert_eq!(
            album_items.last().map(|n| n.name.as_str()),
            Some("Source: Spotify")
        );

        let mut artist_meta = ContextMetadata::new();
        artist_meta.artist_name = Some(String::from("Miles Davis"));
        artist_meta.extid = Some(String::from("spotify:artist:1111111111111111111111"));
        let artist_items = artist_info_menu(&artist_meta);
        assert_eq!(artist_items.len(), 1);
        assert_eq!(artist_items[0].name, "On Spotify: Miles Davis");
    }

    #[test]
    fn global_search_menu_strips_icons_and_respects_podcast_capability() {
        let with_podcasts = global_search_menu("miles", PodcastCapability::Enabled);
        assert_eq!(with_podcasts.name, "Spotty");
        assert_eq!(with_podcasts.items.len(), 7);
        for item in &with_podcasts.items {
            assert_eq!(item.image, None, "global search must strip icons");
        }

        let no_podcasts = global_search_menu("miles", PodcastCapability::Disabled);
        assert_eq!(no_podcasts.items.len(), 5);
    }

    #[tokio::test]
    async fn track_or_artist_radio_and_add_track_to_playlist_hit_spotify_endpoints() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/recommendations"))
            .and(query_param("seed_tracks", "t111111111111111111111"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": [{
                    "id": "rec1111111111111111111",
                    "uri": "spotify:track:rec1111111111111111111",
                    "name": "Recommended Tune",
                    "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "Herbie Hancock"}],
                    "album": {
                        "id": "al1",
                        "uri": "spotify:album:al1",
                        "name": "Maiden Voyage",
                        "album_type": "album",
                        "images": []
                    },
                    "duration_ms": 240000,
                    "track_number": 1,
                    "disc_number": 1,
                    "explicit": false,
                    "is_playable": true
                }]
            })))
            .mount(&server)
            .await;

        let radio = track_or_artist_radio(
            &client,
            RadioSeedKind::Track,
            "spotify:track:t111111111111111111111",
            ExplicitFilterMode::AllowAll,
        )
        .await
        .expect("track radio should succeed");
        assert_eq!(radio.len(), 1);
        assert_eq!(
            radio[0].url.as_deref(),
            Some("spotify://track:rec1111111111111111111")
        );

        Mock::given(method("GET"))
            .and(path("/v1/users/alice/playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 2,
                "next": null,
                "items": [
                    {
                        "id": "pl11111111111111111111",
                        "uri": "spotify:playlist:pl11111111111111111111",
                        "name": "My Mix",
                        "owner": {"id": "alice"},
                        "collaborative": false,
                        "tracks": {"total": 5}
                    },
                    {
                        "id": "pl22222222222222222222",
                        "uri": "spotify:playlist:pl22222222222222222222",
                        "name": "Someone Else",
                        "owner": {"id": "bob"},
                        "collaborative": false,
                        "tracks": {"total": 10}
                    }
                ]
            })))
            .mount(&server)
            .await;

        let targets = add_track_to_playlist_menu(&client, "spotify:track:t1", None)
            .await
            .expect("owned playlist list should succeed");
        assert_eq!(targets.len(), 1, "followed playlist by bob must be skipped");
        assert_eq!(targets[0].name, "My Mix");

        Mock::given(method("POST"))
            .and(path("/v1/playlists/pl11111111111111111111/tracks"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"snapshot_id": "s1"})))
            .mount(&server)
            .await;

        let added = add_track_to_playlist_action(
            &client,
            "spotify:playlist:pl11111111111111111111",
            "spotify:track:1111111111111111111111",
        )
        .await
        .expect("adding track to playlist should succeed");
        assert_eq!(added[0].name, "Music has been added");
    }

    #[tokio::test]
    async fn transfer_playback_from_spotify_emits_clear_play_jump_and_seek() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        let playlist_id = "pl11111111111111111111";
        let active_track = "spotify:track:3333333333333333333333";

        Mock::given(method("GET"))
            .and(path("/v1/me/player"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "is_playing": true,
                "progress_ms": 45_600,
                "currently_playing_type": "track",
                "device": {
                    "id": "dev1",
                    "name": "Kitchen Speaker",
                    "type": "Speaker",
                    "is_active": true
                },
                "context": {"uri": format!("spotify:playlist:{playlist_id}")},
                "item": {
                    "id": "3333333333333333333333",
                    "uri": active_track,
                    "name": "Blue in Green",
                    "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "Miles Davis"}],
                    "album": {
                        "id": "al1",
                        "uri": "spotify:album:al1",
                        "name": "Kind of Blue",
                        "album_type": "album",
                        "images": []
                    },
                    "duration_ms": 327000,
                    "track_number": 3,
                    "disc_number": 1,
                    "explicit": false,
                    "is_playable": true
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/playlists/{playlist_id}/tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 3,
                "next": null,
                "items": [
                    {"track": {"uri": "spotify:track:1111111111111111111111", "is_local": false}},
                    {"track": {"uri": "spotify:track:2222222222222222222222", "is_local": false}},
                    {"track": {"uri": active_track, "is_local": false}}
                ]
            })))
            .mount(&server)
            .await;

        let host = RecordingHost::default();
        let player_id = ClientId::new("aa:bb:cc:11:22:33");
        let transferred = transfer_playback_from_spotify(&client, &host, &player_id)
            .await
            .expect("playback transfer should succeed");
        assert!(transferred, "expected transfer to return true");

        assert_eq!(
            host.recorded(),
            vec![
                PlaylistCommand::Clear,
                PlaylistCommand::PlayTracks(vec![
                    String::from("spotify://track:1111111111111111111111"),
                    String::from("spotify://track:2222222222222222222222"),
                    String::from("spotify://track:3333333333333333333333"),
                ]),
                PlaylistCommand::JumpIndex(2),
                PlaylistCommand::SeekSeconds(45),
            ],
            "transfer command sequence mismatch"
        );
    }

    #[tokio::test]
    async fn browse_artist_menu_searches_when_no_extid_and_loads_overview() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);
        let artist_id = "1111111111111111111111";

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "artists": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": artist_id,
                        "uri": format!("spotify:artist:{artist_id}"),
                        "name": "Miles Davis",
                        "images": [],
                        "followers": {"total": 100}
                    }]
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{artist_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": artist_id,
                "uri": format!("spotify:artist:{artist_id}"),
                "name": "Miles Davis",
                "images": [],
                "followers": {"total": 100},
                "genres": []
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{artist_id}/top-tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tracks": []})))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{artist_id}/albums")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 0,
                "next": null,
                "items": []
            })))
            .mount(&server)
            .await;

        let items = browse_artist_menu(
            &client,
            "Miles Davis",
            &[],
            AlbumScope::FullArtistCatalog,
            ExplicitFilterMode::AllowAll,
            SortOrder::NaturalOrder,
        )
        .await
        .expect("browse_artist_menu search resolution should succeed");

        assert_eq!(
            items.len(),
            3,
            "expected Artist Radio, Related Artists, Follow artist"
        );
        assert_eq!(items[0].name, "Artist Radio");
    }
}
