//! Spotify account folder management, credential storage, and recovery.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use md5::Digest as _;
use md5::Md5;
use serde::Deserialize;
use serde::Serialize;
use spotty_types::Result;
use spotty_types::SpottyError;
use tracing::warn;

/// Temporary folder name used by `librespot` during interactive authentication.
pub const AUTHENTICATE_DIR: &str = "__AUTHENTICATE__";

const CREDENTIALS_FILE: &str = "credentials.json";
const PLAYLIST_FOLDERS_DIR: &str = "playlistFolders";

/// Short 8-character lowercase hex identifier derived from a Spotify username.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    /// Computes the 8-hex account folder ID from a Spotify `username`.
    ///
    /// Matches `AccountHelper.pm`
    /// (`substr(md5_hex(utf8toLatin1Transliterate($u)), 0, 8)`).
    #[must_use]
    pub fn from_username(username: &str) -> Self {
        let latin1 = latin1_transliterate(username);
        let digest = Md5::digest(&latin1);
        let hex = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
        Self(hex)
    }

    /// Parses an existing 8-hex folder ID or [`AUTHENTICATE_DIR`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] if `raw` is neither an 8-character hex
    /// string nor [`AUTHENTICATE_DIR`].
    pub fn parse(raw: impl AsRef<str>) -> Result<Self> {
        let trimmed = raw.as_ref().trim();
        if trimmed == AUTHENTICATE_DIR || is_hex_len(trimmed, 8) {
            let normalized = if trimmed == AUTHENTICATE_DIR {
                String::from(AUTHENTICATE_DIR)
            } else {
                trimmed.to_ascii_lowercase()
            };
            Ok(Self(normalized))
        } else {
            Err(SpottyError::Cache(format!(
                "invalid Spotty account id: {trimmed}"
            )))
        }
    }

    /// Returns the account folder identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AccountId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for AccountId {
    type Err = SpottyError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Deserialized `librespot` `credentials.json` payload.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CredentialsRecord {
    /// Canonical Spotify username stored by `librespot`.
    pub username: String,
    /// Authentication credential type tag (for example `1` for reusable auth).
    pub auth_type: Option<u32>,
    /// Reusable authentication payload encoded as a string.
    pub auth_data: Option<String>,
}

impl CredentialsRecord {
    /// Creates a new [`CredentialsRecord`] with the given fields.
    #[must_use]
    pub fn new(
        username: impl Into<String>,
        auth_type: Option<u32>,
        auth_data: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            auth_type,
            auth_data,
        }
    }
}

/// Controls which directories are preserved in [`AccountStore::purge_cache`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeScope {
    /// Preserves valid 8-hex account folders, `playlistFolders`, and 12-hex
    /// player MAC directories.
    PreservePlayerMacs,
    /// Preserves valid 8-hex account folders and `playlistFolders`, removing
    /// 12-hex player MAC directories as well as stale folders.
    FullInit,
}

/// Manages Spotify account folders and credentials under `<cache>/spotty`.
#[derive(Debug)]
pub struct AccountStore {
    spotty_dir: PathBuf,
    cached_accounts: Mutex<Option<Vec<(String, AccountId)>>>,
}

impl AccountStore {
    /// Creates an [`AccountStore`] rooted at `<base_cache_dir>/spotty`.
    #[must_use]
    pub fn new(base_cache_dir: impl AsRef<Path>) -> Self {
        Self {
            spotty_dir: base_cache_dir.as_ref().join("spotty"),
            cached_accounts: Mutex::new(None),
        }
    }

    /// Returns the `<base_cache_dir>/spotty` directory path.
    #[must_use]
    pub fn spotty_dir(&self) -> &Path {
        &self.spotty_dir
    }

    /// Returns the directory path for a specific [`AccountId`].
    #[must_use]
    pub fn account_dir(&self, id: &AccountId) -> PathBuf {
        self.spotty_dir.join(id.as_str())
    }

    /// Executes `f` with a read-only view of the in-memory cached account list.
    ///
    /// # Panics
    ///
    /// Panics if the internal account cache mutex was poisoned by a prior panic.
    pub fn with_cached_accounts<R>(
        &self,
        f: impl FnOnce(Option<&[(String, AccountId)]>) -> R,
    ) -> R {
        let guard = self
            .cached_accounts
            .lock()
            .expect("account cache mutex is never poisoned");
        f(guard.as_deref())
    }

