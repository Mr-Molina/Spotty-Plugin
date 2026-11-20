//! Spotify Web API v1 client, token manager, and pagination pipeline.

pub mod models;
pub mod normalize;

pub use crate::models::{
    ArtistSummary, BrowseCategory, LinkedFrom, MinAlbum, NormalizedAlbum, NormalizedArtist,
    NormalizedEpisode, NormalizedPlaylist, NormalizedShow, NormalizedTrack, PlayerContextState,
    PlayerDevice, SearchResults, UserProfile,
};
pub use crate::normalize::{
    normalize_album, normalize_artist, normalize_browse_category, normalize_episode,
    normalize_playlist, normalize_show, normalize_track, normalize_user_profile, strip_html_tags,
    strip_leading_articles, EntityNormalizer,
};
