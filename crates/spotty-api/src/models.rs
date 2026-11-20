//! Normalized domain models for Spotify Web API v1 responses.

use serde::{Deserialize, Serialize};

/// Minimal artist reference embedded in tracks, albums, and shows.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtistSummary {
    /// Spotify artist identifier.
    pub id: String,
    /// Canonical `spotify:artist:<id>` URI.
    pub uri: String,
    /// Display name of the artist or podcast publisher.
    pub name: String,
}

impl ArtistSummary {
    /// Creates a new [`ArtistSummary`].
    #[must_use]
    pub fn new(id: impl Into<String>, uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            uri: uri.into(),
            name: name.into(),
        }
    }
}

/// Normalized Spotify artist metadata with article-stripped sort name.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedArtist {
    /// Spotify artist identifier.
    pub id: String,
    /// Canonical `spotify:artist:<id>` URI.
    pub uri: String,
    /// Display name of the artist.
    pub name: String,
    /// Article-stripped sort name for alphabetical artist sorting.
    pub sortname: String,
    /// Largest available artwork URL, or empty when unavailable.
    pub image: String,
    /// Total follower count reported by Spotify.
    pub followers_total: u64,
    /// Associated genre labels.
    pub genres: Vec<String>,
}

/// Compact album descriptor injected onto child tracks (`minAlbum`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MinAlbum {
    /// Spotify album identifier.
    pub id: String,
    /// Canonical `spotify:album:<id>` URI.
    pub uri: String,
    /// Album title.
    pub name: String,
    /// Album artists.
    pub artists: Vec<ArtistSummary>,
    /// Largest available album artwork URL, or empty when unavailable.
    pub image: String,
    /// Album release date string (`YYYY`, `YYYY-MM`, or `YYYY-MM-DD`).
    pub release_date: String,
    /// Album classification (`album`, `single`, or `compilation`).
    pub album_type: String,
    /// ISO-8601 timestamp when saved to the user's library, if present.
    pub added_at: Option<String>,
}

impl MinAlbum {
    /// Creates a new [`MinAlbum`].
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        uri: impl Into<String>,
        name: impl Into<String>,
        artists: Vec<ArtistSummary>,
        image: impl Into<String>,
        release_date: impl Into<String>,
        album_type: impl Into<String>,
        added_at: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            uri: uri.into(),
            name: name.into(),
            artists,
            image: image.into(),
            release_date: release_date.into(),
            album_type: album_type.into(),
            added_at,
        }
    }
}

/// Alternate linked track identifier used for market relinking (`linked_from`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkedFrom {
    /// Original requested Spotify track identifier.
    pub id: String,
    /// Original requested `spotify:track:<id>` URI.
    pub uri: String,
}

impl LinkedFrom {
    /// Creates a new [`LinkedFrom`] reference.
    #[must_use]
    pub fn new(id: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            uri: uri.into(),
        }
    }
}

/// Normalized Spotify track entity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedTrack {
    /// Spotify track identifier.
    pub id: String,
    /// Canonical `spotify:track:<id>` URI.
    pub uri: String,
    /// Track title.
    pub name: String,
    /// Performing artists.
    pub artists: Vec<ArtistSummary>,
    /// Compact album metadata injected or normalized from the track payload.
    pub album: MinAlbum,
    /// Track duration in milliseconds.
    pub duration_ms: u64,
    /// One-based track number within the disc.
    pub track_number: u32,
    /// One-based disc number.
    pub disc_number: u32,
    /// Whether the track is flagged as explicit.
    pub explicit: bool,
    /// Whether the track is playable in the user's market.
    pub is_playable: bool,
    /// Track artwork URL, falling back to `album.image`.
    pub image: String,
    /// Original track identifier when Spotify relinked the track.
    pub linked_from: Option<LinkedFrom>,
    /// ISO-8601 timestamp when added to a library or playlist, if present.
    pub added_at: Option<String>,
}

