//! Core domain types, error definitions, and LMS host traits for `Spotty-RS`.

pub mod error;
pub mod host;
pub mod manifest;
pub mod prefs;
pub mod uri;

pub use crate::error::{Result, SpottyError};
pub use crate::host::{ClientId, LmsHostCallbacks, PlaylistCommand};
pub use crate::manifest::{IconCode, PluginManifest, DEFAULT_ICON_HASH};
pub use crate::prefs::{
    AccountSwitcherVisibility, AlbumScope, ApFallbackPolicy, Bitrate, ExplicitFilterMode,
    GlobalPrefs, PlayerPrefs, PodcastEpisodeOrder, ReplayGainPolicy, SortOrder, TagCleanupPolicy,
};
pub use crate::uri::{SpotifyEntityKind, SpotifyUri};
