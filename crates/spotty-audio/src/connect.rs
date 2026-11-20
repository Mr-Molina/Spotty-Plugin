//! Spotify Connect Zeroconf discovery authentication and OAuth credential
//! materialization.

use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use librespot_core::authentication::Credentials;
use librespot_discovery::DeviceType;
use librespot_discovery::Discovery;
use spotty_cache::CredentialsRecord;
use spotty_types::ApFallbackPolicy;
use spotty_types::Result;
use spotty_types::SpottyError;
use tokio::sync::oneshot;

const CREDENTIALS_FILE: &str = "credentials.json";
static CRED_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Default Zeroconf authentication timeout matching `PlayerAuth.pm` (15 min).
pub const ZEROCONF_AUTH_TIMEOUT_SECS: u64 = 900;

/// Boxed `Send` future returned by [`DiscoveryBackend::discover_credentials`].
pub type BackendDiscoveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CredentialsRecord>> + Send + 'a>>;

/// Abstraction over Spotify Connect Zeroconf discovery (`librespot-discovery`
/// or a test double).
pub trait DiscoveryBackend: Send + Sync {
    /// Advertises the Spotify Connect receiver and awaits the first
    /// authenticated [`CredentialsRecord`].
    ///
    /// Note: `ap_fallback` is accepted for LMS `PlayerAuth.pm` CLI parity;
    /// `librespot-discovery` only advertises local mDNS/HTTP Zeroconf and does
    /// not open a Spotify AP connection during discovery.
    fn discover_credentials<'a>(
        &'a self,
        device_name: &'a str,
        client_id: &'a str,
        ap_fallback: ApFallbackPolicy,
    ) -> BackendDiscoveryFuture<'a>;

    /// Variant of [`Self::discover_credentials`] accepting an explicit
    /// Zeroconf HTTP listener port (`--zeroconf-port`).
    fn discover_credentials_with_port<'a>(
        &'a self,
        device_name: &'a str,
        client_id: &'a str,
        ap_fallback: ApFallbackPolicy,
        zeroconf_port: Option<u16>,
    ) -> BackendDiscoveryFuture<'a> {
        let _ = zeroconf_port;
        self.discover_credentials(device_name, client_id, ap_fallback)
    }
}

/// Production [`DiscoveryBackend`] backed by pure-Rust `librespot-discovery`
/// with the `libmdns` responder.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
pub struct LibrespotDiscoveryBackend;

impl LibrespotDiscoveryBackend {
    /// Creates a new [`LibrespotDiscoveryBackend`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl DiscoveryBackend for LibrespotDiscoveryBackend {
    fn discover_credentials<'a>(
        &'a self,
        device_name: &'a str,
        client_id: &'a str,
        ap_fallback: ApFallbackPolicy,
    ) -> BackendDiscoveryFuture<'a> {
        self.discover_credentials_with_port(device_name, client_id, ap_fallback, None)
    }

    fn discover_credentials_with_port<'a>(
        &'a self,
        device_name: &'a str,
        client_id: &'a str,
        ap_fallback: ApFallbackPolicy,
        zeroconf_port: Option<u16>,
    ) -> BackendDiscoveryFuture<'a> {
        Box::pin(discover_via_librespot(
            device_name,
            client_id,
            ap_fallback,
            zeroconf_port,
        ))
    }
}

/// Coordinates Spotify Connect Zeroconf authentication with timeout and
/// cancellation support.
#[derive(Debug)]
pub struct ZeroconfAuthenticator<B = LibrespotDiscoveryBackend> {
    backend: B,
}

impl ZeroconfAuthenticator<LibrespotDiscoveryBackend> {
    /// Creates a [`ZeroconfAuthenticator`] backed by [`LibrespotDiscoveryBackend`].
    #[must_use]
    pub fn with_librespot() -> Self {
        Self::new(LibrespotDiscoveryBackend::new())
    }
}

impl Default for ZeroconfAuthenticator<LibrespotDiscoveryBackend> {
    fn default() -> Self {
        Self::with_librespot()
    }
}