/// Normalized Spotify album entity with injected `minAlbum` child tracks.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedAlbum {
    /// Spotify album identifier.
    pub id: String,
    /// Canonical `spotify:album:<id>` URI.
    pub uri: String,
    /// Album title.
    pub name: String,
    /// Primary artist display name (`artists[0].name`).
    pub artist: String,
    /// Album artists.
    pub artists: Vec<ArtistSummary>,
    /// Largest available album artwork URL, or empty when unavailable.
    pub image: String,
    /// Album release date string.
    pub release_date: String,
    /// Album classification (`album`, `single`, or `compilation`).
    pub album_type: String,
    /// Total number of tracks on the album.
    pub total_tracks: u32,
    /// Normalized album tracks with `minAlbum` injected.
    pub tracks: Vec<NormalizedTrack>,
    /// ISO-8601 timestamp when saved to the user's library, if present.
    pub added_at: Option<String>,
}

impl NormalizedAlbum {
    /// Builds a [`MinAlbum`] projection from this album.
    #[must_use]
    pub fn to_min_album(&self) -> MinAlbum {
        MinAlbum {
            id: self.id.clone(),
            uri: self.uri.clone(),
            name: self.name.clone(),
            artists: self.artists.clone(),
            image: self.image.clone(),
            release_date: self.release_date.clone(),
            album_type: self.album_type.clone(),
            added_at: self.added_at.clone(),
        }
    }
}

/// Normalized Spotify playlist summary.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedPlaylist {
    /// Spotify playlist identifier.
    pub id: String,
    /// Canonical `spotify:playlist:<id>` URI.
    pub uri: String,
    /// Playlist title.
    pub name: String,
    /// Playlist owner user ID (`owner.id`).
    pub creator: String,
    /// Playlist owner display name, if provided by Spotify.
    pub owner_display_name: Option<String>,
    /// Plain-text playlist description with HTML tags stripped.
    pub description: String,
    /// Largest available playlist cover URL, or empty when unavailable.
    pub image: String,
    /// Playlist snapshot version identifier.
    pub snapshot_id: String,
    /// Whether the playlist is collaborative.
    pub collaborative: bool,
    /// Total number of tracks reported for the playlist.
    pub tracks_total: u32,
}

/// Normalized Spotify podcast episode entity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedEpisode {
    /// Spotify episode identifier.
    pub id: String,
    /// Canonical `spotify:episode:<id>` URI.
    pub uri: String,
    /// Episode title.
    pub name: String,
    /// Plain-text episode description with HTML tags stripped.
    pub description: String,
    /// Episode duration in milliseconds.
    pub duration_ms: u64,
    /// Episode release date string.
    pub release_date: String,
    /// Whether the episode is flagged as explicit.
    pub explicit: bool,
    /// Whether the episode is playable.
    pub is_playable: bool,
    /// Largest available episode or show artwork URL.
    pub image: String,
    /// Parent show title.
    pub show_name: String,
    /// Parent show `spotify:show:<id>` URI.
    pub show_uri: String,
    /// Podcast publisher name.
    pub publisher: String,
    /// Saved playback resume position in milliseconds, if available.
    pub resume_position_ms: Option<u64>,
}

impl NormalizedEpisode {
    /// Projects this episode into a [`NormalizedTrack`]-compatible view.
    ///
    /// Mirrors `API/Cache.pm` mapping of show name, show URI, publisher, and
    /// artwork into `album` and `artists` fields for unified playback lookup.
    #[must_use]
    pub fn to_track_view(&self) -> NormalizedTrack {
        let artists = if self.publisher.is_empty() {
            Vec::new()
        } else {
            vec![ArtistSummary::new("", "", self.publisher.clone())]
        };

        let album = MinAlbum {
            id: String::new(),
            uri: self.show_uri.clone(),
            name: self.show_name.clone(),
            artists: artists.clone(),
            image: self.image.clone(),
            release_date: self.release_date.clone(),
            album_type: String::from("show"),
            added_at: None,
        };

        NormalizedTrack {
            id: self.id.clone(),
            uri: self.uri.clone(),
            name: self.name.clone(),
            artists,
            album,
            duration_ms: self.duration_ms,
            track_number: 1,
            disc_number: 1,
            explicit: self.explicit,
            is_playable: self.is_playable,
            image: self.image.clone(),
            linked_from: None,
            added_at: None,
        }
    }
}

