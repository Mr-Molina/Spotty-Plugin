//! Persistent two-tier cache and account storage for Spotty-RS.

pub mod account_store;
pub mod gc;

pub use crate::account_store::{
    AccountId, AccountStore, CredentialsRecord, PurgeScope, AUTHENTICATE_DIR,
};
pub use crate::gc::{TempAudioGc, TrackCounter, CACHE_PURGE_INTERVAL_COUNT, DEFAULT_TEMP_MAX_AGE};
