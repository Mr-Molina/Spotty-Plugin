//! Playlist folders settings controller (`Settings/PlaylistFolders.pm`).

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use spotty_cache::DualCache;
use spotty_playlist_folders::{
    default_spotify_storage_dir, format_file_size_kb, FolderDiscovery, FolderUploadManager,
    PurgeMode,
};
use spotty_types::{Result, SpottyError};

const DELETE_PARAM_PREFIX: &str = "delete_";
const FOLDER_FILE_EXT: &str = ".file";

/// View entry describing an uploaded or desktop Spotify playlist folder file.
#[non_exhaustive]
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct CachedFolderFileView {
    /// Full filesystem path to the `.file` blob.
    pub path: String,
    /// Base filename (`basename`) of the `.file` blob.
    pub filename: String,
    /// Human-readable size (`"{n} Bytes"` if `< 3200`, else `"{n/1024} KB"`).
    pub size_display: String,
    /// File modification time in Unix epoch seconds.
    pub mtime_epoch_secs: u64,
}

impl CachedFolderFileView {
    /// Creates a [`CachedFolderFileView`] from path metadata.
    #[must_use]
    pub fn from_metadata(path: &Path, size_bytes: u64, mtime_epoch_secs: u64) -> Self {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        Self {
            path: path.to_string_lossy().into_owned(),
            filename,
            size_display: format_file_size_kb(size_bytes),
            mtime_epoch_secs,
        }
    }
}

/// View model for `Settings/PlaylistFolders.pm` (`playlist-folders.html`).
#[non_exhaustive]
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct PlaylistFoldersViewModel {
    /// Manually uploaded `playlistFolders` files sorted newest-first.
    #[serde(rename = "cacheFiles")]
    pub cache_files: Vec<CachedFolderFileView>,
    /// Desktop Spotify `Storage/*.file` candidates outside `upload_dir`.
    #[serde(rename = "spotifyFiles")]
    pub spotify_files: Vec<CachedFolderFileView>,
    /// Configured or auto-detected desktop Spotify `Storage` path, if known.
    #[serde(rename = "spotifyCacheFolder")]
    pub spotify_cache_folder: Option<String>,
}

/// Controller for playlist folder cache enumeration, deletion, and purging.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct PlaylistFoldersSettingsController {
    upload_dir: PathBuf,
    spotify_storage_dir: Option<PathBuf>,
    cache: Arc<DualCache>,
}

impl PlaylistFoldersSettingsController {
    /// Creates a [`PlaylistFoldersSettingsController`] with explicit paths.
    #[must_use]
    pub fn new(
        upload_dir: impl Into<PathBuf>,
        spotify_storage_dir: Option<PathBuf>,
        cache: Arc<DualCache>,
    ) -> Self {
        Self {
            upload_dir: upload_dir.into(),
            spotify_storage_dir,
            cache,
        }
    }

    /// Creates a controller targeting `<spotty_dir>/playlistFolders` and the
    /// platform-default desktop Spotify `Storage` directory.
    #[must_use]
    pub fn with_default_storage(spotty_dir: &Path, cache: Arc<DualCache>) -> Self {
        Self::new(
            spotty_dir.join("playlistFolders"),
            default_spotify_storage_dir(),
            cache,
        )
    }

    /// Returns the `<spotty_dir>/playlistFolders` upload cache directory.
    #[must_use]
    pub fn upload_dir(&self) -> &Path {
        &self.upload_dir
    }

    /// Returns the desktop Spotify `Storage` directory if configured/detected.
    #[must_use]
    pub fn spotify_storage_dir(&self) -> Option<&Path> {
        self.spotify_storage_dir.as_deref()
    }