/// Normalized Spotify podcast show entity.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NormalizedShow {
    /// Spotify show identifier.
    pub id: String,
    /// Canonical `spotify:show:<id>` URI.
    pub uri: String,
    /// Show title.
    pub name: String,
    /// Show publisher name.
    pub publisher: String,
    /// Plain-text show description with HTML tags stripped.
    pub description: String,
    /// Largest available show artwork URL, or empty when unavailable.
    pub image: String,
    /// Total number of episodes in the show.
    pub total_episodes: u32,
    /// Show media type (`audio` or `mixed`).
    pub media_type: String,
    /// Normalized episodes belonging to the show.
    pub episodes: Vec<NormalizedEpisode>,
}

/// Spotify user profile returned by `/v1/me` or `/v1/users/{id}`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserProfile {
    /// Spotify user identifier.
    pub id: String,
    /// Canonical `spotify:user:<id>` URI.
    pub uri: String,
    /// User display name, if set.
    pub display_name: Option<String>,
    /// ISO 3166-1 alpha-2 country code, if present.
    pub country: Option<String>,
    /// Subscription tier (`premium`, `free`, etc.), if present.
    pub product: Option<String>,
    /// Account email address when `user-read-email` scope is granted.
    pub email: Option<String>,
    /// Largest available profile avatar URL, or empty when unavailable.
    pub image: String,
    /// Total follower count, if returned by the endpoint.
    pub followers_total: Option<u64>,
}

/// Active Spotify Connect playback device summary.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerDevice {
    /// Device identifier, if exposed by Spotify Connect.
    pub id: Option<String>,
    /// Human-readable device name.
    pub name: String,
    /// Device class (`Speaker`, `Computer`, `Smartphone`, etc.).
    pub device_type: String,
    /// Whether this device is currently active.
    pub is_active: bool,
    /// Current volume percentage (`0..=100`), if reported.
    pub volume_percent: Option<u32>,
}

/// Normalized Spotify Connect player state from `/v1/me/player`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerContextState {
    /// Whether playback is currently active.
    pub is_playing: bool,
    /// Current playback progress in milliseconds.
    pub progress_ms: Option<u64>,
    /// Server Unix timestamp in milliseconds when state was sampled.
    pub timestamp: Option<u64>,
    /// Whether shuffle mode is enabled.
    pub shuffle_state: bool,
    /// Repeat mode (`off`, `track`, or `context`).
    pub repeat_state: String,
    /// Active item kind (`track`, `episode`, `ad`, or `unknown`).
    pub currently_playing_type: String,
    /// Currently active Connect device, if any.
    pub device: Option<PlayerDevice>,
    /// Active playback context URI, if any.
    pub context_uri: Option<String>,
    /// Normalized current track when `currently_playing_type` is `track`.
    pub track: Option<NormalizedTrack>,
    /// Normalized current episode when `currently_playing_type` is `episode`.
    pub episode: Option<NormalizedEpisode>,
}

/// Spotify browse category descriptor from `/v1/browse/categories`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowseCategory {
    /// Spotify category identifier.
    pub id: String,
    /// Localized category display name.
    pub name: String,
    /// Largest available category icon URL, or empty when unavailable.
    pub image: String,
}

impl BrowseCategory {
    /// Creates a new [`BrowseCategory`].
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            image: image.into(),
        }
    }
}

/// Normalized multi-entity response from `/v1/search`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResults {
    /// Matching normalized tracks.
    pub tracks: Vec<NormalizedTrack>,
    /// Matching normalized albums.
    pub albums: Vec<NormalizedAlbum>,
    /// Matching normalized artists.
    pub artists: Vec<NormalizedArtist>,
    /// Matching normalized playlists.
    pub playlists: Vec<NormalizedPlaylist>,
    /// Matching normalized podcast shows.
    pub shows: Vec<NormalizedShow>,
    /// Matching normalized podcast episodes.
    pub episodes: Vec<NormalizedEpisode>,
    /// Total result count when searching a single entity type.
    pub total: Option<u32>,
    /// Next page URL when more search results exist.
    pub next: Option<String>,
}
