//! Native `SQLite` online-library importer targeting LMS `library.db`.

pub mod needs_update;
pub mod runner;
pub mod scanners;
pub mod schema_sink;
pub mod track_prep;

pub use crate::needs_update::online_library_needs_update;
pub use crate::runner::{
    artist_picture_url, ImporterRunner, PlaylistSyncPolicy, ScanScope, ScanSummary,
};
pub use crate::scanners::{
    scan_enabled_albums, scan_enabled_albums_at, scan_enabled_artists, scan_enabled_artists_at,
    scan_enabled_playlists, SPOTIFY_EDITORIAL_REFRESH_SECS, WATERMARK_TTL_SECS,
};
pub use crate::schema_sink::{LibrarySink, SqliteLibrarySink};
pub use crate::track_prep::{prepare_lms_track, LmsReleaseType, LmsTrackRow};
