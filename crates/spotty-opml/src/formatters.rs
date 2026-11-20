//! Pure OPML list formatters and alphabetical index bucket builders.

use std::collections::BTreeMap;
use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;
use spotty_api::strip_leading_articles;
use spotty_api::NormalizedAlbum;
use spotty_api::NormalizedArtist;
use spotty_api::NormalizedEpisode;
use spotty_api::NormalizedPlaylist;
use spotty_api::NormalizedShow;
use spotty_api::NormalizedTrack;
use spotty_playlist_folders::build_folder_tree;
use spotty_playlist_folders::FolderEntry;
use spotty_playlist_folders::FolderTreeItem;
use spotty_types::AlbumScope;
use spotty_types::ExplicitFilterMode;
use spotty_types::PodcastEpisodeOrder;
use spotty_types::SortOrder;

use crate::home::IMG_ALBUM;
use crate::home::IMG_ARTIST;
use crate::home::IMG_COLLABORATIVE;
use crate::home::IMG_PLAYLIST;
use crate::home::IMG_PODCAST;
use crate::home::IMG_TRACK;

/// Default leading articles stripped when sorting or bucketing titles.
pub const DEFAULT_SORT_ARTICLES: &[&str] = &["the", "a", "an"];

const MAX_EPISODE_DESC_CHARS: usize = 512;

/// OPML menu item kind matching Logitech Media Server feed types.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OpmlItemKind {
    /// Navigable link item (`type => 'link'`).
    #[default]
    Link,
    /// Playable container item (`type => 'playlist'`).
    Playlist,
    /// Directly playable audio item (`on_select => 'play'`).
    Audio,
    /// Interactive search prompt item (`type => 'search'`).
    Search,
    /// Non-playable single-line text label (`type => 'text'`).
    Text,
    /// Multi-line descriptive text block (`type => 'textarea'`).
    Textarea,
    /// Expandable folder or sub-menu container (`type => 'outline'`).
    Outline,
}

/// Alphabetical jump-bar bucket (`indexList` entry `[label, count]`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpmlIndexBucket {
    /// Uppercase initial character label for the bucket.
    pub label: String,
    /// Number of consecutive items belonging to this bucket.
    pub count: usize,
}

impl OpmlIndexBucket {
    /// Creates a new [`OpmlIndexBucket`].
    #[must_use]
    pub fn new(label: impl Into<String>, count: usize) -> Self {
        Self {
            label: label.into(),
            count,
        }
    }
}

/// Pure OPML feed node serialized for LMS menu responses.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpmlNode {
    /// Primary display title.
    pub name: String,
    /// Optional first line for two-line player UIs.
    pub line1: Option<String>,
    /// Optional second line for two-line player UIs.
    pub line2: Option<String>,
    /// LMS OPML node type.
    pub kind: OpmlItemKind,
    /// Target playback URI or route identifier.
    pub url: Option<String>,
    /// Artwork URL or plugin-relative icon path.
    pub image: Option<String>,
    /// Whether selecting this item starts `playall` queue playback.
    pub playall: bool,
    /// Route parameters forwarded to child OPML handlers.
    pub passthrough: BTreeMap<String, String>,
    /// Nested child nodes for outline or folder containers.
    pub items: Vec<OpmlNode>,
    /// Alphabetical index buckets when formatting sorted lists.
    pub index_list: Vec<OpmlIndexBucket>,
}

impl OpmlNode {
    /// Creates a new [`OpmlNode`] with the given title and kind.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: OpmlItemKind) -> Self {
        Self {
            name: name.into(),
            kind,
            ..Self::default()
        }
    }
}

/// Controls whether `format_track_list` prefixes titles with track numbers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum TrackNumbering {
    /// Render track titles without a numeric prefix (default).
    #[default]
    Hidden,
    /// Prefix track titles with `<track_number>. `.
    TrackNumber,
}

/// Presentation options passed to [`format_track_list`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackListOptions {
    /// Track number prefix display mode.
    pub numbering: TrackNumbering,
    /// Optional fallback image used when a track and its album lack artwork.
    pub fallback_image: Option<String>,
    /// Localized `"by"` conjunction placed between title and artists.
    pub by_label: String,
    /// Localized `"from"` conjunction placed between artists and album.
    pub from_label: String,
}

