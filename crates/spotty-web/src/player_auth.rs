//! Zeroconf player authentication supervisor (`Settings/PlayerAuth.pm`).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use spotty_audio::{
    DiscoveryBackend, LibrespotDiscoveryBackend, ZeroconfAuthenticator, ZEROCONF_AUTH_TIMEOUT_SECS,
};
use spotty_cache::AccountStore;
use spotty_types::{ApFallbackPolicy, Result, SpottyError};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{info, warn};

/// JSON payload returned by `plugins/Spotty/settings/hasPlayerCredentials`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlayerAuthCheckJson {
    /// Whether `<spotty_dir>/credentials.json` is present on disk.
    #[serde(rename = "hasPlayerCredentials")]
    pub has_player_credentials: bool,
}

impl PlayerAuthCheckJson {
    /// Creates a [`PlayerAuthCheckJson`] payload.
    #[must_use]
    pub fn new(has_player_credentials: bool) -> Self {
        Self {
            has_player_credentials,
        }
    }
}

/// Checks whether Zeroconf player credentials have been persisted.
#[must_use]
pub fn check_player_credentials(store: &AccountStore) -> PlayerAuthCheckJson {
    PlayerAuthCheckJson {
        has_player_credentials: store.has_player_credentials(),
    }
}

/// Page routing outcome for `Settings/PlayerAuth.pm:handler`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerAuthPageOutcome {
    /// Credentials already exist; supervisor shut down and redirect to
    /// `basic.html`.
    RedirectBasicHtml,
    /// Credentials missing; Zeroconf listener is active on `player-auth.html`.
    AwaitingCredentials,
}

#[derive(Debug)]
struct ActiveAuthTask {
    cancel_tx: Option<oneshot::Sender<()>>,
    abort_handle: AbortHandle,
    join_handle: JoinHandle<()>,
}

/// Supervises a single abortable background [`ZeroconfAuthenticator`] task.
///
/// Mirrors `Plugins::Spotty::Settings::PlayerAuth`: enforces the 15-minute
/// [`ZEROCONF_AUTH_TIMEOUT_SECS`] timeout and cleanly cancels the background
/// task when [`PlayerAuthSupervisor::shutdown`] is invoked (for example on
/// returning to `basic.html`) or when the supervisor is dropped.
#[derive(Debug)]
pub struct PlayerAuthSupervisor<B = LibrespotDiscoveryBackend> {
    authenticator: Arc<ZeroconfAuthenticator<B>>,
    timeout: Duration,
    active: Mutex<Option<ActiveAuthTask>>,
}

impl PlayerAuthSupervisor<LibrespotDiscoveryBackend> {
    /// Creates a [`PlayerAuthSupervisor`] backed by
    /// [`LibrespotDiscoveryBackend`].
    #[must_use]
    pub fn with_librespot() -> Self {
        Self::new(LibrespotDiscoveryBackend::new())
    }
}

impl Default for PlayerAuthSupervisor<LibrespotDiscoveryBackend> {
    fn default() -> Self {
        Self::with_librespot()
    }
}

