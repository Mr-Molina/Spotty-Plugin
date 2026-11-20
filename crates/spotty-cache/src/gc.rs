//! Temporary `librespot` audio cache garbage collection and track counter.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use spotty_types::Result;

/// Number of played tracks between automatic temporary audio cache sweeps.
pub const CACHE_PURGE_INTERVAL_COUNT: usize = 15;

/// Default maximum age (`3600s` / 1 hour) before `.tmpXXXXXX` files are swept.
pub const DEFAULT_TEMP_MAX_AGE: Duration = Duration::from_secs(3600);

/// Sweeps stale `.tmp[a-z0-9]{6}` files left by `librespot` on track skips.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempAudioGc {
    max_age: Duration,
}

impl TempAudioGc {
    /// Creates a [`TempAudioGc`] deleting matching files older than `max_age`.
    #[must_use]
    pub fn new(max_age: Duration) -> Self {
        Self { max_age }
    }

    /// Returns the configured maximum age threshold.
    #[must_use]
    pub fn max_age(&self) -> Duration {
        self.max_age
    }

    /// Returns `true` when `name` matches `.tmp[a-z0-9]{6}` (case-insensitive).
    #[must_use]
    pub fn is_temp_audio_filename(name: &str) -> bool {
        if name.len() != 10 {
            return false;
        }
        let Some(prefix) = name.get(..4) else {
            return false;
        };
        let Some(suffix) = name.get(4..) else {
            return false;
        };
        prefix.eq_ignore_ascii_case(".tmp") && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
    }

    /// Sweeps all matching stale `.tmpXXXXXX` files in `dirs`, returning the
    /// number of deleted files.
    ///
    /// Non-existent directories in `dirs` are skipped without error.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError::Io`] if reading directory entries,
    /// inspecting file metadata, or deleting a stale file fails.
    pub fn sweep<I, P>(&self, dirs: I) -> Result<usize>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let now = SystemTime::now();
        let mut deleted = 0usize;

        for dir in dirs {
            let dir_path = dir.as_ref();
            if !dir_path.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(dir_path) {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };

            for entry_res in entries {
                let entry = entry_res?;
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                if !Self::is_temp_audio_filename(name) {
                    continue;
                }
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(meta) => meta,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err.into()),
                };
                if !metadata.is_file() {
                    continue;
                }

                let modified = metadata.modified()?;
                let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
                if self.max_age.is_zero() || age > self.max_age {
                    match fs::remove_file(&path) {
                        Ok(()) => deleted += 1,
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }

        Ok(deleted)
    }
}

impl Default for TempAudioGc {
    fn default() -> Self {
        Self::new(DEFAULT_TEMP_MAX_AGE)
    }
}

/// Counts played tracks and signals when a temporary audio cache sweep is due.
#[derive(Debug)]
pub struct TrackCounter {
    count: AtomicUsize,
    threshold: usize,
}

impl TrackCounter {
    /// Creates a [`TrackCounter`] triggering every `threshold` tracks.
    ///
    /// A `threshold` of `0` is clamped to `1`.
    #[must_use]
    pub fn new(threshold: usize) -> Self {
        Self {
            count: AtomicUsize::new(0),
            threshold: threshold.max(1),
        }
    }

    /// Returns the configured trigger threshold.
    #[must_use]
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Returns the current number of tracks played since the last trigger.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Resets the track counter to zero.
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
    }

    /// Increments the played track count and returns `true` when `threshold` is
    /// reached, resetting the counter to zero on trigger.
    pub fn record_track(&self) -> bool {
        let mut triggered = false;
        let _ = self
            .count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                let next = cur.saturating_add(1);
                if next >= self.threshold {
                    triggered = true;
                    Some(0)
                } else {
                    triggered = false;
                    Some(next)
                }
            });
        triggered
    }
}