impl<B: DiscoveryBackend> ZeroconfAuthenticator<B> {
    /// Creates a [`ZeroconfAuthenticator`] backed by a custom
    /// [`DiscoveryBackend`].
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Returns a shared reference to the underlying [`DiscoveryBackend`].
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Runs Zeroconf discovery until credentials are received or `timeout`
    /// elapses, persisting `credentials.json` inside `cache_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `device_name` or `client_id` is empty,
    /// discovery fails or times out, or received credentials are incomplete, or
    /// [`SpottyError::Io`] / [`SpottyError::Json`] if writing
    /// `credentials.json` fails.
    pub async fn authenticate_with_timeout(
        &self,
        cache_dir: &Path,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
        timeout: Duration,
    ) -> Result<CredentialsRecord> {
        self.authenticate_with_port(
            cache_dir,
            device_name,
            client_id,
            ap_fallback,
            None,
            timeout,
        )
        .await
    }

    /// Runs Zeroconf discovery on an optional custom `--zeroconf-port` until
    /// credentials are received or `timeout` elapses, persisting
    /// `credentials.json` inside `cache_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `device_name` or `client_id` is empty,
    /// discovery fails or times out, or received credentials are incomplete, or
    /// [`SpottyError::Io`] / [`SpottyError::Json`] if writing
    /// `credentials.json` fails.
    pub async fn authenticate_with_port(
        &self,
        cache_dir: &Path,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
        zeroconf_port: Option<u16>,
        timeout: Duration,
    ) -> Result<CredentialsRecord> {
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let result = self
            .authenticate_with_port_and_cancellation(
                cache_dir,
                device_name,
                client_id,
                ap_fallback,
                zeroconf_port,
                timeout,
                cancel_rx,
            )
            .await;
        drop(cancel_tx);
        result
    }

    /// Runs Zeroconf discovery until credentials are received, `timeout`
    /// elapses, or `cancel_rx` fires/drops, persisting `credentials.json`
    /// inside `cache_dir`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `device_name` or `client_id` is empty,
    /// discovery is cancelled, times out, or fails, or received credentials are
    /// incomplete; returns [`SpottyError::Io`] / [`SpottyError::Json`] if
    /// writing `credentials.json` fails.
    pub async fn authenticate_with_cancellation(
        &self,
        cache_dir: &Path,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
        timeout: Duration,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<CredentialsRecord> {
        self.authenticate_with_port_and_cancellation(
            cache_dir,
            device_name,
            client_id,
            ap_fallback,
            None,
            timeout,
            cancel_rx,
        )
        .await
    }

    /// Runs Zeroconf discovery on an optional `--zeroconf-port` with timeout
    /// and cancellation support, persisting `credentials.json` atomically via
    /// `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Audio`] if `device_name` or `client_id` is empty,
    /// discovery is cancelled, times out, or fails, or received credentials are
    /// incomplete; returns [`SpottyError::Io`] / [`SpottyError::Json`] if
    /// writing `credentials.json` fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn authenticate_with_port_and_cancellation(
        &self,
        cache_dir: &Path,
        device_name: &str,
        client_id: &str,
        ap_fallback: ApFallbackPolicy,
        zeroconf_port: Option<u16>,
        timeout: Duration,
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<CredentialsRecord> {
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

        let record = tokio::select! {
            biased;
            _ = cancel_rx => {
                return Err(SpottyError::Audio(String::from(
                    "Zeroconf authentication cancelled",
                )));
            }
            outcome = tokio::time::timeout(
                timeout,
                self.backend.discover_credentials_with_port(
                    device_name,
                    client_id,
                    ap_fallback,
                    zeroconf_port,
                ),
            ) => {
                match outcome {
                    Ok(inner) => inner?,
                    Err(_) => {
                        return Err(SpottyError::Audio(format!(
                            "Zeroconf authentication timed out after {}s",
                            timeout.as_secs()
                        )));
                    }
                }
            }
        };

        validate_record(&record)?;
        let dir = cache_dir.to_path_buf();
        let rec = record.clone();
        tokio::task::spawn_blocking(move || write_credentials_file(&dir, &rec))
            .await
            .map_err(|err| SpottyError::Audio(format!("credentials write task failed: {err}")))??;
        Ok(record)
    }
}

/// Materializes an OAuth PKCE access token directly into
/// `<cache_dir>/credentials.json` in-process, replacing
/// `spotty --get-token --access-token=<tok> -c <__AUTHENTICATE__>`.
///
/// # Errors
///
/// Returns [`SpottyError::NoAccessToken`] if `access_token` is empty,
/// [`SpottyError::Audio`] if `username` is empty, or [`SpottyError::Io`] /
/// [`SpottyError::Json`] if writing `credentials.json` fails.
pub fn materialize_oauth_credentials(
    cache_dir: &Path,
    access_token: &str,
    username: &str,
) -> Result<PathBuf> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(SpottyError::NoAccessToken);
    }
    let trimmed_user = username.trim();
    if trimmed_user.is_empty() {
        return Err(SpottyError::Audio(String::from(
            "Spotify username cannot be empty when materializing OAuth credentials",
        )));
    }

    let mut creds = Credentials::with_access_token(token);
    creds.username = Some(trimmed_user.to_owned());
    let record = from_librespot_credentials(&creds)?;
    validate_record(&record)?;
    write_credentials_file(cache_dir, &record)
}