impl<B: DiscoveryBackend + 'static> PlayerAuthSupervisor<B> {
    /// Creates a [`PlayerAuthSupervisor`] with a custom [`DiscoveryBackend`]
    /// and the default 15-minute [`ZEROCONF_AUTH_TIMEOUT_SECS`] timeout.
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self {
            authenticator: Arc::new(ZeroconfAuthenticator::new(backend)),
            timeout: Duration::from_secs(ZEROCONF_AUTH_TIMEOUT_SECS),
            active: Mutex::new(None),
        }
    }

    /// Overrides the Zeroconf authentication timeout (default: `900s`).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Returns the configured Zeroconf authentication timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns `true` if a background Zeroconf task is currently running.
    ///
    /// # Panics
    ///
    /// Panics if the internal supervisor mutex was poisoned by a prior panic.
    #[must_use]
    pub fn is_running(&self) -> bool {
        let mut guard = self
            .active
            .lock()
            .expect("player auth supervisor mutex is never poisoned");
        let Some(task) = guard.as_ref() else {
            return false;
        };
        if task.join_handle.is_finished() {
            *guard = None;
            return false;
        }
        true
    }

    /// Starts the single background [`ZeroconfAuthenticator`] task if not
    /// already running.
    ///
    /// Returns `Ok(true)` if a new task was spawned, or `Ok(false)` if an
    /// active task is already running.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `device_name` or `client_id` is empty.
    ///
    /// # Panics
    ///
    /// Panics if the internal supervisor mutex was poisoned by a prior panic.
    pub fn start(
        &self,
        spotty_dir: &Path,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
    ) -> Result<bool> {
        if device_name.trim().is_empty() {
            return Err(SpottyError::Audio(String::from(
                "Zeroconf device name cannot be empty",
            )));
        }
        if client_id.trim().is_empty() {
            return Err(SpottyError::Audio(String::from(
                "Spotify client_id cannot be empty",
            )));
        }

        let mut guard = self
            .active
            .lock()
            .expect("player auth supervisor mutex is never poisoned");
        if let Some(existing) = guard.as_ref() {
            if !existing.join_handle.is_finished() {
                return Ok(false);
            }
            *guard = None;
        }

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let auth = Arc::clone(&self.authenticator);
        let dir = spotty_dir.to_path_buf();
        let dev = device_name.to_owned();
        let cid = client_id.to_owned();
        let timeout = self.timeout;

        let join_handle = tokio::spawn(async move {
            let outcome = auth
                .authenticate_with_cancellation(&dir, &dev, &cid, ap_fallback, timeout, cancel_rx)
                .await;
            match outcome {
                Ok(record) => {
                    info!(
                        username = %record.username,
                        "Zeroconf player authentication succeeded"
                    );
                }
                Err(err) => {
                    warn!(error = %err, "Zeroconf player authentication ended");
                }
            }
        });
        let abort_handle = join_handle.abort_handle();

        *guard = Some(ActiveAuthTask {
            cancel_tx: Some(cancel_tx),
            abort_handle,
            join_handle,
        });
        Ok(true)
    }

    /// Cancels and aborts any running Zeroconf authentication task.
    ///
    /// # Panics
    ///
    /// Panics if the internal supervisor mutex was poisoned by a prior panic.
    pub fn shutdown(&self) {
        let mut guard = self
            .active
            .lock()
            .expect("player auth supervisor mutex is never poisoned");
        if let Some(mut task) = guard.take() {
            if let Some(tx) = task.cancel_tx.take() {
                let _ = tx.send(());
            }
            task.abort_handle.abort();
        }
    }

    /// Cancels the running task via its oneshot signal and awaits completion.
    ///
    /// # Panics
    ///
    /// Panics if the internal supervisor mutex was poisoned by a prior panic.
    pub async fn shutdown_and_wait(&self) {
        let maybe_task = {
            let mut guard = self
                .active
                .lock()
                .expect("player auth supervisor mutex is never poisoned");
            guard.take()
        };
        if let Some(mut task) = maybe_task {
            if let Some(tx) = task.cancel_tx.take() {
                let _ = tx.send(());
            }
            let _ = task.join_handle.await;
        }
    }

    /// Evaluates `player-auth.html` page state: shuts down and redirects to
    /// `basic.html` once credentials exist, or starts the Zeroconf listener.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if starting the listener fails validation.
    pub fn handle_page(
        &self,
        store: &AccountStore,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
    ) -> Result<PlayerAuthPageOutcome> {
        if store.has_player_credentials() {
            self.shutdown();
            return Ok(PlayerAuthPageOutcome::RedirectBasicHtml);
        }
        let _ = self.start(store.spotty_dir(), device_name, client_id, ap_fallback)?;
        Ok(PlayerAuthPageOutcome::AwaitingCredentials)
    }

    /// Polls credential readiness for `hasPlayerCredentials`, ensuring the
    /// Zeroconf listener is running while credentials are still absent.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if starting the listener fails validation.
    pub fn poll_status(
        &self,
        store: &AccountStore,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
    ) -> Result<PlayerAuthCheckJson> {
        let status = check_player_credentials(store);
        if status.has_player_credentials {
            self.shutdown();
        } else {
            let _ = self.start(store.spotty_dir(), device_name, client_id, ap_fallback)?;
        }
        Ok(status)
    }
}