    /// Lists 8-hex account folders and [`AUTHENTICATE_DIR`] that contain
    /// `credentials.json`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if reading directory entries fails for a reason
    /// other than the `spotty` directory not existing yet.
    pub fn cache_folders(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.spotty_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(SpottyError::Io(err.to_string())),
        };

        let mut folders = Vec::new();
        for entry_res in entries {
            let entry = entry_res?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let is_account_dir =
                is_hex_len(&name, 8) || name.eq_ignore_ascii_case(AUTHENTICATE_DIR);
            if !is_account_dir {
                continue;
            }
            if path.join(CREDENTIALS_FILE).is_file() {
                folders.push(name);
            }
        }
        folders.sort();
        Ok(folders)
    }

    /// Promotes [`AUTHENTICATE_DIR`] to its `<8-hex>` account folder.
    ///
    /// Replaces an existing stale `<8-hex>` target folder if present. Returns
    /// `Ok(None)` when no valid pending authentication folder exists.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if filesystem promotion fails.
    pub fn promote_authenticate_folder(&self) -> Result<Option<AccountId>> {
        let auth_dir = self.spotty_dir.join(AUTHENTICATE_DIR);
        if !auth_dir.is_dir() {
            return Ok(None);
        }

        let auth_id = AccountId(String::from(AUTHENTICATE_DIR));
        let Some(record) = self.credentials(&auth_id)? else {
            return Ok(None);
        };
        if record.username.is_empty() {
            return Ok(None);
        }

        let target_id = AccountId::from_username(&record.username);
        let target_dir = self.account_dir(&target_id);
        if target_dir.exists() {
            fs::remove_dir_all(&target_dir)?;
        }
        fs::rename(&auth_dir, &target_dir)?;
        self.invalidate_cache();
        Ok(Some(target_id))
    }

    /// Reads and parses `credentials.json` for `id`.
    ///
    /// If `credentials.json` is malformed JSON or unreadable as UTF-8, backs it
    /// up to `<spotty_dir>/credentials-<unix_ts>.json` and removes the corrupt
    /// account directory instead of failing.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if backup or cleanup I/O fails unexpectedly.
    pub fn credentials(&self, id: &AccountId) -> Result<Option<CredentialsRecord>> {
        let folder = self.account_dir(id);
        let creds_path = folder.join(CREDENTIALS_FILE);
        if !creds_path.is_file() {
            return Ok(None);
        }

        let bytes = fs::read(&creds_path)?;
        let parsed = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| serde_json::from_str::<CredentialsRecord>(text).ok());

        if let Some(record) = parsed {
            return Ok(Some(record));
        }

        warn!(
            account_id = %id,
            path = %creds_path.display(),
            "corrupted credentials.json discovered; backing up and removing folder"
        );
        let backup_path = self.next_backup_path();
        if let Err(err) = fs::rename(&creds_path, &backup_path) {
            // Fall back to copy when rename fails across mount boundaries.
            fs::copy(&creds_path, &backup_path)?;
            warn!(error = %err, "rename failed during backup; used copy fallback");
        }
        if folder.exists() {
            fs::remove_dir_all(&folder)?;
        }
        self.invalidate_cache();
        Ok(None)
    }

    /// Returns all valid `(username, AccountId)` pairs sorted case-insensitively.
    ///
    /// Results are cached in memory until an account write, deletion, or purge
    /// invalidates the cache.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if reading the cache directory fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal account cache mutex was poisoned by a prior panic.
    pub fn all_credentials(&self) -> Result<Vec<(String, AccountId)>> {
        if let Some(cached) = self.with_cached_accounts(|opt| opt.map(<[_]>::to_vec)) {
            return Ok(cached);
        }

        let folders = self.cache_folders()?;
        let mut by_username: HashMap<String, AccountId> = HashMap::new();
        for folder in folders {
            let Ok(id) = AccountId::parse(&folder) else {
                continue;
            };
            let Some(record) = self.credentials(&id)? else {
                continue;
            };
            if record.username.is_empty() {
                continue;
            }
            if id.as_str() == AUTHENTICATE_DIR && by_username.contains_key(&record.username) {
                continue;
            }
            by_username.insert(record.username, id);
        }

        let mut accounts: Vec<(String, AccountId)> = by_username.into_iter().collect();
        accounts.sort_by(|a, b| {
            a.0.to_ascii_lowercase()
                .cmp(&b.0.to_ascii_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });

        if !accounts.is_empty() {
            let mut guard = self
                .cached_accounts
                .lock()
                .expect("account cache mutex is never poisoned");
            *guard = Some(accounts.clone());
        }
        Ok(accounts)
    }

    /// Returns `true` if `<spotty_dir>/credentials.json` exists.
    #[must_use]
    pub fn has_player_credentials(&self) -> bool {
        self.spotty_dir.join(CREDENTIALS_FILE).is_file()
    }

    /// Reads `<spotty_dir>/credentials.json` used for Zeroconf player auth.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] or [`SpottyError::Json`] if the file exists
    /// and cannot be read or deserialized.
    pub fn player_credentials(&self) -> Result<Option<CredentialsRecord>> {
        let path = self.spotty_dir.join(CREDENTIALS_FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(SpottyError::Io(err.to_string())),
        };
        let record: CredentialsRecord = serde_json::from_str(&raw)?;
        Ok(Some(record))
    }

    /// Deletes `<spotty_dir>/credentials.json` if it exists.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if deletion fails for a reason other than
    /// the file being absent.
    pub fn delete_player_credentials(&self) -> Result<()> {
        let path = self.spotty_dir.join(CREDENTIALS_FILE);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(SpottyError::Io(err.to_string())),
        }
    }

    /// Deletes the account folder for `id` and purges stale folders.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if removing the credentials or directory fails.
    pub fn delete_account(&self, id: &AccountId) -> Result<()> {
        let creds_path = self.account_dir(id).join(CREDENTIALS_FILE);
        match fs::remove_file(&creds_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(SpottyError::Io(err.to_string())),
        }
        self.invalidate_cache();
        let _removed = self.purge_cache(PurgeScope::PreservePlayerMacs)?;
        Ok(())
    }

    /// Removes stale subdirectories under `<spotty_dir>`, returning how many
    /// directories were deleted.
    ///
    /// Always preserves valid 8-hex account folders containing `credentials.json`
    /// and `playlistFolders`. Preserves 12-hex player MAC directories when `scope`
    /// is [`PurgeScope::PreservePlayerMacs`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if reading or removing directories fails.
    pub fn purge_cache(&self, scope: PurgeScope) -> Result<usize> {
        let entries = match fs::read_dir(&self.spotty_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(SpottyError::Io(err.to_string())),
        };

        let mut removed = 0usize;
        for entry_res in entries {
            let entry = entry_res?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };

            if is_hex_len(&name, 8) && path.join(CREDENTIALS_FILE).is_file() {
                continue;
            }
            if scope == PurgeScope::PreservePlayerMacs && is_hex_len(&name, 12) {
                continue;
            }
            if name == PLAYLIST_FOLDERS_DIR {
                continue;
            }

            fs::remove_dir_all(&path)?;
            removed += 1;
        }

        if removed > 0 {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    fn invalidate_cache(&self) {
        let mut guard = self
            .cached_accounts
            .lock()
            .expect("account cache mutex is never poisoned");
        *guard = None;
    }

    fn next_backup_path(&self) -> PathBuf {
        let unix_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let primary = self.spotty_dir.join(format!("credentials-{unix_ts}.json"));
        if !primary.exists() {
            return primary;
        }
        let mut suffix = 1u32;
        loop {
            let candidate = self
                .spotty_dir
                .join(format!("credentials-{unix_ts}-{suffix}.json"));
            if !candidate.exists() {
                return candidate;
            }
            suffix += 1;
        }
    }
}

