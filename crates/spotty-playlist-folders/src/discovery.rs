//! Desktop Spotify `Storage/*.file` discovery, `0.8` scorer, and upload GC.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use spotty_cache::DualCache;
use spotty_types::Result;
use spotty_types::SpottyError;

use crate::parser::contains_word;
use crate::parser::parse_folder_blob;
use crate::parser::FolderEntry;
use crate::parser::MAX_FOLDER_FILE_BYTES;

/// Minimum fraction of playlists that must match a cached folder hierarchy.
pub const ASSUMED_HIT_THRESHOLD: f64 = 0.8;

/// Maximum age in seconds (30 days) for uploaded folder files before expiry.
pub const MAX_FOLDER_FILE_AGE_SECS: u64 = 86_400 * 30;

const CANDIDATE_CACHE_TTL_SECS: u64 = 86_400 * 7;
const SMALL_BYTES_LIMIT: u64 = 3200;

/// Retention policy for [`FolderUploadManager::purge_folder_cache`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeMode {
    /// Remove only files older than [`MAX_FOLDER_FILE_AGE_SECS`].
    ExpiredOnly,
    /// Remove every file in the upload cache directory regardless of age.
    DeleteAll,
}

/// Discovers Spotify `Storage/*.file` candidates and scores hierarchy matches.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FolderDiscovery;

impl FolderDiscovery {
    /// Creates a new [`FolderDiscovery`] helper.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Scans `upload_dir` and optional `extra_storage_dir` for `.file` blobs
    /// containing a `start-group` marker, memoizing
    /// `spotty:2:<path>:<size>:<mtime>` results in `cache`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] or [`SpottyError::Cache`] if filesystem
    /// traversal or cache I/O fails.
    pub fn scan_candidate_files(
        upload_dir: &Path,
        extra_storage_dir: Option<&Path>,
        cache: &DualCache,
    ) -> Result<Vec<PathBuf>> {
        let mut candidates = Vec::new();
        let dirs = [Some(upload_dir), extra_storage_dir];

        for maybe_dir in dirs {
            let Some(root) = maybe_dir else {
                continue;
            };
            if !root.is_dir() {
                continue;
            }
            scan_dir_recursive(root, cache, &mut candidates)?;
        }

        Ok(candidates)
    }

    /// Evaluates `candidate_files` against `playlist_uris` and returns the
    /// highest-scoring hierarchy exceeding [`ASSUMED_HIT_THRESHOLD`] (`0.8`),
    /// breaking score ties by the most recent file modification timestamp.
    ///
    /// Normalizes `spotify:user:[^:]+:playlist:` URIs to `spotify:playlist:`
    /// without mutating the caller's slice.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] or [`SpottyError::Cache`] on unexpected
    /// filesystem or cache failures.
    pub fn find_best_hierarchy(
        playlist_uris: &[String],
        candidate_files: &[PathBuf],
        cache: &DualCache,
    ) -> Result<Option<HashMap<String, FolderEntry>>> {
        if playlist_uris.is_empty() || candidate_files.is_empty() {
            return Ok(None);
        }

        let mut normalized_uris: Vec<Cow<'_, str>> = Vec::with_capacity(playlist_uris.len());
        for uri in playlist_uris {
            normalized_uris.push(normalize_playlist_uri(uri));
        }

        let total = playlist_uris.len() as f64;
        let mut best: Option<ScoredHierarchy> = None;

        for candidate in candidate_files {
            let Ok(meta) = fs::metadata(candidate) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_FOLDER_FILE_BYTES as u64 {
                continue;
            }

            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            let Some(hierarchy) = load_parsed_hierarchy(candidate, &meta, cache)? else {
                continue;
            };

            let mut hits: usize = 0;
            for uri in &normalized_uris {
                if matches!(
                    hierarchy.get(uri.as_ref()),
                    Some(FolderEntry::Playlist { .. })
                ) {
                    hits += 1;
                }
            }

            let ratio = (hits as f64) / total;
            let replaces_best = match &best {
                None => true,
                Some(current) => is_better_candidate(ratio, mtime, current.ratio, current.mtime),
            };
            if replaces_best {
                best = Some(ScoredHierarchy {
                    ratio,
                    mtime,
                    hierarchy,
                });
            }
        }

        let Some(winner) = best else {
            return Ok(None);
        };

        if winner.ratio > ASSUMED_HIT_THRESHOLD {
            Ok(Some(winner.hierarchy))
        } else {
            Ok(None)
        }
    }
}