impl Default for TrackListOptions {
    fn default() -> Self {
        Self {
            numbering: TrackNumbering::Hidden,
            fallback_image: None,
            by_label: String::from("by"),
            from_label: String::from("from"),
        }
    }
}

impl TrackListOptions {
    /// Creates a [`TrackListOptions`] with explicit track numbering mode.
    #[must_use]
    pub fn with_numbering(mut self, numbering: TrackNumbering) -> Self {
        self.numbering = numbering;
        self
    }

    /// Sets a fallback artwork URL on this [`TrackListOptions`].
    #[must_use]
    pub fn with_fallback_image(mut self, image: impl Into<String>) -> Self {
        self.fallback_image = Some(image.into());
        self
    }
}

/// Formats normalized tracks into OPML nodes (`OPML.pm:trackList`).
///
/// Filters non-playable tracks (`!track.is_playable`), drops explicit tracks
/// under [`ExplicitFilterMode::HideExplicit`], marks explicit tracks as
/// non-playable `* ` text items under [`ExplicitFilterMode::StrictMaster`], and
/// maps `spotify:track:<id>` or `spotify:episode:<id>` to `spotify://<uri>`.
#[must_use]
pub fn format_track_list(
    tracks: &[NormalizedTrack],
    explicit_filter: ExplicitFilterMode,
    options: &TrackListOptions,
) -> Vec<OpmlNode> {
    let mut nodes = Vec::with_capacity(tracks.len());

    for track in tracks {
        if !track.is_playable {
            continue;
        }
        if track.explicit && explicit_filter == ExplicitFilterMode::HideExplicit {
            continue;
        }
        let Some(playable_url) = to_spotify_protocol_uri(&track.uri, &["track:", "episode:"])
        else {
            continue;
        };

        let title = match options.numbering {
            TrackNumbering::TrackNumber => format!("{}. {}", track.track_number, track.name),
            TrackNumbering::Hidden => track.name.clone(),
        };
        let artists = join_artist_names(&track.artists);
        let album_name = &track.album.name;
        let display_name = format!(
            "{title} {} {artists} {} {album_name}",
            options.by_label, options.from_label
        );
        let line2 = format!("{artists} \u{2022} {album_name}");
        let image = first_non_empty(&[
            track.image.as_str(),
            track.album.image.as_str(),
            options.fallback_image.as_deref().unwrap_or(""),
            IMG_TRACK,
        ]);

        if track.explicit && explicit_filter == ExplicitFilterMode::StrictMaster {
            let mut node = OpmlNode::new(format!("* {display_name}"), OpmlItemKind::Text);
            node.line1 = Some(format!("* {title}"));
            node.line2 = Some(line2);
            node.image = Some(image);
            nodes.push(node);
            continue;
        }

        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("uri"), track.uri.clone());

        let mut node = OpmlNode::new(display_name, OpmlItemKind::Audio);
        node.line1 = Some(title);
        node.line2 = Some(line2);
        node.url = Some(playable_url);
        node.image = Some(image);
        node.playall = true;
        node.passthrough = passthrough;
        nodes.push(node);
    }

    nodes
}

/// Formats albums into an outline [`OpmlNode`] with alphabetical index buckets.
///
/// Mirrors `OPML.pm:albumList`, optionally sorting case-insensitively with
/// leading articles stripped via `spotty_api::strip_leading_articles`.
#[must_use]
pub fn format_album_list(
    albums: &[NormalizedAlbum],
    sort_order: SortOrder,
    articles: &[&str],
    by_label: &str,
) -> OpmlNode {
    let mut ordered: Vec<(&NormalizedAlbum, String)> = Vec::with_capacity(albums.len());
    for album in albums {
        let stripped = strip_leading_articles(&album.name, articles);
        ordered.push((album, stripped));
    }

    if sort_order == SortOrder::Alphabetical {
        ordered.sort_by(|(lhs, lhs_key), (rhs, rhs_key)| {
            lhs_key
                .to_lowercase()
                .cmp(&rhs_key.to_lowercase())
                .then_with(|| lhs.name.to_lowercase().cmp(&rhs.name.to_lowercase()))
        });
    }

    let mut items = Vec::with_capacity(ordered.len());
    let mut bucket_keys = Vec::with_capacity(ordered.len());

    for (album, stripped) in ordered {
        let key = leading_bucket_label(&stripped);
        bucket_keys.push(key);

        let artists = if album.artists.is_empty() {
            album.artist.clone()
        } else {
            join_artist_names(&album.artists)
        };
        let display_name = if artists.is_empty() {
            album.name.clone()
        } else {
            format!("{} {by_label} {artists}", album.name)
        };
        let image = first_non_empty(&[album.image.as_str(), IMG_ALBUM]);

        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("uri"), album.uri.clone());

        let mut node = OpmlNode::new(display_name, OpmlItemKind::Playlist);
        node.line1 = Some(album.name.clone());
        node.line2 = (!artists.is_empty()).then_some(artists);
        node.url = Some(album.uri.clone());
        node.image = Some(image);
        node.passthrough = passthrough;
        items.push(node);
    }

    let mut container = OpmlNode::new(String::new(), OpmlItemKind::Outline);
    container.items = items;
    container.index_list = compute_index_buckets(&bucket_keys);
    container
}

