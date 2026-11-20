//! Spotify Web API v1 client, token manager, and pagination pipeline.

pub mod models;
pub mod normalize;
pub mod pipeline;
pub mod token;
pub mod transport;

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
pub use crate::pipeline::{
    extract_after_cursor, CursorPage, OffsetPage, Pipeline, DEFAULT_MAX_CONCURRENCY,
    DEFAULT_MAX_ITERATIONS,
};
pub use crate::token::{
    PkceChallenge, TokenBundle, TokenManager, DEFAULT_EXPIRATION_SECS, DEFAULT_TOKEN_URL,
    REFRESH_TOKEN_TTL_SECS, SPOTIFY_API_SCOPES, SPOTIFY_AUTH_SCOPES,
};
pub use crate::transport::{
    serialize_query, CachePolicy, RateLimitGate, RateLimitPolicy, SpotifyTransport,
    TransportResponse, DEFAULT_API_BASE_URL, DEFAULT_REQUEST_TIMEOUT_SECS,
};
