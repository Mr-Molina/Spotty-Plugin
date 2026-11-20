//! OPML feed router, menus, formatters, and search history for `Spotty-RS`.

pub mod formatters;
pub mod home;
pub mod search_lru;

pub use crate::formatters::{
    format_album_list, format_artist_list, format_episodes_list, format_playlist_list,
    format_podcast_list, format_track_list, OpmlIndexBucket, OpmlItemKind, OpmlNode,
    TrackListOptions, TrackNumbering, DEFAULT_SORT_ARTICLES,
};
pub use crate::home::{
    sort_home_items, top_chart_uri, IMG_ACCOUNT, IMG_ALBUM, IMG_ARTIST, IMG_COLLABORATIVE,
    IMG_COMPILATION, IMG_FOLLOW_ARTIST, IMG_GENRES, IMG_HOME, IMG_INBOX, IMG_NEWS, IMG_PLAYLIST,
    IMG_PODCAST, IMG_RADIO, IMG_SEARCH, IMG_SONG, IMG_SONGS, IMG_TOPTRACKS, IMG_TOP_TRACKS,
    IMG_TRACK, IMG_TRANSFER, IMG_WHATS_NEW,
};
pub use crate::search_lru::{
    RecentSearchCliItem, RecentSearchCliQuery, RecentSearchCliResponse, RecentSearchesStore,
    MAX_RECENT_SEARCHES,
};