/// Formats artists into an outline [`OpmlNode`] with index buckets.
///
/// Mirrors `OPML.pm:artistList`, computing bucket labels with leading articles
/// stripped and annotating `myAlbumsOnly` in each item's `passthrough`.
#[must_use]
pub fn format_artist_list(
    artists: &[NormalizedArtist],
    sort_order: SortOrder,
    album_scope: AlbumScope,
    articles: &[&str],
    followers_label: &str,
) -> OpmlNode {
    let mut ordered: Vec<(&NormalizedArtist, String)> = Vec::with_capacity(artists.len());
    for artist in artists {
        let base = if artist.sortname.is_empty() {
            artist.name.as_str()
        } else {
            artist.sortname.as_str()
        };
        let stripped = strip_leading_articles(base, articles);
        ordered.push((artist, stripped));
    }

    if sort_order == SortOrder::Alphabetical {
        ordered.sort_by(|(lhs, lhs_key), (rhs, rhs_key)| {
            lhs_key
                .to_lowercase()
                .cmp(&rhs_key.to_lowercase())
                .then_with(|| lhs.name.to_lowercase().cmp(&rhs.name.to_lowercase()))
        });
    }

    let my_albums_only = match album_scope {
        AlbumScope::MyAlbumsOnly => "1",
        AlbumScope::FullArtistCatalog | _ => "0",
    };

    let mut items = Vec::with_capacity(ordered.len());
    let mut bucket_keys = Vec::with_capacity(ordered.len());

    for (artist, stripped) in ordered {
        let key = leading_bucket_label(&stripped);
        bucket_keys.push(key);

        let image = first_non_empty(&[artist.image.as_str(), IMG_ARTIST]);
        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("uri"), artist.uri.clone());
        passthrough.insert(String::from("myAlbumsOnly"), String::from(my_albums_only));

        let mut node = OpmlNode::new(artist.name.clone(), OpmlItemKind::Link);
        node.line1 = Some(artist.name.clone());
        if artist.followers_total > 0 {
            node.line2 = Some(format!("{followers_label} {}", artist.followers_total));
        }
        node.url = Some(artist.uri.clone());
        node.image = Some(image);
        node.passthrough = passthrough;
        items.push(node);
    }

    let mut container = OpmlNode::new(String::new(), OpmlItemKind::Outline);
    container.items = items;
    container.index_list = compute_index_buckets(&bucket_keys);
    container
}

/// Formats podcast shows into an outline [`OpmlNode`] with index buckets.
///
/// Mirrors `OPML.pm:podcastList`.
#[must_use]
pub fn format_podcast_list(shows: &[NormalizedShow]) -> OpmlNode {
    let mut items = Vec::with_capacity(shows.len());
    let mut bucket_keys = Vec::with_capacity(shows.len());

    for show in shows {
        if !show.media_type.is_empty() && show.media_type != "audio" {
            tracing::warn!(uri = %show.uri, media_type = %show.media_type, "non-audio show");
        }

        bucket_keys.push(leading_bucket_label(&show.name));
        let image = first_non_empty(&[show.image.as_str(), IMG_PODCAST]);

        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("uri"), show.uri.clone());
        passthrough.insert(String::from("id"), show.id.clone());
        passthrough.insert(String::from("name"), show.name.clone());
        passthrough.insert(String::from("description"), show.description.clone());

        let mut node = OpmlNode::new(show.name.clone(), OpmlItemKind::Playlist);
        node.line1 = Some(show.name.clone());
        node.line2 = (!show.description.is_empty()).then(|| show.description.clone());
        node.url = Some(show.uri.clone());
        node.image = Some(image);
        node.passthrough = passthrough;
        items.push(node);
    }

    let mut container = OpmlNode::new(String::new(), OpmlItemKind::Outline);
    container.items = items;
    container.index_list = compute_index_buckets(&bucket_keys);
    container
}