/// Manages manual playlist folder uploads and cache garbage collection.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FolderUploadManager;

impl FolderUploadManager {
    /// Creates a new [`FolderUploadManager`] helper.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Validates an uploaded folder blob (`<= 512 KiB` and parsing yields
    /// `>= 2` entries), persists it in `upload_dir`, and warms `cache`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] if validation fails or [`SpottyError::Io`]
    /// if the filename is invalid or writing fails.
    pub fn store_uploaded_blob(
        upload_dir: &Path,
        filename: &str,
        bytes: &[u8],
        cache: &DualCache,
    ) -> Result<PathBuf> {
        if bytes.len() > MAX_FOLDER_FILE_BYTES {
            return Err(SpottyError::Cache(format!(
                "uploaded folder file exceeds {} limit ({})",
                format_file_size_kb(MAX_FOLDER_FILE_BYTES as u64),
                format_file_size_kb(bytes.len() as u64),
            )));
        }

        let Some(safe_name) = Path::new(filename).file_name() else {
            return Err(SpottyError::Io(format!(
                "invalid uploaded filename: {filename}"
            )));
        };

        let parsed = parse_folder_blob(bytes)?;
        if parsed.len() < 2 {
            return Err(SpottyError::Cache(String::from(
                "No playlist items found in uploaded folder blob",
            )));
        }

        fs::create_dir_all(upload_dir)?;
        let dest = upload_dir.join(safe_name);
        fs::write(&dest, bytes)?;

        let meta = fs::metadata(&dest)?;
        let key = file_cache_key(&dest, meta.len(), modified_epoch_secs(&meta));
        cache.put_ephemeral(
            &key,
            &Value::Bool(true),
            Duration::from_secs(CANDIDATE_CACHE_TTL_SECS),
        )?;

        Ok(dest)
    }

    /// Purges uploaded folder files in `upload_dir` according to `mode`,
    /// evaluating file age independently per entry without mutating `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if reading directory entries, file metadata,
    /// or deleting a file fails.
    pub fn purge_folder_cache(
        upload_dir: &Path,
        mode: PurgeMode,
        now_epoch_secs: u64,
    ) -> Result<usize> {
        if !upload_dir.is_dir() {
            return Ok(0);
        }

        let mut deleted: usize = 0;
        for entry_result in fs::read_dir(upload_dir)? {
            let entry = entry_result?;
            if !entry.file_type()?.is_file() {
                continue;
            }

            let should_delete = match mode {
                PurgeMode::DeleteAll => true,
                PurgeMode::ExpiredOnly => {
                    let meta = entry.metadata()?;
                    let mtime_secs = modified_epoch_secs(&meta);
                    now_epoch_secs.saturating_sub(mtime_secs) > MAX_FOLDER_FILE_AGE_SECS
                }
            };

            if should_delete {
                fs::remove_file(entry.path())?;
                deleted += 1;
            }
        }

        Ok(deleted)
    }
}

/// Returns the platform-specific default desktop Spotify `Storage` directory.
#[must_use]
pub fn default_spotify_storage_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        env::var_os("LOCALAPPDATA")
            .filter(|val| !val.is_empty())
            .map(|base| PathBuf::from(base).join("Spotify").join("Storage"))
    }

    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME")
            .filter(|val| !val.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join("Library/Application Support/Spotify/PersistentCache/Storage")
            })
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let cache_root = env::var_os("XDG_CACHE_HOME")
            .filter(|val| !val.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|val| !val.is_empty())
                    .map(|home| PathBuf::from(home).join(".cache"))
            })?;
        Some(cache_root.join("spotify").join("Storage"))
    }
}

