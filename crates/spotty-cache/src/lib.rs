//! Persistent two-tier cache and account storage for `Spotty-RS`.

pub mod account_store;
pub mod backend;
pub mod gc;
pub mod merge;
pub mod metadata_cache;

pub use crate::account_store::{
    AccountId, AccountStore, CredentialsRecord, PurgeScope, AUTHENTICATE_DIR,
};
pub use crate::backend::{CachedHttpEntry, RedbStore};
pub use crate::gc::{TempAudioGc, TrackCounter, CACHE_PURGE_INTERVAL_COUNT, DEFAULT_TEMP_MAX_AGE};
pub use crate::merge::{cleanup_tags, largest_artwork_url, merge_no_duplicates, ArtworkImage};
pub use crate::metadata_cache::{
    DualCache, MetadataWriteMode, EPISODE_ARTIST_TTL_SECS, METADATA_TTL_SECS,
    PLAYLIST_OWNER_TTL_SECS,
};