/// Formats podcast episodes into playable or restricted OPML nodes.
///
/// Applies [`PodcastEpisodeOrder`] (`OldestFirst` keeps input order;
/// `ReverseNewestFirst` reverses iteration matching `reversePodcastOrder`),
/// filters non-playable episodes, enforces [`ExplicitFilterMode`], and maps
/// `spotify:episode:<id>` to `spotify://episode:<id>`.
#[must_use]
pub fn format_episodes_list(
    episodes: &[NormalizedEpisode],
    explicit_filter: ExplicitFilterMode,
    order: PodcastEpisodeOrder,
    fallback_image: Option<&str>,
) -> Vec<OpmlNode> {
    let mut items = Vec::with_capacity(episodes.len());
    let indices: Vec<usize> = match order {
        PodcastEpisodeOrder::ReverseNewestFirst => (0..episodes.len()).rev().collect(),
        PodcastEpisodeOrder::OldestFirst | _ => (0..episodes.len()).collect(),
    };

    for idx in indices {
        let episode = &episodes[idx];
        if !episode.is_playable {
            continue;
        }
        if episode.explicit && explicit_filter == ExplicitFilterMode::HideExplicit {
            continue;
        }
        let Some(playable_url) = to_spotify_protocol_uri(&episode.uri, &["episode:"]) else {
            continue;
        };

        let title = if episode.release_date.is_empty() {
            episode.name.clone()
        } else {
            format!("{} - {}", episode.release_date, episode.name)
        };
        let description: String = episode
            .description
            .chars()
            .take(MAX_EPISODE_DESC_CHARS)
            .collect();
        let image = first_non_empty(&[
            episode.image.as_str(),
            fallback_image.unwrap_or(""),
            IMG_TRACK,
        ]);

        if episode.explicit && explicit_filter == ExplicitFilterMode::StrictMaster {
            let mut node = OpmlNode::new(format!("* {title}"), OpmlItemKind::Text);
            node.line1 = Some(format!("* {title}"));
            node.line2 = (!description.is_empty()).then_some(description);
            node.image = Some(image);
            items.push(node);
            continue;
        }

        let mut passthrough = BTreeMap::new();
        passthrough.insert(String::from("uri"), episode.uri.clone());

        let mut node = OpmlNode::new(title.clone(), OpmlItemKind::Audio);
        node.line1 = Some(title);
        node.line2 = (!description.is_empty()).then_some(description);
        node.url = Some(playable_url);
        node.image = Some(image);
        node.passthrough = passthrough;
        items.push(node);
    }

    items
}

/// Formats user playlists, optionally grouping them into folder outline nodes.
///
/// When `hierarchy` is `Some` and non-empty, builds the pruned folder tree via
/// [`spotty_playlist_folders::build_folder_tree`] and renders each folder as an
/// [`OpmlItemKind::Outline`] node using [`IMG_PLAYLIST`].
#[must_use]
pub fn format_playlist_list(
    playlists: &[NormalizedPlaylist],
    current_user_id: &str,
    hierarchy: Option<&HashMap<String, FolderEntry>>,
    by_label: &str,
) -> Vec<OpmlNode> {
    if let Some(folder_map) = hierarchy {
        if !folder_map.is_empty() {
            let tree = build_folder_tree(playlists, folder_map, |playlist| playlist.uri.as_str());
            return convert_folder_tree(&tree, current_user_id, by_label);
        }
    }

    let mut nodes = Vec::with_capacity(playlists.len());
    for playlist in playlists {
        nodes.push(format_single_playlist(playlist, current_user_id, by_label));
    }
    nodes
}