/// Formats a byte count matching `PlaylistFolders.pm:440` (`< 3200` as
/// `"{bytes} Bytes"`, otherwise integer `"{} KB"`).
#[must_use]
pub fn format_file_size_kb(bytes: u64) -> String {
    if bytes < SMALL_BYTES_LIMIT {
        format!("{bytes} Bytes")
    } else {
        format!("{} KB", bytes / 1024)
    }
}

struct ScoredHierarchy {
    ratio: f64,
    mtime: SystemTime,
    hierarchy: HashMap<String, FolderEntry>,
}

fn is_better_candidate(
    ratio: f64,
    mtime: SystemTime,
    best_ratio: f64,
    best_mtime: SystemTime,
) -> bool {
    if (ratio - best_ratio).abs() <= f64::EPSILON {
        mtime > best_mtime
    } else {
        ratio > best_ratio
    }
}

fn scan_dir_recursive(root: &Path, cache: &DualCache, candidates: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        for entry_res in entries {
            let Ok(entry) = entry_res else {
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || !has_file_extension(&path) {
                continue;
            }

            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            if size > MAX_FOLDER_FILE_BYTES as u64 {
                continue;
            }

            let mtime = modified_epoch_secs(&meta);
            let key = file_cache_key(&path, size, mtime);
            if let Some(cached) = cache.ephemeral_value(&key)? {
                if cached.as_bool() == Some(true) {
                    candidates.push(path);
                }
                continue;
            }

            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let is_candidate =
                bytes.len() <= MAX_FOLDER_FILE_BYTES && contains_word(&bytes, b"start-group");
            cache.put_ephemeral(
                &key,
                &Value::Bool(is_candidate),
                Duration::from_secs(CANDIDATE_CACHE_TTL_SECS),
            )?;
            if is_candidate {
                candidates.push(path);
            }
        }
    }

    Ok(())
}

fn load_parsed_hierarchy(
    path: &Path,
    meta: &fs::Metadata,
    cache: &DualCache,
) -> Result<Option<HashMap<String, FolderEntry>>> {
    let parsed_key = format!(
        "spotty:2:parsed:{}:{}:{}",
        path.display(),
        meta.len(),
        modified_epoch_secs(meta)
    );
    if let Some(cached_val) = cache.ephemeral_value(&parsed_key)? {
        if let Ok(map) = serde_json::from_value::<HashMap<String, FolderEntry>>(cached_val) {
            return Ok(Some(map));
        }
    }

    let Ok(bytes) = fs::read(path) else {
        return Ok(None);
    };
    let Ok(parsed) = parse_folder_blob(&bytes) else {
        return Ok(None);
    };

    if let Ok(json_val) = serde_json::to_value(&parsed) {
        cache.put_ephemeral(
            &parsed_key,
            &json_val,
            Duration::from_secs(CANDIDATE_CACHE_TTL_SECS),
        )?;
    }

    Ok(Some(parsed))
}

fn has_file_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("file"))
}

fn file_cache_key(path: &Path, size: u64, mtime: u64) -> String {
    format!("spotty:2:{}:{size}:{mtime}", path.display())
}

fn modified_epoch_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |dur| dur.as_secs())
}