fn is_hex_len(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn latin1_transliterate(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for ch in input.chars() {
        let code = u32::from(ch);
        if code <= 0xFF {
            out.push(code as u8);
            continue;
        }
        if let Some(replacement) = transliterate_non_latin1(ch) {
            out.extend_from_slice(replacement.as_bytes());
        } else if !(0x0300..=0x036F).contains(&code) {
            // Drop combining diacritical marks; map unrepresentable glyphs to '?'.
            out.push(b'?');
        }
    }
    out
}

fn transliterate_non_latin1(ch: char) -> Option<&'static str> {
    match ch {
        '\u{0152}' => Some("OE"),
        '\u{0153}' => Some("oe"),
        '\u{0160}' => Some("S"),
        '\u{0161}' => Some("s"),
        '\u{017D}' => Some("Z"),
        '\u{017E}' => Some("z"),
        '\u{0178}' => Some("Y"),
        '\u{0141}' => Some("L"),
        '\u{0142}' => Some("l"),
        '\u{010C}' => Some("C"),
        '\u{010D}' => Some("c"),
        '\u{0106}' => Some("C"),
        '\u{0107}' => Some("c"),
        '\u{0110}' => Some("D"),
        '\u{0111}' => Some("d"),
        '\u{2018}' | '\u{2019}' => Some("'"),
        '\u{201C}' | '\u{201D}' => Some("\""),
        '\u{2013}' | '\u{2014}' => Some("-"),
        '\u{20AC}' => Some("EUR"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory should be created");
        }
        fs::write(path, content).expect("test file should be written");
    }

    #[test]
    fn non_ascii_username_uses_latin1_transliteration_for_md5() {
        // "täst" in Latin-1 is [0x74, 0xE4, 0x73, 0x74], whereas UTF-8 has 0xC3 0xA4.
        let latin1_bytes = [0x74_u8, 0xE4, 0x73, 0x74];
        let expected_digest = Md5::digest(latin1_bytes);
        let expected_hex = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            expected_digest[0], expected_digest[1], expected_digest[2], expected_digest[3]
        );

        let utf8_digest = Md5::digest("täst".as_bytes());
        let utf8_hex = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            utf8_digest[0], utf8_digest[1], utf8_digest[2], utf8_digest[3]
        );
        assert_ne!(
            expected_hex, utf8_hex,
            "Latin-1 and UTF-8 digests must differ for non-ASCII test vector"
        );

        let account_id = AccountId::from_username("täst");
        assert_eq!(
            account_id.as_str(),
            expected_hex,
            "expected Latin-1 MD5 prefix {expected_hex}, got {}",
            account_id.as_str()
        );
        assert_eq!(account_id.as_str().len(), 8);
    }

    #[test]
    fn promote_authenticate_folder_overwrites_stale_target() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());
        let target_id = AccountId::from_username("alice");

        // Seed a stale target folder that must be replaced.
        let stale_creds = store.account_dir(&target_id).join(CREDENTIALS_FILE);
        write_file(&stale_creds, br#"{"username":"alice","auth_data":"old"}"#);

        // Seed __AUTHENTICATE__ with fresh credentials.
        let auth_creds = store
            .spotty_dir()
            .join(AUTHENTICATE_DIR)
            .join(CREDENTIALS_FILE);
        write_file(
            &auth_creds,
            br#"{"username":"alice","auth_type":1,"auth_data":"fresh-blob"}"#,
        );

        // Warm cache first to ensure promotion invalidates it.
        let initial = store
            .all_credentials()
            .expect("all_credentials should succeed");
        assert_eq!(initial.len(), 1);
        store.with_cached_accounts(|cached| {
            assert!(
                cached.is_some(),
                "cache should be populated before promotion"
            );
        });

        let promoted = store
            .promote_authenticate_folder()
            .expect("promotion should succeed")
            .expect("promotion should return target AccountId");
        assert_eq!(promoted, target_id);
        assert!(!store.spotty_dir().join(AUTHENTICATE_DIR).exists());

        store.with_cached_accounts(|cached| {
            assert!(
                cached.is_none(),
                "cache must be invalidated after promotion"
            );
        });

        let loaded = store
            .credentials(&target_id)
            .expect("credentials read should succeed")
            .expect("promoted credentials must exist");
        assert_eq!(loaded.username, "alice");
        assert_eq!(loaded.auth_type, Some(1));
        assert_eq!(loaded.auth_data.as_deref(), Some("fresh-blob"));
    }

    #[test]
    fn corrupt_credentials_json_is_backed_up_and_folder_removed() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());
        let bad_id = AccountId::from_username("corrupt-user");
        let bad_folder = store.account_dir(&bad_id);
        let bad_creds = bad_folder.join(CREDENTIALS_FILE);
        write_file(&bad_creds, b"{not valid json!!!");

        let result = store
            .credentials(&bad_id)
            .expect("corrupt credentials must recover instead of erroring");
        assert!(result.is_none(), "corrupt credentials should return None");
        assert!(
            !bad_folder.exists(),
            "corrupt folder {} should be removed",
            bad_folder.display()
        );

        let backups: Vec<PathBuf> = fs::read_dir(store.spotty_dir())
            .expect("spotty dir should exist")
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("credentials-") && n.ends_with(".json"))
            })
            .collect();
        assert_eq!(backups.len(), 1, "expected 1 backup file, got {backups:?}");
        let backup_body = fs::read(&backups[0]).expect("backup file should be readable");
        assert_eq!(backup_body, b"{not valid json!!!");
    }

    #[test]
    fn purge_cache_preserves_12hex_macs_on_preserve_and_removes_on_full_init() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());
        let spotty = store.spotty_dir();

        let valid_id = AccountId::from_username("bob");
        write_file(
            &store.account_dir(&valid_id).join(CREDENTIALS_FILE),
            br#"{"username":"bob"}"#,
        );
        let mac_dir = spotty.join("aabbcc112233");
        write_file(&mac_dir.join("cache.bin"), b"player-data");
        let playlists_dir = spotty.join("playlistFolders");
        write_file(&playlists_dir.join("folders.json"), b"[]");
        let stale_dir = spotty.join("deadbeef");
        fs::create_dir_all(&stale_dir).expect("stale dir should be created");
        let auth_dir = spotty.join(AUTHENTICATE_DIR);
        fs::create_dir_all(&auth_dir).expect("auth dir should be created");

        let removed_non_init = store
            .purge_cache(PurgeScope::PreservePlayerMacs)
            .expect("preserve purge should succeed");
        assert_eq!(
            removed_non_init, 2,
            "stale 8-hex without creds + __AUTHENTICATE__"
        );
        assert!(store.account_dir(&valid_id).exists());
        assert!(mac_dir.exists(), "12-hex MAC dir must be preserved");
        assert!(playlists_dir.exists(), "playlistFolders must be preserved");
        assert!(!stale_dir.exists());
        assert!(!auth_dir.exists());

        let removed_init = store
            .purge_cache(PurgeScope::FullInit)
            .expect("full init purge should succeed");
        assert_eq!(
            removed_init, 1,
            "12-hex MAC dir should be purged on FullInit"
        );
        assert!(!mac_dir.exists());
        assert!(store.account_dir(&valid_id).exists());
        assert!(playlists_dir.exists());
    }

    #[test]
    fn all_credentials_sorts_case_insensitively_and_caches_in_mutex() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());

        let id_z = AccountId::from_username("zoe");
        let id_b = AccountId::from_username("Beta");
        let id_a = AccountId::from_username("alpha");

        write_file(
            &store.account_dir(&id_z).join(CREDENTIALS_FILE),
            br#"{"username":"zoe"}"#,
        );
        write_file(
            &store.account_dir(&id_b).join(CREDENTIALS_FILE),
            br#"{"username":"Beta"}"#,
        );
        write_file(
            &store.account_dir(&id_a).join(CREDENTIALS_FILE),
            br#"{"username":"alpha"}"#,
        );

        let accounts = store
            .all_credentials()
            .expect("all_credentials should succeed");
        assert_eq!(
            accounts,
            vec![
                (String::from("alpha"), id_a.clone()),
                (String::from("Beta"), id_b.clone()),
                (String::from("zoe"), id_z.clone()),
            ]
        );

        store.with_cached_accounts(|cached| {
            let slice = cached.expect("accounts should be cached");
            assert_eq!(slice.len(), 3);
        });

        store
            .delete_account(&id_b)
            .expect("delete_account should succeed");
        store.with_cached_accounts(|cached| {
            assert!(cached.is_none(), "delete_account must invalidate cache");
        });
        assert!(!store.account_dir(&id_b).exists());
    }

    #[test]
    fn player_credentials_lifecycle() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());

        assert!(!store.has_player_credentials());
        assert_eq!(
            store
                .player_credentials()
                .expect("missing player creds should be Ok(None)"),
            None
        );

        write_file(
            &store.spotty_dir().join(CREDENTIALS_FILE),
            br#"{"username":"player-user","auth_type":1}"#,
        );
        assert!(store.has_player_credentials());
        let creds = store
            .player_credentials()
            .expect("player creds should parse")
            .expect("player creds should exist");
        assert_eq!(creds.username, "player-user");
        assert_eq!(creds.auth_type, Some(1));

        store
            .delete_player_credentials()
            .expect("delete_player_credentials should succeed");
        assert!(!store.has_player_credentials());
        // Deleting when absent should remain idempotent.
        store
            .delete_player_credentials()
            .expect("idempotent delete should succeed");
    }
}