impl Default for TrackCounter {
    fn default() -> Self {
        Self::new(CACHE_PURGE_INTERVAL_COUNT)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::thread;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn temp_audio_filename_matching_parity() {
        assert!(TempAudioGc::is_temp_audio_filename(".tmpa1b2c3"));
        assert!(TempAudioGc::is_temp_audio_filename(".TMPA1B2C3"));
        assert!(TempAudioGc::is_temp_audio_filename(".tmp000000"));
        assert!(!TempAudioGc::is_temp_audio_filename(".tmp12345"));
        assert!(!TempAudioGc::is_temp_audio_filename(".tmp1234567"));
        assert!(!TempAudioGc::is_temp_audio_filename(".tmpabc_ef"));
        assert!(!TempAudioGc::is_temp_audio_filename("credentials.json"));
    }

    #[test]
    fn temp_audio_gc_sweeps_only_expired_matching_files() {
        let dir = TempDir::new().expect("temp dir should be created");
        let stale_path = dir.path().join(".tmpabc123");
        let unrelated_path = dir.path().join("keepme.ogg");
        let subdir_path = dir.path().join(".tmpdir999");

        fs::write(&stale_path, b"stale").expect("stale file should be written");
        fs::write(&unrelated_path, b"keep").expect("unrelated file should be written");
        fs::create_dir_all(&subdir_path).expect("subdir should be created");

        // Fresh sweep with default 3600s max_age must leave newly created file alone.
        let default_gc = TempAudioGc::default();
        assert_eq!(default_gc.max_age(), Duration::from_secs(3600));
        let fresh_deleted = default_gc
            .sweep([dir.path()])
            .expect("fresh sweep should succeed");
        assert_eq!(fresh_deleted, 0);
        assert!(stale_path.exists());

        // Wait briefly, then sweep with a short max_age so only `.tmpabc123` is swept.
        thread::sleep(Duration::from_millis(30));
        let short_gc = TempAudioGc::new(Duration::from_millis(10));
        let swept = short_gc
            .sweep([dir.path(), &dir.path().join("nonexistent")])
            .expect("short sweep should succeed");
        assert_eq!(swept, 1);
        assert!(!stale_path.exists(), "expired .tmp file must be deleted");
        assert!(unrelated_path.exists(), "non-.tmp file must remain");
        assert!(subdir_path.exists(), "directories must not be deleted");
    }

    #[test]
    fn track_counter_triggers_every_15_tracks() {
        let counter = TrackCounter::default();
        assert_eq!(counter.threshold(), CACHE_PURGE_INTERVAL_COUNT);
        assert_eq!(counter.count(), 0);

        for i in 1..CACHE_PURGE_INTERVAL_COUNT {
            assert!(
                !counter.record_track(),
                "counter must not trigger on track {i}"
            );
            assert_eq!(counter.count(), i);
        }
        assert!(
            counter.record_track(),
            "counter must trigger on track {CACHE_PURGE_INTERVAL_COUNT}"
        );
        assert_eq!(counter.count(), 0, "counter must reset after trigger");
    }

    #[test]
    fn track_counter_concurrent_record_track_triggers_exact_count() {
        use std::sync::Arc;

        let counter = Arc::new(TrackCounter::new(15));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&counter);
                thread::spawn(move || {
                    let mut triggers = 0usize;
                    for _ in 0..150 {
                        if c.record_track() {
                            triggers += 1;
                        }
                    }
                    triggers
                })
            })
            .collect();

        let total_triggers: usize = threads
            .into_iter()
            .map(|h| h.join().expect("worker thread must not panic"))
            .sum();
        assert_eq!(total_triggers, 80);
        assert_eq!(counter.count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn temp_audio_gc_ignores_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().expect("temp dir");
        let target_dir = TempDir::new().expect("target dir");
        let target_file = target_dir.path().join("target.ogg");
        fs::write(&target_file, b"keep").expect("write target");

        let link_path = dir.path().join(".tmp111111");
        symlink(&target_file, &link_path).expect("create symlink");

        let gc = TempAudioGc::new(Duration::ZERO);
        let swept = gc.sweep([dir.path()]).expect("sweep");
        assert_eq!(swept, 0);
        assert!(target_file.exists());
    }
}