    /// Enumerates uploaded `playlistFolders` files and desktop `spotifyFiles`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] or [`SpottyError::Cache`] if scanning the
    /// upload directory, reading file metadata, or querying [`DualCache`] fails.
    pub fn load_view(&self) -> Result<PlaylistFoldersViewModel> {
        let mut cache_files = enumerate_uploaded_files(&self.upload_dir)?;
        cache_files.sort_by(|a, b| {
            b.mtime_epoch_secs
                .cmp(&a.mtime_epoch_secs)
                .then_with(|| a.filename.cmp(&b.filename))
        });

        let canonical_upload = fs::canonicalize(&self.upload_dir).ok();
        let mut spotify_files = Vec::new();
        if let Some(storage_dir) = self.spotify_storage_dir.as_deref() {
            let candidates = FolderDiscovery::scan_candidate_files(
                Path::new(""),
                Some(storage_dir),
                &self.cache,
            )?;
            for candidate in candidates {
                let is_inside_upload = match (&canonical_upload, fs::canonicalize(&candidate)) {
                    (Some(canon_up), Ok(canon_cand)) => canon_cand.starts_with(canon_up),
                    _ => candidate.starts_with(&self.upload_dir),
                };
                if is_inside_upload {
                    continue;
                }
                let Ok(meta) = fs::metadata(&candidate) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let mtime = modified_epoch_secs(&meta);
                spotify_files.push(CachedFolderFileView::from_metadata(
                    &candidate,
                    meta.len(),
                    mtime,
                ));
            }
            spotify_files.sort_by(|a, b| {
                b.mtime_epoch_secs
                    .cmp(&a.mtime_epoch_secs)
                    .then_with(|| a.filename.cmp(&b.filename))
            });
        }

        let spotify_cache_folder = self
            .spotify_storage_dir
            .as_ref()
            .map(|dir| dir.to_string_lossy().into_owned());

        Ok(PlaylistFoldersViewModel {
            cache_files,
            spotify_files,
            spotify_cache_folder,
        })
    }