pub(crate) fn normalize_playlist_uri(uri: &str) -> Cow<'_, str> {
    if let Some(after_user_prefix) = uri.strip_prefix("spotify:user:") {
        if let Some((user, tail)) = after_user_prefix.split_once(':') {
            if !user.is_empty() && tail.starts_with("playlist:") {
                return Cow::Owned(format!("spotify:{tail}"));
            }
        }
    }
    Cow::Borrowed(uri)
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;

    use tempfile::tempdir;

    use super::*;

    fn test_cache(dir: &Path) -> DualCache {
        DualCache::open(dir.join("cache.redb")).expect("test DualCache should open")
    }

    fn write_with_mtime(path: &Path, bytes: &[u8], mtime_secs: u64) {
        fs::write(path, bytes).expect("fixture file should write");
        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("fixture file should open for timestamp update");
        let times = FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs));
        file.set_times(times).expect("mtime should update");
    }

    #[test]
    fn purge_expired_evaluates_each_file_independently_review_focus_3() {
        let dir = tempdir().expect("tempdir should allocate");
        let now_secs: u64 = 1_750_000_000;
        let forty_days_ago = now_secs - (86_400 * 40);
        let one_day_ago = now_secs - 86_400;

        let old_file = dir.path().join("00_old.file");
        let fresh_file = dir.path().join("99_fresh.file");
        write_with_mtime(&old_file, b"old", forty_days_ago);
        write_with_mtime(&fresh_file, b"fresh", one_day_ago);

        let deleted =
            FolderUploadManager::purge_folder_cache(dir.path(), PurgeMode::ExpiredOnly, now_secs)
                .expect("purge_folder_cache should succeed");

        assert_eq!(deleted, 1, "only the 40-day-old file must be deleted");
        assert!(!old_file.exists(), "40-day-old file should be removed");
        assert!(fresh_file.exists(), "1-day-old file must be preserved");
    }

    #[test]
    fn purge_delete_all_removes_fresh_files() {
        let dir = tempdir().expect("tempdir should allocate");
        let now_secs: u64 = 1_750_000_000;
        let fresh_a = dir.path().join("a.file");
        let fresh_b = dir.path().join("b.file");
        write_with_mtime(&fresh_a, b"a", now_secs - 60);
        write_with_mtime(&fresh_b, b"b", now_secs - 120);

        let deleted =
            FolderUploadManager::purge_folder_cache(dir.path(), PurgeMode::DeleteAll, now_secs)
                .expect("DeleteAll purge should succeed");
        assert_eq!(deleted, 2, "DeleteAll must delete both files");
        assert!(!fresh_a.exists() && !fresh_b.exists());
    }

    #[test]
    fn scans_candidates_and_memoizes_in_dual_cache() {
        let dir = tempdir().expect("tempdir should allocate");
        let upload_dir = dir.path().join("uploads");
        let storage_dir = dir.path().join("storage");
        fs::create_dir_all(&upload_dir).expect("upload dir should create");
        fs::create_dir_all(storage_dir.join("nested")).expect("nested storage dir should create");
        let cache = test_cache(dir.path());

        let valid_blob = b"spotify:start-group:f1:Mix\rspotify:playlist:1111111111111111111111\r";
        let valid_path = storage_dir.join("nested").join("good.file");
        let flat_path = upload_dir.join("flat.file");
        let ignored_ext = storage_dir.join("notes.txt");
        fs::write(&valid_path, valid_blob).expect("valid file writes");
        fs::write(&flat_path, b"spotify:playlist:2222222222222222222222\r")
            .expect("flat file writes");
        fs::write(&ignored_ext, valid_blob).expect("txt file writes");

        let found = FolderDiscovery::scan_candidate_files(&upload_dir, Some(&storage_dir), &cache)
            .expect("scan_candidate_files should succeed");
        assert_eq!(found, vec![valid_path.clone()]);

        let meta = fs::metadata(&valid_path).expect("metadata exists");
        let key = file_cache_key(&valid_path, meta.len(), modified_epoch_secs(&meta));
        let memoized = cache.ephemeral_value(&key).expect("cache lookup succeeds");
        assert_eq!(
            memoized,
            Some(Value::Bool(true)),
            "candidate key must be memoized in DualCache"
        );
    }

    #[test]
    fn finds_best_hierarchy_above_threshold_without_mutating_uris() {
        let dir = tempdir().expect("tempdir should allocate");
        let cache = test_cache(dir.path());

        let blob_partial = b"spotify:start-group:f1:A\r\
            spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa\r\
            spotify:playlist:bbbbbbbbbbbbbbbbbbbbbb\r\
            spotify:end-group:f1\r";
        let blob_old_full = b"spotify:start-group:f_old:Old\r\
            spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa\r\
            spotify:playlist:bbbbbbbbbbbbbbbbbbbbbb\r\
            spotify:playlist:cccccccccccccccccccccc\r\
            spotify:playlist:dddddddddddddddddddddd\r\
            spotify:playlist:eeeeeeeeeeeeeeeeeeeeee\r\
            spotify:end-group:f_old\r";
        let blob_new_full = b"spotify:start-group:f_new:New\r\
            spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa\r\
            spotify:playlist:bbbbbbbbbbbbbbbbbbbbbb\r\
            spotify:playlist:cccccccccccccccccccccc\r\
            spotify:playlist:dddddddddddddddddddddd\r\
            spotify:playlist:eeeeeeeeeeeeeeeeeeeeee\r\
            spotify:end-group:f_new\r";

        let p_partial = dir.path().join("partial.file");
        let p_old = dir.path().join("old.file");
        let p_new = dir.path().join("new.file");
        write_with_mtime(&p_partial, blob_partial, 1_700_000_000);
        write_with_mtime(&p_old, blob_old_full, 1_700_000_100);
        write_with_mtime(&p_new, blob_new_full, 1_700_000_200);

        let uris = vec![
            String::from("spotify:user:alice:playlist:aaaaaaaaaaaaaaaaaaaaaa"),
            String::from("spotify:playlist:bbbbbbbbbbbbbbbbbbbbbb"),
            String::from("spotify:playlist:cccccccccccccccccccccc"),
            String::from("spotify:playlist:dddddddddddddddddddddd"),
            String::from("spotify:playlist:eeeeeeeeeeeeeeeeeeeeee"),
        ];
        let original_uris = uris.clone();

        let exact_08_only =
            FolderDiscovery::find_best_hierarchy(&uris, std::slice::from_ref(&p_partial), &cache)
                .expect("scoring succeeds");
        assert!(
            exact_08_only.is_none(),
            "2/5 = 0.4 must not pass strict > 0.8 threshold"
        );

        let tie_08_uris = vec![
            String::from("spotify:playlist:aaaaaaaaaaaaaaaaaaaaaa"),
            String::from("spotify:playlist:bbbbbbbbbbbbbbbbbbbbbb"),
            String::from("spotify:playlist:cccccccccccccccccccccc"),
            String::from("spotify:playlist:dddddddddddddddddddddd"),
            String::from("spotify:playlist:ffffffffffffffffffffff"),
        ];
        let at_boundary = FolderDiscovery::find_best_hierarchy(
            &tie_08_uris,
            std::slice::from_ref(&p_new),
            &cache,
        )
        .expect("boundary check succeeds");
        assert!(
            at_boundary.is_none(),
            "exact 4/5 = 0.8 must not pass strict > 0.8 threshold"
        );

        let best = FolderDiscovery::find_best_hierarchy(&uris, &[p_partial, p_old, p_new], &cache)
            .expect("find_best_hierarchy succeeds")
            .expect("100% match must win");
        assert!(
            best.contains_key("f_new"),
            "newer mtime must win ties at equal hit ratio"
        );
        assert_eq!(
            uris, original_uris,
            "caller URI slice must never be mutated"
        );
    }

    #[test]
    fn store_uploaded_blob_validates_size_and_entry_count() {
        let dir = tempdir().expect("tempdir should allocate");
        let upload_dir = dir.path().join("uploads");
        let cache = test_cache(dir.path());

        let too_few = b"spotify:start-group:onlyone:Solo\rspotify:end-group:onlyone\r";
        let err_few =
            FolderUploadManager::store_uploaded_blob(&upload_dir, "solo.file", too_few, &cache);
        assert!(err_few.is_err(), "blob with < 2 entries must be rejected");
        assert!(!upload_dir.join("solo.file").exists());

        let valid = b"spotify:start-group:f1:One\rspotify:playlist:1111111111111111111111\r";
        let stored = FolderUploadManager::store_uploaded_blob(
            &upload_dir,
            "../../evil/my.file",
            valid,
            &cache,
        )
        .expect("valid uploaded blob should store");
        assert_eq!(stored, upload_dir.join("my.file"));
        assert!(stored.exists());
    }

    #[test]
    fn formats_file_size_matching_perl_threshold() {
        assert_eq!(format_file_size_kb(0), "0 Bytes");
        assert_eq!(format_file_size_kb(3199), "3199 Bytes");
        assert_eq!(format_file_size_kb(3200), "3 KB");
        assert_eq!(format_file_size_kb(512 * 1024), "512 KB");
    }
}
