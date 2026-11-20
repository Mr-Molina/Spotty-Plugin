//! Low-level `redb` storage backend with two tables and TTL envelopes.

use std::fmt;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spotty_types::{Result, SpottyError};

const EPHEMERAL_TABLE: TableDefinition<'_, &str, &str> = TableDefinition::new("ephemeral_kv");
const METADATA_TABLE: TableDefinition<'_, &str, &str> = TableDefinition::new("metadata_90d");
const HTTP_KEY_PREFIX: &str = "http:";

/// Cached HTTP response body and conditional revalidation headers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedHttpEntry {
    /// Serialized HTTP response payload.
    pub body: String,
    /// Optional `ETag` response header for `If-None-Match`.
    pub etag: Option<String>,
    /// Optional `Last-Modified` header for `If-Modified-Since`.
    pub last_modified: Option<String>,
    /// Expiration timestamp in Unix epoch seconds.
    pub expires_at_epoch_secs: u64,
}

impl CachedHttpEntry {
    /// Creates a new [`CachedHttpEntry`].
    #[must_use]
    pub fn new(
        body: impl Into<String>,
        etag: Option<String>,
        last_modified: Option<String>,
        expires_at_epoch_secs: u64,
    ) -> Self {
        Self {
            body: body.into(),
            etag,
            last_modified,
            expires_at_epoch_secs,
        }
    }

    /// Returns `true` when `now_epoch_secs` is at or past expiration.
    #[must_use]
    pub fn is_expired_at(&self, now_epoch_secs: u64) -> bool {
        now_epoch_secs >= self.expires_at_epoch_secs
    }