    /// Deletes a single uploaded file by `filename` inside `upload_dir`.
    ///
    /// Returns `Ok(true)` if the file existed and was deleted, or `Ok(false)`
    /// if the file was already absent.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if `filename` attempts path traversal or
    /// filesystem removal fails.
    pub fn delete_uploaded_file(&self, filename: &str) -> Result<bool> {
        validate_single_filename(filename)?;
        let target = self.upload_dir.join(filename);
        match fs::remove_file(&target) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(SpottyError::Io(err.to_string())),
        }
    }

    /// Deletes uploaded `.file` entries matching submitted `delete_<name>.file`
    /// form parameters (`Settings/PlaylistFolders.pm:34-38`).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if any matched filename is invalid or
    /// removal fails.
    pub fn delete_from_params(&self, params: &HashMap<String, String>) -> Result<usize> {
        let mut deleted = 0usize;
        for (key, value) in params {
            let Some(filename) = key.strip_prefix(DELETE_PARAM_PREFIX) else {
                continue;
            };
            if !filename.ends_with(FOLDER_FILE_EXT) || !is_truthy_param(value) {
                continue;
            }
            if self.delete_uploaded_file(filename)? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Purges every file inside the uploaded `playlistFolders` directory.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Io`] if directory traversal or file removal fails.
    pub fn purge_upload_cache(&self) -> Result<usize> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |dur| dur.as_secs());
        FolderUploadManager::purge_folder_cache(&self.upload_dir, PurgeMode::DeleteAll, now_secs)
    }
}

fn enumerate_uploaded_files(upload_dir: &Path) -> Result<Vec<CachedFolderFileView>> {
    if !upload_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry_res in fs::read_dir(upload_dir)? {
        let entry = entry_res?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = modified_epoch_secs(&meta);
        files.push(CachedFolderFileView::from_metadata(
            &path,
            meta.len(),
            mtime,
        ));
    }
    Ok(files)
}

fn validate_single_filename(filename: &str) -> Result<()> {
    let trimmed = filename.trim();
    if trimmed.is_empty() || trimmed != filename {
        return Err(SpottyError::Io(format!(
            "invalid folder cache filename: {filename}"
        )));
    }
    let mut components = Path::new(filename).components();
    let Some(Component::Normal(single)) = components.next() else {
        return Err(SpottyError::Io(format!(
            "invalid folder cache filename: {filename}"
        )));
    };
    if components.next().is_some() || single != filename {
        return Err(SpottyError::Io(format!(
            "path traversal rejected for folder cache filename: {filename}"
        )));
    }
    Ok(())
}

fn modified_epoch_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |dur| dur.as_secs())
}

fn is_truthy_param(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    fn test_cache(dir: &Path) -> Arc<DualCache> {
        Arc::new(DualCache::open(dir.join("cache.redb")).expect("DualCache open must succeed"))
    }

    fn write_sized_file(path: &Path, len: usize, prefix: &[u8], mtime_secs: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir creation must succeed");
        }
        let mut buf = vec![b' '; len];
        let copy_len = prefix.len().min(len);
        buf[..copy_len].copy_from_slice(&prefix[..copy_len]);
        fs::write(path, &buf).expect("fixture file write must succeed");

        let file = fs::File::options()
            .write(true)
            .open(path)
            .expect("fixture file open must succeed");
        let times = FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(mtime_secs));
        file.set_times(times)
            .expect("fixture mtime update must succeed");
    }

    #[test]
    fn load_view_enumerates_uploaded_and_desktop_files_with_kb_formatting() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let spotty_dir = tmp.path().join("spotty");
        let upload_dir = spotty_dir.join("playlistFolders");
        let storage_dir = tmp.path().join("Spotify").join("Storage");
        let cache = test_cache(tmp.path());

        let small_upload = upload_dir.join("small.file");
        let large_upload = upload_dir.join("large.file");
        let desktop_candidate = storage_dir.join("ab").join("desktop.file");
        let desktop_noise = storage_dir.join("ab").join("noise.file");

        write_sized_file(
            &small_upload,
            3199,
            b"spotify:start-group:f1:Small\r",
            1_700_000_100,
        );
        write_sized_file(
            &large_upload,
            3200,
            b"spotify:start-group:f2:Large\r",
            1_700_000_200,
        );
        write_sized_file(
            &desktop_candidate,
            4096,
            b"spotify:start-group:f3:Desktop\r",
            1_700_000_300,
        );
        write_sized_file(&desktop_noise, 512, b"unrelated-cache-blob", 1_700_000_400);

        let controller =
            PlaylistFoldersSettingsController::new(&upload_dir, Some(storage_dir.clone()), cache);
        let view = controller.load_view().expect("load_view must succeed");

        assert_eq!(view.cache_files.len(), 2);
        assert_eq!(view.cache_files[0].filename, "large.file");
        assert_eq!(view.cache_files[0].size_display, "3 KB");
        assert_eq!(view.cache_files[0].mtime_epoch_secs, 1_700_000_200);

        assert_eq!(view.cache_files[1].filename, "small.file");
        assert_eq!(view.cache_files[1].size_display, "3199 Bytes");
        assert_eq!(view.cache_files[1].mtime_epoch_secs, 1_700_000_100);

        assert_eq!(view.spotify_files.len(), 1);
        assert_eq!(view.spotify_files[0].filename, "desktop.file");
        assert_eq!(view.spotify_files[0].size_display, "4 KB");
        assert_eq!(
            view.spotify_cache_folder.as_deref(),
            Some(storage_dir.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn deletes_individual_file_rejects_traversal_and_purges_cache() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let upload_dir = tmp.path().join("playlistFolders");
        let cache = test_cache(tmp.path());

        let file_a = upload_dir.join("a.file");
        let file_b = upload_dir.join("b.file");
        write_sized_file(&file_a, 128, b"spotify:start-group:a:A\r", 1_700_000_010);
        write_sized_file(&file_b, 256, b"spotify:start-group:b:B\r", 1_700_000_020);

        let controller = PlaylistFoldersSettingsController::new(&upload_dir, None, cache);

        let traversal_err = controller
            .delete_uploaded_file("../secret.file")
            .expect_err("path traversal must be rejected");
        assert!(matches!(traversal_err, SpottyError::Io(_)));

        let mut params = HashMap::new();
        params.insert(String::from("delete_a.file"), String::from("1"));
        params.insert(String::from("delete_ignored.txt"), String::from("1"));
        let deleted = controller
            .delete_from_params(&params)
            .expect("delete_from_params must succeed");
        assert_eq!(deleted, 1);
        assert!(!file_a.exists());
        assert!(file_b.exists());

        let purged = controller
            .purge_upload_cache()
            .expect("purge_upload_cache must succeed");
        assert_eq!(purged, 1);
        assert!(!file_b.exists());
    }

    #[test]
    fn load_view_skips_upload_dir_candidates_even_with_non_canonical_paths() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let upload_dir = tmp.path().join("playlistFolders");
        let storage_dir = tmp.path().join("Storage");
        let cache = test_cache(tmp.path());

        let uploaded = upload_dir.join("uploaded.file");
        write_sized_file(
            &uploaded,
            512,
            b"spotify:start-group:u1:Up\r",
            1_700_000_100,
        );
        let desktop = storage_dir.join("desktop.file");
        write_sized_file(
            &desktop,
            512,
            b"spotify:start-group:d1:Desk\r",
            1_700_000_200,
        );

        let non_canonical_upload = tmp
            .path()
            .join("Storage")
            .join("..")
            .join("playlistFolders");
        let controller =
            PlaylistFoldersSettingsController::new(non_canonical_upload, Some(storage_dir), cache);
        let view = controller.load_view().expect("load_view must succeed");
        assert_eq!(view.cache_files.len(), 1);
        assert_eq!(view.spotify_files.len(), 1);
        assert_eq!(view.spotify_files[0].filename, "desktop.file");
    }
}
