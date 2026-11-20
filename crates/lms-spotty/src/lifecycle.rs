//! Periodic background maintenance supervisor for temp audio garbage
//! collection and expired cache pruning.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spotty_cache::{DualCache, TempAudioGc, DEFAULT_TEMP_MAX_AGE};
use spotty_types::Result;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Default background maintenance interval (hourly idle cleanup).
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Configuration for [`LifecycleSupervisor`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    /// Base cache directory containing `spotty/` and temporary audio dirs.
    pub cache_dir: PathBuf,
    /// Interval between periodic maintenance sweeps.
    pub sweep_interval: Duration,
    /// Maximum age for stale temporary audio files before deletion.
    pub temp_max_age: Duration,
}

impl LifecycleConfig {
    /// Creates a [`LifecycleConfig`] with hourly sweep and 1-hour temp audio
    /// max age.
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            temp_max_age: DEFAULT_TEMP_MAX_AGE,
        }
    }

    /// Overrides the periodic sweep interval.
    #[must_use]
    pub fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    /// Overrides the maximum temp audio file age.
    #[must_use]
    pub fn with_temp_max_age(mut self, max_age: Duration) -> Self {
        self.temp_max_age = max_age;
        self
    }
}

/// Summary of a single background maintenance pass.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Number of expired temporary audio files removed.
    pub removed_temp_files: usize,
    /// Number of expired metadata/HTTP cache entries pruned.
    pub pruned_cache_entries: usize,
}

/// Background maintenance supervisor with clean shutdown signalling.
#[derive(Debug)]
pub struct LifecycleSupervisor {
    shutdown_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl LifecycleSupervisor {
    /// Spawns the periodic background maintenance task.
    #[must_use]
    pub fn spawn(config: LifecycleConfig, cache: Option<Arc<DualCache>>) -> Self {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let gc = TempAudioGc::new(config.temp_max_age);
        let cache_dir = config.cache_dir;
        let interval_dur = config.sweep_interval.max(Duration::from_millis(10));

        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval_dur);
            // Skip immediate tick so startup stays non-blocking.
            ticker.tick().await;
            loop {
                tokio::select! {
                    biased;
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        match run_sweep(&cache_dir, &gc, cache.as_deref()) {
                            Ok(report) => {
                                debug!(
                                    removed = report.removed_temp_files,
                                    pruned = report.pruned_cache_entries,
                                    "Lifecycle maintenance sweep complete"
                                );
                            }
                            Err(err) => {
                                warn!(
                                    error = %err,
                                    "Lifecycle maintenance sweep failed"
                                );
                            }
                        }
                    }
                }
            }
        });

        Self {
            shutdown_tx,
            task: Some(task),
        }
    }

    /// Executes a single temp audio GC and cache pruning pass synchronously.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if directory traversal or cache
    /// pruning fails.
    pub fn run_once(
        cache_dir: &Path,
        temp_max_age: Duration,
        cache: Option<&DualCache>,
    ) -> Result<SweepReport> {
        let gc = TempAudioGc::new(temp_max_age);
        run_sweep(cache_dir, &gc, cache)
    }

    /// Signals the background loop to exit without blocking.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Signals shutdown and awaits background task completion.
    pub async fn shutdown_and_wait(mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.task.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for LifecycleSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

fn run_sweep(cache_dir: &Path, gc: &TempAudioGc, cache: Option<&DualCache>) -> Result<SweepReport> {
    let spotty_dir = cache_dir.join("spotty");
    let sweep_dirs = [cache_dir.to_path_buf(), spotty_dir];
    let removed_temp_files = gc.sweep(&sweep_dirs)?;
    let pruned_cache_entries = match cache {
        Some(dual) => dual.prune_expired()?,
        None => 0,
    };
    Ok(SweepReport {
        removed_temp_files,
        pruned_cache_entries,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn supervisor_shuts_down_cleanly() {
        let temp = TempDir::new().expect("tempdir must succeed");
        let cfg = LifecycleConfig::new(temp.path())
            .with_sweep_interval(Duration::from_millis(20))
            .with_temp_max_age(Duration::from_secs(0));

        let temp_audio = temp.path().join("spotty");
        fs::create_dir_all(&temp_audio).expect("create spotty dir");
        fs::write(temp_audio.join(".tmpabc123"), b"stale").expect("write temp file");

        let supervisor = LifecycleSupervisor::spawn(cfg, None);
        tokio::time::sleep(Duration::from_millis(60)).await;
        supervisor.shutdown_and_wait().await;
    }

    #[test]
    fn run_once_sweeps_expired_temp_files_and_cache() {
        let temp = TempDir::new().expect("tempdir must succeed");
        let spotty_dir = temp.path().join("spotty");
        fs::create_dir_all(&spotty_dir).expect("create spotty dir");
        let tmp_file = spotty_dir.join(".tmpabc123");
        fs::write(&tmp_file, b"stale audio").expect("write tmp file");

        let cache = DualCache::open(spotty_dir.join("cache.redb")).expect("open cache");
        let report =
            LifecycleSupervisor::run_once(temp.path(), Duration::from_secs(0), Some(&cache))
                .expect("run_once must succeed");
        assert!(report.removed_temp_files >= 1);
        assert!(!tmp_file.exists());
    }
}