    fn has_revalidation_validators(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope {
    value: Value,
    expires_at_epoch_secs: u64,
}

impl CacheEnvelope {
    fn is_expired_at(&self, now_epoch_secs: u64) -> bool {
        now_epoch_secs >= self.expires_at_epoch_secs
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
enum EphemeralRecord {
    Kv(CacheEnvelope),
    Http(CachedHttpEntry),
}

/// Persistent `redb` store managing `ephemeral_kv` and `metadata_90d` tables.
#[derive(Debug)]
pub struct RedbStore {
    db: Database,
}

impl RedbStore {
    /// Opens or creates a `redb` database at `path` and ensures both cache
    /// tables exist.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if parent directory creation fails or
    /// [`SpottyError::Cache`] if opening or initializing `redb` fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db_path = path.as_ref();
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let db = Database::create(db_path).map_err(map_cache_err)?;
        let write_txn = db.begin_write().map_err(map_cache_err)?;
        {
            let _ephemeral = write_txn
                .open_table(EPHEMERAL_TABLE)
                .map_err(map_cache_err)?;
            let _metadata = write_txn
                .open_table(METADATA_TABLE)
                .map_err(map_cache_err)?;
        }
        write_txn.commit().map_err(map_cache_err)?;

        Ok(Self { db })
    }

    /// Inserts or replaces an ephemeral JSON value with an explicit expiry.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] on serialization failure or
    /// [`SpottyError::Cache`] on `redb` write failure.
    pub fn put_ephemeral(
        &self,
        key: &str,
        value: &Value,
        expires_at_epoch_secs: u64,
    ) -> Result<()> {
        let record = EphemeralRecord::Kv(CacheEnvelope {
            value: value.clone(),
            expires_at_epoch_secs,
        });
        let payload = serde_json::to_string(&record)?;
        self.write_table_entry(EPHEMERAL_TABLE, key, &payload)
    }

    /// Reads an ephemeral JSON value at `now_epoch_secs`, pruning it if expired.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` I/O failure or
    /// [`SpottyError::Json`] if the stored envelope cannot be parsed.
    pub fn ephemeral_at(&self, key: &str, now_epoch_secs: u64) -> Result<Option<Value>> {
        let raw = self.read_table_entry(EPHEMERAL_TABLE, key)?;
        let Some(json_str) = raw else {
            return Ok(None);
        };

        let record: EphemeralRecord = serde_json::from_str(&json_str)?;
        let EphemeralRecord::Kv(envelope) = record else {
            return Ok(None);
        };

        if envelope.is_expired_at(now_epoch_secs) {
            let _removed = self.remove_table_entry(EPHEMERAL_TABLE, key)?;
            return Ok(None);
        }

        Ok(Some(envelope.value))
    }

    /// Removes an ephemeral KV entry, returning `true` if one existed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` transaction failure.
    pub fn remove_ephemeral(&self, key: &str) -> Result<bool> {
        self.remove_table_entry(EPHEMERAL_TABLE, key)
    }

    /// Stores a [`CachedHttpEntry`] in `ephemeral_kv` under an HTTP key namespace.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] or [`SpottyError::Cache`] if persisting fails.
    pub fn put_http_entry(&self, key: &str, entry: &CachedHttpEntry) -> Result<()> {
        let storage_key = http_storage_key(key);
        let record = EphemeralRecord::Http(entry.clone());
        let payload = serde_json::to_string(&record)?;
        self.write_table_entry(EPHEMERAL_TABLE, &storage_key, &payload)
    }

    /// Returns an unexpired [`CachedHttpEntry`] at `now_epoch_secs`, or `None`.
    ///
    /// Expired HTTP entries are intentionally retained on read so callers can
    /// inspect stale validators via [`Self::http_entry_stale`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn http_entry_at(&self, key: &str, now_epoch_secs: u64) -> Result<Option<CachedHttpEntry>> {
        let Some(entry) = self.http_entry_stale(key)? else {
            return Ok(None);
        };
        if entry.is_expired_at(now_epoch_secs) {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Returns a [`CachedHttpEntry`] even if expired, for `ETag` revalidation.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn http_entry_stale(&self, key: &str) -> Result<Option<CachedHttpEntry>> {
        let storage_key = http_storage_key(key);
        let raw = self.read_table_entry(EPHEMERAL_TABLE, &storage_key)?;
        let Some(json_str) = raw else {
            return Ok(None);
        };

        let record: EphemeralRecord = serde_json::from_str(&json_str)?;
        let EphemeralRecord::Http(entry) = record else {
            return Ok(None);
        };

        Ok(Some(entry))
    }

    /// Removes a stored HTTP cache entry, returning `true` if one existed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` transaction failure.
    pub fn remove_http_entry(&self, key: &str) -> Result<bool> {
        let storage_key = http_storage_key(key);
        self.remove_table_entry(EPHEMERAL_TABLE, &storage_key)
    }

    /// Writes a metadata JSON envelope into `metadata_90d`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] or [`SpottyError::Cache`] on failure.
    pub fn put_metadata(&self, key: &str, value: &Value, expires_at_epoch_secs: u64) -> Result<()> {
        let envelope = CacheEnvelope {
            value: value.clone(),
            expires_at_epoch_secs,
        };
        let payload = serde_json::to_string(&envelope)?;
        self.write_table_entry(METADATA_TABLE, key, &payload)
    }

    /// Reads an unexpired metadata entry at `now_epoch_secs`, pruning if stale.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn metadata_at(&self, key: &str, now_epoch_secs: u64) -> Result<Option<Value>> {
        let raw = self.read_table_entry(METADATA_TABLE, key)?;
        let Some(json_str) = raw else {
            return Ok(None);
        };

        let envelope: CacheEnvelope = serde_json::from_str(&json_str)?;
        if envelope.is_expired_at(now_epoch_secs) {
            let _removed = self.remove_table_entry(METADATA_TABLE, key)?;
            return Ok(None);
        }

        Ok(Some(envelope.value))
    }

    /// Removes a metadata entry, returning `true` if one existed.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] on `redb` transaction failure.
    pub fn remove_metadata(&self, key: &str) -> Result<bool> {
        self.remove_table_entry(METADATA_TABLE, key)
    }

    /// Scans both tables and prunes expired entries as of `now_epoch_secs`.
    ///
    /// Expired HTTP entries carrying an `ETag` or `Last-Modified` validator
    /// are preserved for conditional revalidation. Returns the count of pruned
    /// keys.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] on failure.
    pub fn prune_expired_at(&self, now_epoch_secs: u64) -> Result<usize> {
        let mut expired_ephemeral = Vec::new();
        let mut expired_metadata = Vec::new();

        {
            let read_txn = self.db.begin_read().map_err(map_cache_err)?;
            let eph_table = read_txn
                .open_table(EPHEMERAL_TABLE)
                .map_err(map_cache_err)?;
            for row in eph_table.iter().map_err(map_cache_err)? {
                let (key_guard, val_guard) = row.map_err(map_cache_err)?;
                let record: EphemeralRecord = serde_json::from_str(val_guard.value())?;
                let should_prune = match record {
                    EphemeralRecord::Kv(env) => env.is_expired_at(now_epoch_secs),
                    EphemeralRecord::Http(entry) => {
                        entry.is_expired_at(now_epoch_secs) && !entry.has_revalidation_validators()
                    }
                };
                if should_prune {
                    expired_ephemeral.push(key_guard.value().to_owned());
                }
            }

            let meta_table = read_txn.open_table(METADATA_TABLE).map_err(map_cache_err)?;
            for row in meta_table.iter().map_err(map_cache_err)? {
                let (key_guard, val_guard) = row.map_err(map_cache_err)?;
                let envelope: CacheEnvelope = serde_json::from_str(val_guard.value())?;
                if envelope.is_expired_at(now_epoch_secs) {
                    expired_metadata.push(key_guard.value().to_owned());
                }
            }
        }

        let mut pruned = 0_usize;
        if expired_ephemeral.is_empty() && expired_metadata.is_empty() {
            return Ok(0);
        }

        let write_txn = self.db.begin_write().map_err(map_cache_err)?;
        {
            let mut eph_table = write_txn
                .open_table(EPHEMERAL_TABLE)
                .map_err(map_cache_err)?;
            for key in &expired_ephemeral {
                if eph_table
                    .remove(key.as_str())
                    .map_err(map_cache_err)?
                    .is_some()
                {
                    pruned += 1;
                }
            }

            let mut meta_table = write_txn
                .open_table(METADATA_TABLE)
                .map_err(map_cache_err)?;
            for key in &expired_metadata {
                if meta_table
                    .remove(key.as_str())
                    .map_err(map_cache_err)?
                    .is_some()
                {
                    pruned += 1;
                }
            }
        }
        write_txn.commit().map_err(map_cache_err)?;

        Ok(pruned)
    }

    fn read_table_entry(
        &self,
        def: TableDefinition<'_, &str, &str>,
        key: &str,
    ) -> Result<Option<String>> {
        let read_txn = self.db.begin_read().map_err(map_cache_err)?;
        let table = read_txn.open_table(def).map_err(map_cache_err)?;
        let value = table
            .get(key)
            .map_err(map_cache_err)?
            .map(|guard| guard.value().to_owned());
        Ok(value)
    }

    fn write_table_entry(
        &self,
        def: TableDefinition<'_, &str, &str>,
        key: &str,
        payload: &str,
    ) -> Result<()> {
        let write_txn = self.db.begin_write().map_err(map_cache_err)?;
        {
            let mut table = write_txn.open_table(def).map_err(map_cache_err)?;
            table.insert(key, payload).map_err(map_cache_err)?;
        }
        write_txn.commit().map_err(map_cache_err)?;
        Ok(())
    }

    fn remove_table_entry(&self, def: TableDefinition<'_, &str, &str>, key: &str) -> Result<bool> {
        let write_txn = self.db.begin_write().map_err(map_cache_err)?;
        let existed = {
            let mut table = write_txn.open_table(def).map_err(map_cache_err)?;
            let removed = table.remove(key).map_err(map_cache_err)?.is_some();
            removed
        };
        write_txn.commit().map_err(map_cache_err)?;
        Ok(existed)
    }
}

/// Returns the current system clock in Unix epoch seconds.
#[must_use]
pub fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

fn http_storage_key(key: &str) -> String {
    format!("{HTTP_KEY_PREFIX}{key}")
}

fn map_cache_err(err: impl fmt::Display) -> SpottyError {
    SpottyError::Cache(err.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;

    use super::{CachedHttpEntry, RedbStore};

    #[test]
    fn redb_store_persists_across_reopen_and_prunes_expired_entries() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let db_path = tmp.path().join("nested").join("spotty.redb");

        {
            let store = RedbStore::open(&db_path).expect("initial open must succeed");
            store
                .put_ephemeral("token:abc", &json!({"access": "xyz"}), 1_000)
                .expect("ephemeral put must succeed");
            store
                .put_metadata("spotify:track:1", &json!({"name": "Song"}), 2_000)
                .expect("metadata put must succeed");
        }

        let reopened = RedbStore::open(&db_path).expect("reopen must succeed");
        assert_eq!(
            reopened.ephemeral_at("token:abc", 999).expect("read ok"),
            Some(json!({"access": "xyz"}))
        );
        // Reading at or past expiry prunes the entry and returns None.
        assert_eq!(
            reopened.ephemeral_at("token:abc", 1_000).expect("read ok"),
            None
        );
        assert_eq!(
            reopened.ephemeral_at("token:abc", 500).expect("read ok"),
            None
        );

        assert_eq!(
            reopened
                .metadata_at("spotify:track:1", 1_999)
                .expect("read ok"),
            Some(json!({"name": "Song"}))
        );
        assert_eq!(reopened.prune_expired_at(2_000).expect("prune ok"), 1);
        assert_eq!(
            reopened
                .metadata_at("spotify:track:1", 1_500)
                .expect("read ok"),
            None
        );
    }

    #[test]
    fn http_entry_stale_preserves_etag_after_expiry() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let db_path = tmp.path().join("http.redb");
        let store = RedbStore::open(&db_path).expect("open must succeed");

        let entry = CachedHttpEntry::new(
            r#"{"items":[]}"#,
            Some(String::from("\"etag-v1\"")),
            Some(String::from("Wed, 19 Nov 2026 12:00:00 GMT")),
            500,
        );
        store
            .put_http_entry("/v1/me/albums", &entry)
            .expect("put http entry must succeed");

        assert_eq!(
            store.http_entry_at("/v1/me/albums", 499).expect("fresh"),
            Some(entry.clone())
        );
        assert_eq!(
            store.http_entry_at("/v1/me/albums", 500).expect("expired"),
            None
        );

        // Stale entry remains available after prune_expired_at due to ETag.
        assert_eq!(store.prune_expired_at(900).expect("prune"), 0);
        let stale = store
            .http_entry_stale("/v1/me/albums")
            .expect("stale read")
            .expect("stale entry must remain");
        assert_eq!(stale.etag.as_deref(), Some("\"etag-v1\""));
        assert_eq!(stale.body, r#"{"items":[]}"#);
    }
}