struct DiscoveryShutdownGuard {
    discovery: Option<Discovery>,
}

impl Drop for DiscoveryShutdownGuard {
    fn drop(&mut self) {
        if let Some(discovery) = self.discovery.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    discovery.shutdown().await;
                });
            }
        }
    }
}

async fn discover_via_librespot(
    device_name: &str,
    client_id: &str,
    ap_fallback: ApFallbackPolicy,
    zeroconf_port: Option<u16>,
) -> Result<CredentialsRecord> {
    use futures_util::StreamExt as _;

    // `ap_fallback` is accepted for `PlayerAuth.pm` CLI parity;
    // `librespot-discovery` only serves local mDNS/HTTP Zeroconf and does not
    // connect to a Spotify AP.
    let _ = ap_fallback;
    let device_id = derive_device_id(device_name);
    let mut builder = Discovery::builder(device_id, client_id.to_owned())
        .name(device_name.to_owned())
        .device_type(DeviceType::Speaker);
    if let Some(port) = zeroconf_port {
        builder = builder.port(port);
    }
    let discovery = builder
        .launch()
        .map_err(|err| SpottyError::Audio(format!("failed to start Zeroconf discovery: {err}")))?;

    let mut guard = DiscoveryShutdownGuard {
        discovery: Some(discovery),
    };
    let maybe_creds = if let Some(disc) = guard.discovery.as_mut() {
        disc.next().await
    } else {
        None
    };
    if let Some(disc) = guard.discovery.take() {
        disc.shutdown().await;
    }

    let Some(creds) = maybe_creds else {
        return Err(SpottyError::Audio(String::from(
            "Zeroconf discovery stream ended without credentials",
        )));
    };
    from_librespot_credentials(&creds)
}

fn from_librespot_credentials(creds: &Credentials) -> Result<CredentialsRecord> {
    let json = serde_json::to_string(creds)?;
    let record: CredentialsRecord = serde_json::from_str(&json)?;
    Ok(record)
}

fn validate_record(record: &CredentialsRecord) -> Result<()> {
    let has_data = record.auth_data.as_deref().is_some_and(|s| !s.is_empty());
    if record.username.trim().is_empty() || record.auth_type.is_none() || !has_data {
        return Err(SpottyError::Audio(String::from(
            "received incomplete Spotify credentials record",
        )));
    }
    Ok(())
}

