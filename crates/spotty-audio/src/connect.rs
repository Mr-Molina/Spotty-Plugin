//! Spotify Connect Zeroconf discovery authentication and OAuth credential
//! materialization.

use std::fs;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
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
    fn discover_credentials<'a>(
        &'a self,
        device_name: &'a str,
        client_id: &'a str,
        ap_fallback: ApFallbackPolicy,
    ) -> BackendDiscoveryFuture<'a>;
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
        Box::pin(discover_via_librespot(device_name, client_id, ap_fallback))
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
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let result = self
            .authenticate_with_cancellation(
                cache_dir,
                device_name,
                client_id,
                ap_fallback,
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
                self.backend.discover_credentials(device_name, client_id, ap_fallback),
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
        write_credentials_file(cache_dir, &record)?;
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

async fn discover_via_librespot(
    device_name: &str,
    client_id: &str,
    ap_fallback: ApFallbackPolicy,
) -> Result<CredentialsRecord> {
    use futures_util::StreamExt as _;

    let _ = ap_fallback;
    let device_id = derive_device_id(device_name);
    let mut discovery = Discovery::builder(device_id, client_id.to_owned())
        .name(device_name.to_owned())
        .device_type(DeviceType::Speaker)
        .launch()
        .map_err(|err| SpottyError::Audio(format!("failed to start Zeroconf discovery: {err}")))?;

    let maybe_creds = discovery.next().await;
    discovery.shutdown().await;

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
    fs::write(&creds_path, json)?;
    Ok(creds_path)
}

fn derive_device_id(device_name: &str) -> String {
    const FNV_OFFSET_1: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_OFFSET_2: u64 = 0x8422_2325_cbf2_9ce4;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    let mut h1 = FNV_OFFSET_1;
    let mut h2 = FNV_OFFSET_2;
    let mut h3 = FNV_OFFSET_1 ^ 0x5555_5555_5555_5555;
    for &byte in device_name.as_bytes() {
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
}
