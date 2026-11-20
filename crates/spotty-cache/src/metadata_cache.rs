//! High-level two-tier cache combining ephemeral KV/HTTP and 90-day metadata.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use spotty_types::{Result, SpottyError};

use crate::backend::{current_epoch_secs, CachedHttpEntry, RedbStore};
use crate::merge::merge_no_duplicates;

/// 90-day TTL for normalized Spotify track, album, and linked metadata.
pub const METADATA_TTL_SECS: u64 = 86_400 * 90;

/// 7-day TTL for episode metadata and artist image fallback keys.
pub const EPISODE_ARTIST_TTL_SECS: u64 = 86_400 * 7;

/// 30-day TTL for playlist owner lookup entries.
pub const PLAYLIST_OWNER_TTL_SECS: u64 = 86_400 * 30;

/// Write mode controlling how `store_metadata` handles existing cache entries.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataWriteMode {
    /// Merge incoming JSON into any existing unexpired entry via
    /// [`merge_no_duplicates`].
    MergeExisting,
    /// Skip writing if an unexpired entry is already present (`$fast = 1`).
    FastSkipIfPresent,
    /// Overwrite any cached entry directly without merging.
    OverwriteDirect,
}

/// Two-tier persistent cache backed by a single [`RedbStore`].
#[derive(Debug)]
pub struct DualCache {
    store: RedbStore,
}