fn write_credentials_file(cache_dir: &Path, record: &CredentialsRecord) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir)?;
    let creds_path = cache_dir.join(CREDENTIALS_FILE);
    let json = serde_json::to_string_pretty(record)?;
    let seq = CRED_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = cache_dir.join(format!(
        ".{CREDENTIALS_FILE}.{}.{seq}.tmp",
        std::process::id()
    ));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);

    let write_res = (|| -> Result<()> {
        let mut file = opts.open(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp_path, &creds_path)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res?;
    Ok(creds_path)
}

fn host_fingerprint() -> String {
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        let trimmed = hostname.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Ok(etc_hostname) = fs::read_to_string("/etc/hostname") {
        let trimmed = etc_hostname.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    String::from("spotty-host")
}

pub(crate) fn derive_device_id(device_name: &str) -> String {
    let salt = host_fingerprint();
    derive_device_id_salted(device_name, &salt)
}

pub(crate) fn derive_device_id_salted(device_name: &str, salt: &str) -> String {
    const FNV_OFFSET_1: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_OFFSET_2: u64 = 0x8422_2325_cbf2_9ce4;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    let mut h1 = FNV_OFFSET_1;
    let mut h2 = FNV_OFFSET_2;
    let mut h3 = FNV_OFFSET_1 ^ 0x5555_5555_5555_5555;
    for &byte in device_name
        .as_bytes()
        .iter()
        .chain(b"\0".iter())
        .chain(salt.as_bytes().iter())
    {
        h1 ^= u64::from(byte);
        h1 = h1.wrapping_mul(FNV_PRIME);
        h2 = h2.wrapping_mul(FNV_PRIME);
        h2 ^= u64::from(byte);
        h3 = h3.rotate_left(7) ^ u64::from(byte);
    }
    format!("{h1:016x}{h2:016x}{:08x}", h3 as u32)
}

#[cfg(test)]
mod tests {
    use std::future;

    use spotty_cache::AccountId;
    use spotty_cache::AccountStore;
    use spotty_cache::AUTHENTICATE_DIR;
    use tempfile::TempDir;

    use super::*;
    use crate::streamer::load_credentials_record;

    #[derive(Debug)]
    enum MockMode {
        Immediate(CredentialsRecord),
        PendingForever,
        StreamClosed,
    }

    #[derive(Debug)]
    struct MockDiscoveryBackend {
        mode: MockMode,
    }

    impl DiscoveryBackend for MockDiscoveryBackend {
        fn discover_credentials<'a>(
            &'a self,
            _device_name: &'a str,
            _client_id: &'a str,
            _ap_fallback: ApFallbackPolicy,
        ) -> BackendDiscoveryFuture<'a> {
            match &self.mode {
                MockMode::Immediate(record) => {
                    let cloned = record.clone();
                    Box::pin(async move { Ok(cloned) })
                }
                MockMode::PendingForever => Box::pin(future::pending()),
                MockMode::StreamClosed => Box::pin(async {
                    Err(SpottyError::Audio(String::from(
                        "Zeroconf discovery stream ended without credentials",
                    )))
                }),
            }
        }
    }

    #[test]
    fn zeroconf_timeout_constant_matches_fifteen_minutes() {
        assert_eq!(
            ZEROCONF_AUTH_TIMEOUT_SECS, 900,
            "PlayerAuth.pm HELPER_TIMEOUT is 60 * 15 = 900s"
        );
    }

    #[test]
    fn materialize_oauth_credentials_writes_valid_account_store_file() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let auth_dir = tmp.path().join("spotty").join(AUTHENTICATE_DIR);

        let path = materialize_oauth_credentials(&auth_dir, "oauth-tok-xyz", "alice_spotify")
            .expect("OAuth credential materialization should succeed");
        assert_eq!(path, auth_dir.join(CREDENTIALS_FILE));
        assert!(path.is_file(), "credentials.json must exist on disk");

        let store = AccountStore::new(tmp.path());
        let accounts = store
            .all_credentials()
            .expect("AccountStore should read materialized credentials");
        assert_eq!(
            accounts,
            vec![(
                String::from("alice_spotify"),
                AccountId::parse(AUTHENTICATE_DIR).expect("__AUTHENTICATE__ must parse"),
            )]
        );

        let loaded = load_credentials_record(&auth_dir)
            .expect("streamer loader should accept materialized credentials");
        assert_eq!(loaded.username, "alice_spotify");
        assert!(
            loaded.auth_type.is_some()
                && loaded.auth_data.as_deref().is_some_and(|s| !s.is_empty()),
            "materialized credentials must include auth_type and auth_data: {loaded:?}"
        );

        let json_str = fs::read_to_string(&path).expect("credentials.json should be readable");
        let librespot_creds: Credentials = serde_json::from_str(&json_str)
            .expect("credentials.json must deserialize as librespot Credentials");
        assert_eq!(librespot_creds.username.as_deref(), Some("alice_spotify"));
        assert_eq!(librespot_creds.auth_data, b"oauth-tok-xyz");
    }

    #[test]
    fn materialize_oauth_credentials_rejects_empty_inputs() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let err_token = materialize_oauth_credentials(tmp.path(), "   ", "alice")
            .expect_err("empty token must fail");
        assert_eq!(err_token, SpottyError::NoAccessToken);

        let err_user = materialize_oauth_credentials(tmp.path(), "tok", "")
            .expect_err("empty username must fail");
        assert!(
            matches!(err_user, SpottyError::Audio(_)),
            "expected Audio error for empty username, got {err_user:?}"
        );
    }

    #[tokio::test]
    async fn zeroconf_authenticator_writes_credentials_on_success() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let spotty_dir = tmp.path().join("spotty");
        let expected = CredentialsRecord::new(
            "connect_user",
            Some(1),
            Some(String::from("c2VjcmV0LWJsb2I=")),
        );
        let auth = ZeroconfAuthenticator::new(MockDiscoveryBackend {
            mode: MockMode::Immediate(expected.clone()),
        });

        let record = auth
            .authenticate_with_timeout(
                &spotty_dir,
                "Spotty (LMS)",
                "93aac68fb06348598c1e67734dfaceee",
                ApFallbackPolicy::ForcePort12321,
                Duration::from_secs(5),
            )
            .await
            .expect("mock Zeroconf authentication should succeed");

        assert_eq!(record, expected);
        let store = AccountStore::new(tmp.path());
        assert!(
            store.has_player_credentials(),
            "AccountStore must detect player credentials.json written by ZeroconfAuthenticator"
        );
        let stored = store
            .player_credentials()
            .expect("player_credentials should parse")
            .expect("player_credentials should be present");
        assert_eq!(stored, expected);
    }

    #[tokio::test]
    async fn zeroconf_authenticator_times_out_when_backend_stalls() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let auth = ZeroconfAuthenticator::new(MockDiscoveryBackend {
            mode: MockMode::PendingForever,
        });

        let err = auth
            .authenticate_with_timeout(
                tmp.path(),
                "Spotty (LMS)",
                "93aac68fb06348598c1e67734dfaceee",
                ApFallbackPolicy::DefaultPort,
                Duration::from_millis(15),
            )
            .await
            .expect_err("stalled discovery must time out");

        assert!(
            matches!(err, SpottyError::Audio(ref msg) if msg.contains("timed out")),
            "expected timeout Audio error, got {err:?}"
        );
        assert!(
            !tmp.path().join(CREDENTIALS_FILE).exists(),
            "credentials.json must not be created on timeout"
        );
    }

    #[tokio::test]
    async fn zeroconf_authenticator_aborts_on_oneshot_cancellation() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let auth = ZeroconfAuthenticator::new(MockDiscoveryBackend {
            mode: MockMode::PendingForever,
        });
        let (cancel_tx, cancel_rx) = oneshot::channel();
        cancel_tx
            .send(())
            .expect("oneshot cancellation signal should send");

        let err = auth
            .authenticate_with_cancellation(
                tmp.path(),
                "Spotty (LMS)",
                "93aac68fb06348598c1e67734dfaceee",
                ApFallbackPolicy::DefaultPort,
                Duration::from_secs(ZEROCONF_AUTH_TIMEOUT_SECS),
                cancel_rx,
            )
            .await
            .expect_err("cancelled discovery must abort immediately");

        assert!(
            matches!(err, SpottyError::Audio(ref msg) if msg.contains("cancelled")),
            "expected cancellation Audio error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn zeroconf_authenticator_propagates_backend_stream_closure() {
        let tmp = TempDir::new().expect("temp dir should be created");
        let auth = ZeroconfAuthenticator::new(MockDiscoveryBackend {
            mode: MockMode::StreamClosed,
        });

        let err = auth
            .authenticate_with_timeout(
                tmp.path(),
                "Spotty (LMS)",
                "93aac68fb06348598c1e67734dfaceee",
                ApFallbackPolicy::DefaultPort,
                Duration::from_secs(5),
            )
            .await
            .expect_err("closed discovery stream must return an error");

        assert!(
            matches!(err, SpottyError::Audio(_)),
            "expected Audio error on stream closure, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_oauth_credentials_writes_atomic_0600_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().expect("temp dir should be created");
        let creds_path = materialize_oauth_credentials(tmp.path(), "tok_secret_123", "testuser")
            .expect("oauth credentials should materialize");
        let mode = fs::metadata(&creds_path)
            .expect("metadata should exist")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credentials.json must have 0600 permissions");

        let leftover_tmps: Vec<_> = fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover_tmps.is_empty(), "no .tmp files should remain");
    }

    #[tokio::test]
    async fn zeroconf_authenticator_plumbs_zeroconf_port_and_drops_cancelled_future() {
        use std::sync::Arc;
        use std::sync::Mutex;

        struct PortAndDropBackend {
            seen_port: Arc<Mutex<Option<Option<u16>>>>,
            dropped: Arc<Mutex<bool>>,
        }

        struct DropFlag(Arc<Mutex<bool>>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                if let Ok(mut guard) = self.0.lock() {
                    *guard = true;
                }
            }
        }

        impl DiscoveryBackend for PortAndDropBackend {
            fn discover_credentials<'a>(
                &'a self,
                _device_name: &'a str,
                _client_id: &'a str,
                _ap_fallback: ApFallbackPolicy,
            ) -> BackendDiscoveryFuture<'a> {
                Box::pin(future::pending())
            }

            fn discover_credentials_with_port<'a>(
                &'a self,
                _device_name: &'a str,
                _client_id: &'a str,
                _ap_fallback: ApFallbackPolicy,
                zeroconf_port: Option<u16>,
            ) -> BackendDiscoveryFuture<'a> {
                if let Ok(mut guard) = self.seen_port.lock() {
                    *guard = Some(zeroconf_port);
                }
                let flag = DropFlag(Arc::clone(&self.dropped));
                Box::pin(async move {
                    let _keep = flag;
                    future::pending::<Result<CredentialsRecord>>().await
                })
            }
        }

        let tmp = TempDir::new().expect("temp dir should be created");
        let seen_port = Arc::new(Mutex::new(None));
        let dropped = Arc::new(Mutex::new(false));
        let auth = ZeroconfAuthenticator::new(PortAndDropBackend {
            seen_port: Arc::clone(&seen_port),
            dropped: Arc::clone(&dropped),
        });

        let err = auth
            .authenticate_with_port(
                tmp.path(),
                "Spotty",
                "client_123",
                ApFallbackPolicy::DefaultPort,
                Some(44_100),
                Duration::from_millis(15),
            )
            .await
            .expect_err("pending discovery must time out");

        assert!(matches!(err, SpottyError::Audio(_)));
        assert_eq!(*seen_port.lock().expect("lock"), Some(Some(44_100)));
        assert!(*dropped.lock().expect("lock"));
    }

    #[test]
    fn derive_device_id_salted_avoids_cross_host_collisions() {
        let a = derive_device_id_salted("Squeezebox", "/var/cache/host-a");
        let b = derive_device_id_salted("Squeezebox", "/var/cache/host-b");
        assert_ne!(a, b);
        assert_eq!(a.len(), 40);
        assert_eq!(b.len(), 40);
    }
}