fn convert_folder_tree(
    tree: &[FolderTreeItem<NormalizedPlaylist>],
    current_user_id: &str,
    by_label: &str,
) -> Vec<OpmlNode> {
    let mut nodes = Vec::with_capacity(tree.len());
    for entry in tree {
        match entry {
            FolderTreeItem::Folder {
                id, name, children, ..
            } => {
                let mut passthrough = BTreeMap::new();
                passthrough.insert(String::from("folder_id"), id.clone());

                let mut folder_node = OpmlNode::new(name.clone(), OpmlItemKind::Outline);
                folder_node.image = Some(String::from(IMG_PLAYLIST));
                folder_node.passthrough = passthrough;
                folder_node.items = convert_folder_tree(children, current_user_id, by_label);
                nodes.push(folder_node);
            }
            FolderTreeItem::Leaf { item, .. } => {
                nodes.push(format_single_playlist(item, current_user_id, by_label));
            }
            _ => {}
        }
    }
    nodes
}

fn format_single_playlist(
    playlist: &NormalizedPlaylist,
    current_user_id: &str,
    by_label: &str,
) -> OpmlNode {
    let default_img = if playlist.collaborative {
        IMG_COLLABORATIVE
    } else {
        IMG_PLAYLIST
    };
    let image = first_non_empty(&[playlist.image.as_str(), default_img]);

    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("uri"), playlist.uri.clone());

    let mut node = OpmlNode::new(playlist.name.clone(), OpmlItemKind::Playlist);
    node.line1 = Some(playlist.name.clone());
    if !playlist.creator.is_empty() && playlist.creator != current_user_id {
        node.line2 = Some(format!("{by_label} {}", playlist.creator));
    }
    node.url = Some(playlist.uri.clone());
    node.image = Some(image);
    node.passthrough = passthrough;
    node
}

fn to_spotify_protocol_uri(raw_uri: &str, allowed_prefixes: &[&str]) -> Option<String> {
    let suffix = raw_uri.strip_prefix("spotify:")?;
    for prefix in allowed_prefixes {
        if let Some(rest) = suffix.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some(format!("spotify://{suffix}"));
            }
        }
    }
    None
}

fn join_artist_names(artists: &[spotty_api::ArtistSummary]) -> String {
    let mut joined = String::new();
    for artist in artists {
        if artist.name.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push_str(", ");
        }
        joined.push_str(&artist.name);
    }
    joined
}

fn first_non_empty(candidates: &[&str]) -> String {
    for candidate in candidates {
        if !candidate.is_empty() {
            return String::from(*candidate);
        }
    }
    String::new()
}

fn leading_bucket_label(text: &str) -> String {
    let Some(first) = text.trim_start().chars().next() else {
        return String::new();
    };
    first.to_uppercase().to_string()
}

fn compute_index_buckets(keys: &[String]) -> Vec<OpmlIndexBucket> {
    let mut buckets = Vec::new();
    let mut current_label: Option<&str> = None;
    let mut count: usize = 0;

    for key in keys {
        match current_label {
            Some(label) if label == key.as_str() => {
                count += 1;
            }
            Some(label) => {
                buckets.push(OpmlIndexBucket::new(label, count));
                current_label = Some(key.as_str());
                count = 1;
            }
            None => {
                current_label = Some(key.as_str());
                count = 1;
            }
        }
    }

    if let Some(label) = current_label {
        buckets.push(OpmlIndexBucket::new(label, count));
    }

    buckets
}

#[cfg(test)]
mod tests {
    use spotty_api::ArtistSummary;
    use spotty_api::MinAlbum;
    use spotty_playlist_folders::parse_folder_blob;

    use super::*;

    fn sample_track(id: &str, name: &str, explicit: bool, is_playable: bool) -> NormalizedTrack {
        let mut track = NormalizedTrack::default();
        track.id = String::from(id);
        track.uri = format!("spotify:track:{id}");
        track.name = String::from(name);
        track.artists = vec![ArtistSummary::new("a1", "spotify:artist:a1", "Artist One")];
        track.album = MinAlbum::new(
            "al1",
            "spotify:album:al1",
            "Album One",
            Vec::new(),
            "https://img/album.jpg",
            "2024",
            "album",
            None,
        );
        track.duration_ms = 180_000;
        track.track_number = 3;
        track.disc_number = 1;
        track.explicit = explicit;
        track.is_playable = is_playable;
        track
    }