impl DualCache {
    /// Opens or creates a [`DualCache`] at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] or [`SpottyError::Cache`] if opening fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = RedbStore::open(path)?;
        Ok(Self { store })
    }

    /// Wraps an already-opened [`RedbStore`].
    #[must_use]
    pub fn from_store(store: RedbStore) -> Self {
        Self { store }
    }

    /// Returns a reference to the underlying [`RedbStore`].
    #[must_use]
    pub fn store(&self) -> &RedbStore {
        &self.store
    }

    /// Reads an unexpired ephemeral KV entry using the system clock.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn ephemeral_value(&self, key: &str) -> Result<Option<Value>> {
        self.ephemeral_value_at(key, current_epoch_secs())
    }

    /// Reads an unexpired ephemeral KV entry evaluated at `now_epoch_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn ephemeral_value_at(&self, key: &str, now_epoch_secs: u64) -> Result<Option<Value>> {
        self.store.ephemeral_at(key, now_epoch_secs)
    }

    /// Stores an ephemeral KV entry with the specified `ttl` from now.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] or [`SpottyError::Cache`] on failure.
    pub fn put_ephemeral(&self, key: &str, value: &Value, ttl: Duration) -> Result<()> {
        self.put_ephemeral_at(key, value, ttl.as_secs(), current_epoch_secs())
    }

    /// Stores an ephemeral KV entry expiring at `now_epoch_secs + ttl_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] or [`SpottyError::Cache`] on failure.
    pub fn put_ephemeral_at(
        &self,
        key: &str,
        value: &Value,
        ttl_secs: u64,
        now_epoch_secs: u64,
    ) -> Result<()> {
        let expires_at = now_epoch_secs.saturating_add(ttl_secs);
        self.store.put_ephemeral(key, value, expires_at)
    }

    /// Removes an ephemeral KV entry, returning `true` if one existed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` transaction failure.
    pub fn remove_ephemeral(&self, key: &str) -> Result<bool> {
        self.store.remove_ephemeral(key)
    }

    /// Reads an unexpired [`CachedHttpEntry`] using the system clock.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn http_entry(&self, key: &str) -> Result<Option<CachedHttpEntry>> {
        self.http_entry_at(key, current_epoch_secs())
    }

    /// Reads an unexpired [`CachedHttpEntry`] evaluated at `now_epoch_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn http_entry_at(&self, key: &str, now_epoch_secs: u64) -> Result<Option<CachedHttpEntry>> {
        self.store.http_entry_at(key, now_epoch_secs)
    }

    /// Reads a [`CachedHttpEntry`] even if expired, supporting `ETag` and
    /// `Last-Modified` conditional revalidation.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn http_entry_stale(&self, key: &str) -> Result<Option<CachedHttpEntry>> {
        self.store.http_entry_stale(key)
    }

    /// Persists a [`CachedHttpEntry`] in the ephemeral tier.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] or [`SpottyError::Cache`] on failure.
    pub fn put_http_entry(&self, key: &str, entry: &CachedHttpEntry) -> Result<()> {
        self.store.put_http_entry(key, entry)
    }

    /// Removes a stored HTTP cache entry, returning `true` if one existed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` transaction failure.
    pub fn remove_http_entry(&self, key: &str) -> Result<bool> {
        self.store.remove_http_entry(key)
    }

    /// Reads an unexpired 90-day metadata entry using the system clock.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn metadata_value(&self, uri: &str) -> Result<Option<Value>> {
        self.metadata_value_at(uri, current_epoch_secs())
    }

    /// Reads an unexpired 90-day metadata entry evaluated at `now_epoch_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn metadata_value_at(&self, uri: &str, now_epoch_secs: u64) -> Result<Option<Value>> {
        self.store.metadata_at(uri, now_epoch_secs)
    }

    /// Stores metadata for `uri` (and optional `linked_from_uri` alias) with
    /// the 90-day TTL according to `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `uri` is empty, or
    /// [`SpottyError::Cache`] / [`SpottyError::Json`] on storage failure.
    pub fn store_metadata(
        &self,
        uri: &str,
        linked_from_uri: Option<&str>,
        data: &Value,
        mode: MetadataWriteMode,
    ) -> Result<()> {
        self.store_metadata_at(uri, linked_from_uri, data, mode, current_epoch_secs())
    }

    /// Stores metadata at an explicit `now_epoch_secs` timestamp for
    /// deterministic TTL testing.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidUri`] if `uri` is empty, or
    /// [`SpottyError::Cache`] / [`SpottyError::Json`] on storage failure.
    pub fn store_metadata_at(
        &self,
        uri: &str,
        linked_from_uri: Option<&str>,
        data: &Value,
        mode: MetadataWriteMode,
        now_epoch_secs: u64,
    ) -> Result<()> {
        if uri.is_empty() {
            return Err(SpottyError::InvalidUri(String::from(
                "metadata URI must not be empty",
            )));
        }

        let expires_at = now_epoch_secs.saturating_add(METADATA_TTL_SECS);
        let written_primary =
            self.write_single_metadata(uri, data, mode, now_epoch_secs, expires_at)?;

        if let Some(linked_uri) = linked_from_uri.filter(|alt| !alt.is_empty() && *alt != uri) {
            let _written = self.write_single_metadata(
                linked_uri,
                &written_primary,
                mode,
                now_epoch_secs,
                expires_at,
            )?;
        }

        Ok(())
    }

    fn write_single_metadata(
        &self,
        key: &str,
        data: &Value,
        mode: MetadataWriteMode,
        now_epoch_secs: u64,
        expires_at: u64,
    ) -> Result<Value> {
        match mode {
            MetadataWriteMode::FastSkipIfPresent => {
                if let Some(existing) = self.store.metadata_at(key, now_epoch_secs)? {
                    return Ok(existing);
                }
                self.store.put_metadata(key, data, expires_at)?;
                Ok(data.clone())
            }
            MetadataWriteMode::MergeExisting => {
                let merged = if let Some(existing) = self.store.metadata_at(key, now_epoch_secs)? {
                    merge_no_duplicates(&existing, data)
                } else {
                    data.clone()
                };
                self.store.put_metadata(key, &merged, expires_at)?;
                Ok(merged)
            }
            MetadataWriteMode::OverwriteDirect => {
                self.store.put_metadata(key, data, expires_at)?;
                Ok(data.clone())
            }
        }
    }

    /// Prunes expired entries across both tables using the system clock.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn prune_expired(&self) -> Result<usize> {
        self.prune_expired_at(current_epoch_secs())
    }

    /// Prunes expired entries across both tables at `now_epoch_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn prune_expired_at(&self, now_epoch_secs: u64) -> Result<usize> {
        self.store.prune_expired_at(now_epoch_secs)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        DualCache, MetadataWriteMode, EPISODE_ARTIST_TTL_SECS, METADATA_TTL_SECS,
        PLAYLIST_OWNER_TTL_SECS,
    };
    use crate::backend::CachedHttpEntry;

    #[test]
    fn ttl_constants_match_legacy_durations() {
        assert_eq!(EPISODE_ARTIST_TTL_SECS, 7 * 86_400);
        assert_eq!(PLAYLIST_OWNER_TTL_SECS, 30 * 86_400);
        assert_eq!(METADATA_TTL_SECS, 90 * 86_400);
    }

    #[test]
    fn metadata_merge_preserves_image_and_indexes_linked_from() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let cache = DualCache::open(tmp.path().join("cache.redb")).expect("open cache");
        let t0 = 1_700_000_000_u64;

        let initial = json!({
            "uri": "spotify:track:canonical",
            "name": "Track",
            "added_at": "2026-01-01T00:00:00Z",
            "album": {
                "name": "Album",
                "image": "https://img/album.jpg"
            },
            "artists": [{"uri": "spotify:artist:1", "name": "Artist 1"}]
        });
        cache
            .store_metadata_at(
                "spotify:track:canonical",
                Some("spotify:track:original"),
                &initial,
                MetadataWriteMode::MergeExisting,
                t0,
            )
            .expect("initial store");

        let partial_update = json!({
            "uri": "spotify:track:canonical",
            "popularity": 77,
            "album": {"name": "Album"},
            "artists": [
                {"uri": "spotify:artist:1", "name": "Artist 1 Duplicate"},
                {"uri": "spotify:artist:2", "name": "Artist 2"}
            ]
        });
        cache
            .store_metadata_at(
                "spotify:track:canonical",
                Some("spotify:track:original"),
                &partial_update,
                MetadataWriteMode::MergeExisting,
                t0 + 100,
            )
            .expect("merge update");

        let canonical = cache
            .metadata_value_at("spotify:track:canonical", t0 + 200)
            .expect("read canonical")
            .expect("canonical present");
        let linked = cache
            .metadata_value_at("spotify:track:original", t0 + 200)
            .expect("read linked")
            .expect("linked present");

        assert_eq!(canonical, linked);
        assert_eq!(canonical["album"]["image"], "https://img/album.jpg");
        assert_eq!(canonical["added_at"], "2026-01-01T00:00:00Z");
        assert_eq!(canonical["popularity"], 77);
        assert_eq!(
            canonical["artists"]
                .as_array()
                .expect("artists array")
                .len(),
            2
        );

        // Expires after 90 days.
        let at_expiry = t0 + 100 + METADATA_TTL_SECS;
        assert_eq!(
            cache
                .metadata_value_at("spotify:track:canonical", at_expiry)
                .expect("read expired"),
            None
        );
    }

    #[test]
    fn fast_skip_if_present_keeps_existing_until_expiry() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let cache = DualCache::open(tmp.path().join("fast.redb")).expect("open cache");
        let t0 = 10_000_u64;

        cache
            .store_metadata_at(
                "spotify:album:1",
                None,
                &json!({"name": "First"}),
                MetadataWriteMode::OverwriteDirect,
                t0,
            )
            .expect("seed album");

        cache
            .store_metadata_at(
                "spotify:album:1",
                None,
                &json!({"name": "Second"}),
                MetadataWriteMode::FastSkipIfPresent,
                t0 + 60,
            )
            .expect("fast skip");

        let cached = cache
            .metadata_value_at("spotify:album:1", t0 + 120)
            .expect("read")
            .expect("present");
        assert_eq!(cached["name"], "First");

        // Once expired, FastSkipIfPresent writes the new payload.
        let after_90d = t0 + METADATA_TTL_SECS;
        cache
            .store_metadata_at(
                "spotify:album:1",
                None,
                &json!({"name": "Refreshed"}),
                MetadataWriteMode::FastSkipIfPresent,
                after_90d,
            )
            .expect("write after expiry");
        let refreshed = cache
            .metadata_value_at("spotify:album:1", after_90d + 1)
            .expect("read refreshed")
            .expect("refreshed present");
        assert_eq!(refreshed["name"], "Refreshed");
    }

    #[test]
    fn ephemeral_ttls_and_http_etag_stale_lookup() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let cache = DualCache::open(tmp.path().join("ephemeral.redb")).expect("open cache");
        let t0 = 5_000_u64;

        cache
            .put_ephemeral_at(
                "playlist_owner_abc",
                &json!("owner_user"),
                PLAYLIST_OWNER_TTL_SECS,
                t0,
            )
            .expect("put playlist owner");
        assert_eq!(
            cache
                .ephemeral_value_at("playlist_owner_abc", t0 + PLAYLIST_OWNER_TTL_SECS - 1)
                .expect("before 30d"),
            Some(json!("owner_user"))
        );
        assert_eq!(
            cache
                .ephemeral_value_at("playlist_owner_abc", t0 + PLAYLIST_OWNER_TTL_SECS)
                .expect("at 30d"),
            None
        );

        let http_entry = CachedHttpEntry::new(
            r#"{"tracks":[]}"#,
            Some(String::from("W/\"abc123\"")),
            None,
            t0 + 300,
        );
        cache
            .put_http_entry("playlist:abc", &http_entry)
            .expect("put http");
        assert!(cache
            .http_entry_at("playlist:abc", t0 + 299)
            .expect("fresh")
            .is_some());
        assert!(cache
            .http_entry_at("playlist:abc", t0 + 300)
            .expect("expired")
            .is_none());
        let stale = cache
            .http_entry_stale("playlist:abc")
            .expect("stale lookup")
            .expect("stale entry retained");
        assert_eq!(stale.etag.as_deref(), Some("W/\"abc123\""));
    }
}