impl<B> Drop for PlayerAuthSupervisor<B> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.active.lock() {
            if let Some(mut task) = guard.take() {
                if let Some(tx) = task.cancel_tx.take() {
                    let _ = tx.send(());
                }
                task.abort_handle.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use spotty_audio::BackendDiscoveryFuture;
    use spotty_cache::CredentialsRecord;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    enum StubMode {
        PendingForever,
        Immediate(CredentialsRecord),
    }

    #[derive(Debug)]
    struct StubDiscoveryBackend {
        mode: StubMode,
        invocations: Arc<AtomicUsize>,
    }

    impl DiscoveryBackend for StubDiscoveryBackend {
        fn discover_credentials<'a>(
            &'a self,
            _device_name: &'a str,
            _client_id: &'a str,
            _ap_fallback: ApFallbackPolicy,
        ) -> BackendDiscoveryFuture<'a> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            match &self.mode {
                StubMode::PendingForever => Box::pin(future::pending()),
                StubMode::Immediate(record) => {
                    let cloned = record.clone();
                    Box::pin(async move { Ok(cloned) })
                }
            }
        }
    }

    #[test]
    fn supervisor_defaults_to_fifteen_minute_timeout() {
        let supervisor = PlayerAuthSupervisor::with_librespot();
        assert_eq!(
            supervisor.timeout(),
            Duration::from_secs(ZEROCONF_AUTH_TIMEOUT_SECS)
        );
        assert_eq!(supervisor.timeout(), Duration::from_secs(900));
    }

    #[test]
    fn check_player_credentials_serializes_expected_json() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let store = AccountStore::new(tmp.path());

        let absent = check_player_credentials(&store);
        assert!(!absent.has_player_credentials);
        let json_absent = serde_json::to_value(absent).expect("JSON serialization must succeed");
        assert_eq!(
            json_absent,
            serde_json::json!({ "hasPlayerCredentials": false })
        );

        fs::create_dir_all(store.spotty_dir()).expect("spotty dir creation must succeed");
        fs::write(
            store.spotty_dir().join("credentials.json"),
            br#"{"username":"alice","auth_type":1,"auth_data":"blob"}"#,
        )
        .expect("credentials file write must succeed");

        let present = check_player_credentials(&store);
        assert!(present.has_player_credentials);
        let json_present = serde_json::to_value(present).expect("JSON serialization must succeed");
        assert_eq!(
            json_present,
            serde_json::json!({ "hasPlayerCredentials": true })
        );
    }

    #[tokio::test]
    async fn supervisor_starts_single_task_and_cancels_on_shutdown() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let store = AccountStore::new(tmp.path());
        let invocations = Arc::new(AtomicUsize::new(0));

        let supervisor = PlayerAuthSupervisor::new(StubDiscoveryBackend {
            mode: StubMode::PendingForever,
            invocations: Arc::clone(&invocations),
        });

        let started = supervisor
            .start(
                store.spotty_dir(),
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::ForcePort12321,
            )
            .expect("first start must succeed");
        assert!(started, "first start must spawn a background task");
        assert!(supervisor.is_running());

        let restarted = supervisor
            .start(
                store.spotty_dir(),
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::ForcePort12321,
            )
            .expect("second start while running must succeed");
        assert!(
            !restarted,
            "supervisor must keep a single active background task"
        );

        tokio::task::yield_now().await;
        assert_eq!(invocations.load(Ordering::SeqCst), 1);

        supervisor.shutdown_and_wait().await;
        assert!(
            !supervisor.is_running(),
            "supervisor must not be running after shutdown"
        );
    }

    #[tokio::test]
    async fn handle_page_redirects_to_basic_html_and_shuts_down_when_creds_exist() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let store = AccountStore::new(tmp.path());
        let invocations = Arc::new(AtomicUsize::new(0));

        let supervisor = PlayerAuthSupervisor::new(StubDiscoveryBackend {
            mode: StubMode::PendingForever,
            invocations: Arc::clone(&invocations),
        });

        let page_before = supervisor
            .handle_page(
                &store,
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::DefaultPort,
            )
            .expect("handle_page before auth must succeed");
        assert_eq!(page_before, PlayerAuthPageOutcome::AwaitingCredentials);
        assert!(supervisor.is_running());

        fs::create_dir_all(store.spotty_dir()).expect("spotty dir creation must succeed");
        fs::write(
            store.spotty_dir().join("credentials.json"),
            br#"{"username":"alice","auth_type":1,"auth_data":"blob"}"#,
        )
        .expect("credentials write must succeed");

        let page_after = supervisor
            .handle_page(
                &store,
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::DefaultPort,
            )
            .expect("handle_page after auth must succeed");
        assert_eq!(page_after, PlayerAuthPageOutcome::RedirectBasicHtml);
        assert!(
            !supervisor.is_running(),
            "returning to basic.html must cleanly shut down the supervisor"
        );
    }

    #[tokio::test]
    async fn supervisor_persists_credentials_and_exposes_status_on_completion() {
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let store = AccountStore::new(tmp.path());
        let expected = CredentialsRecord::new(
            "zeroconf_user",
            Some(1),
            Some(String::from("c2VjcmV0LWRhdGE=")),
        );
        let supervisor = PlayerAuthSupervisor::new(StubDiscoveryBackend {
            mode: StubMode::Immediate(expected),
            invocations: Arc::new(AtomicUsize::new(0)),
        });

        let first_poll = supervisor
            .poll_status(
                &store,
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::DefaultPort,
            )
            .expect("initial poll_status must succeed");
        assert!(!first_poll.has_player_credentials);

        for _ in 0..20 {
            if !supervisor.is_running() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let second_poll = supervisor
            .poll_status(
                &store,
                "Spotty (LMS)",
                "client-id-123",
                ApFallbackPolicy::DefaultPort,
            )
            .expect("post-completion poll_status must succeed");
        assert!(second_poll.has_player_credentials);
        assert!(!supervisor.is_running());
    }
}