    fn sample_album(id: &str, name: &str, artist: &str) -> NormalizedAlbum {
        let mut album = NormalizedAlbum::default();
        album.id = String::from(id);
        album.uri = format!("spotify:album:{id}");
        album.name = String::from(name);
        album.artist = String::from(artist);
        album
    }

    fn sample_artist(
        id: &str,
        name: &str,
        sortname: &str,
        followers_total: u64,
    ) -> NormalizedArtist {
        let mut artist = NormalizedArtist::default();
        artist.id = String::from(id);
        artist.uri = format!("spotify:artist:{id}");
        artist.name = String::from(name);
        artist.sortname = String::from(sortname);
        artist.followers_total = followers_total;
        artist
    }

    fn sample_episode(
        id: &str,
        name: &str,
        release_date: &str,
        explicit: bool,
    ) -> NormalizedEpisode {
        let mut episode = NormalizedEpisode::default();
        episode.id = String::from(id);
        episode.uri = format!("spotify:episode:{id}");
        episode.name = String::from(name);
        episode.release_date = String::from(release_date);
        episode.is_playable = true;
        episode.explicit = explicit;
        episode
    }

    fn sample_playlist(
        id: &str,
        name: &str,
        creator: &str,
        collaborative: bool,
    ) -> NormalizedPlaylist {
        let mut playlist = NormalizedPlaylist::default();
        playlist.id = String::from(id);
        playlist.uri = format!("spotify:playlist:{id}");
        playlist.name = String::from(name);
        playlist.creator = String::from(creator);
        playlist.collaborative = collaborative;
        playlist
    }

    #[test]
    fn format_track_list_filters_unplayable_and_applies_explicit_modes() {
        let mut episode_track = sample_track("ep99", "Podcast Ep", false, true);
        episode_track.uri = String::from("spotify:episode:ep99");

        let tracks = vec![
            sample_track("t1", "Clean Song", false, true),
            sample_track("t2", "Explicit Song", true, true),
            sample_track("t3", "Unplayable Song", false, false),
            episode_track,
        ];

        let opts = TrackListOptions::default().with_numbering(TrackNumbering::TrackNumber);

        let allow = format_track_list(&tracks, ExplicitFilterMode::AllowAll, &opts);
        assert_eq!(allow.len(), 3, "AllowAll should skip only unplayable");
        assert_eq!(allow[0].url.as_deref(), Some("spotify://track:t1"));
        assert_eq!(allow[0].line1.as_deref(), Some("3. Clean Song"));
        assert_eq!(allow[1].kind, OpmlItemKind::Audio);
        assert_eq!(allow[2].url.as_deref(), Some("spotify://episode:ep99"));
        assert!(allow[0].playall);

        let hide = format_track_list(&tracks, ExplicitFilterMode::HideExplicit, &opts);
        assert_eq!(hide.len(), 2, "HideExplicit should omit explicit tracks");
        assert_eq!(hide[0].url.as_deref(), Some("spotify://track:t1"));
        assert_eq!(hide[1].url.as_deref(), Some("spotify://episode:ep99"));

        let strict = format_track_list(&tracks, ExplicitFilterMode::StrictMaster, &opts);
        assert_eq!(strict.len(), 3, "StrictMaster keeps explicit as Text");
        assert_eq!(strict[1].kind, OpmlItemKind::Text);
        assert!(strict[1].name.starts_with("* "), "got {}", strict[1].name);
        assert_eq!(strict[1].line1.as_deref(), Some("* 3. Explicit Song"));
        assert_eq!(strict[1].url, None);
        assert!(!strict[1].playall);
    }

    #[test]
    fn format_album_and_artist_lists_compute_article_stripped_buckets() {
        let albums = vec![
            sample_album("1", "The Bends", "Radiohead"),
            sample_album("2", "Abbey Road", "The Beatles"),
            sample_album("3", "Achtung Baby", "U2"),
            sample_album("4", "Blue", "Joni Mitchell"),
        ];

        let out = format_album_list(
            &albums,
            SortOrder::Alphabetical,
            DEFAULT_SORT_ARTICLES,
            "by",
        );
        assert_eq!(out.items.len(), 4);
        assert_eq!(out.items[0].line1.as_deref(), Some("Abbey Road"));
        assert_eq!(out.items[1].line1.as_deref(), Some("Achtung Baby"));
        assert_eq!(out.items[2].line1.as_deref(), Some("The Bends"));
        assert_eq!(out.items[3].line1.as_deref(), Some("Blue"));
        assert_eq!(
            out.index_list,
            vec![OpmlIndexBucket::new("A", 2), OpmlIndexBucket::new("B", 2)]
        );

        let artists = vec![
            sample_artist("ar1", "The Cure", "Cure", 1200),
            sample_artist("ar2", "Autechre", "", 0),
        ];

        let artist_out = format_artist_list(
            &artists,
            SortOrder::Alphabetical,
            AlbumScope::MyAlbumsOnly,
            DEFAULT_SORT_ARTICLES,
            "Followers:",
        );
        assert_eq!(artist_out.items[0].name, "Autechre");
        assert_eq!(artist_out.items[1].name, "The Cure");
        assert_eq!(
            artist_out.items[1].line2.as_deref(),
            Some("Followers: 1200")
        );
        assert_eq!(
            artist_out.items[1]
                .passthrough
                .get("myAlbumsOnly")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            artist_out.index_list,
            vec![OpmlIndexBucket::new("A", 1), OpmlIndexBucket::new("C", 1)]
        );
    }

    #[test]
    fn format_podcast_and_episodes_lists_respect_order_and_explicit_filter() {
        let mut show = NormalizedShow::default();
        show.id = String::from("s1");
        show.uri = String::from("spotify:show:s1");
        show.name = String::from("Rust Talk");
        show.description = String::from("Systems podcast");
        show.media_type = String::from("audio");

        let pod = format_podcast_list(&[show]);
        assert_eq!(pod.items.len(), 1);
        assert_eq!(pod.items[0].image.as_deref(), Some(IMG_PODCAST));
        assert_eq!(pod.index_list, vec![OpmlIndexBucket::new("R", 1)]);

        let episodes = vec![
            sample_episode("e1", "Episode 1", "2025-01-01", false),
            sample_episode("e2", "Episode 2", "2025-01-08", true),
        ];

        let oldest = format_episodes_list(
            &episodes,
            ExplicitFilterMode::AllowAll,
            PodcastEpisodeOrder::OldestFirst,
            None,
        );
        assert_eq!(oldest[0].url.as_deref(), Some("spotify://episode:e1"));
        assert_eq!(oldest[1].url.as_deref(), Some("spotify://episode:e2"));

        let newest = format_episodes_list(
            &episodes,
            ExplicitFilterMode::HideExplicit,
            PodcastEpisodeOrder::ReverseNewestFirst,
            None,
        );
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].url.as_deref(), Some("spotify://episode:e1"));
    }

    #[test]
    fn format_playlist_list_renders_folder_outline_tree() {
        let blob = b"spotify:start-group:f_rock:Rock+Mixes\r\
            spotify:playlist:1111111111111111111111\r\
            spotify:end-group:f_rock\r";
        let hierarchy = parse_folder_blob(blob).expect("fixture blob should parse");

        let playlists = vec![
            sample_playlist("1111111111111111111111", "Classic Rock", "alice", false),
            sample_playlist("2222222222222222222222", "Loose Playlist", "me", true),
        ];

        let nodes = format_playlist_list(&playlists, "me", Some(&hierarchy), "by");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].kind, OpmlItemKind::Outline);
        assert_eq!(nodes[0].name, "Rock Mixes");
        assert_eq!(nodes[0].image.as_deref(), Some(IMG_PLAYLIST));
        assert_eq!(nodes[0].items.len(), 1);
        assert_eq!(nodes[0].items[0].name, "Classic Rock");
        assert_eq!(nodes[0].items[0].line2.as_deref(), Some("by alice"));

        assert_eq!(nodes[1].kind, OpmlItemKind::Playlist);
        assert_eq!(nodes[1].name, "Loose Playlist");
        assert_eq!(nodes[1].line2, None);
        assert_eq!(nodes[1].image.as_deref(), Some(IMG_COLLABORATIVE));
    }
}
