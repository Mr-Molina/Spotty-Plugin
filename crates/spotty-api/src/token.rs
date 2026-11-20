//! `OAuth2` PKCE challenge generation and single-flight Spotify token manager.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use spotty_cache::{AccountId, AccountStore, DualCache};
use spotty_types::{IconCode, Result, SpottyError};
use tokio::sync::watch;
use tracing::{error, info};

/// Default Spotify `OAuth2` token endpoint URL.
pub const DEFAULT_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Fallback Spotify access token lifetime in seconds when omitted by the API.
pub const DEFAULT_EXPIRATION_SECS: u64 = 3600;

/// Persistent TTL for Spotify `OAuth2` refresh tokens (180 days).
pub const REFRESH_TOKEN_TTL_SECS: u64 = 180 * 86_400;

const ACCESS_TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
const PKCE_VERIFIER_LEN: usize = 128;
const PKCE_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-~.";

/// 13 Spotify Web API OAuth scopes for API operations (`API/Token.pm`).
pub const SPOTIFY_API_SCOPES: &[&str] = &[
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read-collaborative",
    "playlist-read-private",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify-playback-state",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

/// 21 Spotify PKCE callback OAuth scopes (`Settings/Callback.pm`).
pub const SPOTIFY_AUTH_SCOPES: &[&str] = &[
    "playlist-read",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-public",
    "playlist-modify-private",
    "user-follow-modify",
    "user-follow-read",
    "user-library-read",
    "user-library-modify",
    "user-read-private",
    "user-read-email",
    "user-top-read",
    "app-remote-control",
    "streaming",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "user-modify-private",
    "user-modify",
    "user-read-playback-position",
    "user-read-recently-played",
];

/// Generated `OAuth2` PKCE verifier and `S256` challenge pair.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceChallenge {
    /// 128-character CSPRNG code verifier from `A-Za-z0-9_-~.`.
    pub verifier: String,
    /// `Base64URL` (no-pad) SHA-256 digest of `verifier`.
    pub challenge_s256: String,
}

impl PkceChallenge {
    /// Generates a fresh 128-character CSPRNG PKCE verifier and `S256` challenge.
    #[must_use]
    pub fn generate() -> Self {
        use rand::rngs::OsRng;
        use rand::Rng as _;

        let mut rng = OsRng;
        let mut verifier = String::with_capacity(PKCE_VERIFIER_LEN);
        for _ in 0..PKCE_VERIFIER_LEN {
            let idx = rng.gen_range(0..PKCE_ALPHABET.len());
            verifier.push(char::from(PKCE_ALPHABET[idx]));
        }
        Self::from_verifier(verifier)
    }

    /// Computes the `S256` PKCE challenge from an existing `verifier`.
    #[must_use]
    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use sha2::Digest as _;

        let verifier_str = verifier.into();
        let digest = Sha256::digest(verifier_str.as_bytes());
        let challenge_s256 = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier: verifier_str,
            challenge_s256,
        }
    }
}

/// `OAuth2` token exchange payload returned by Spotify's `/api/token` endpoint.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBundle {
    /// Short-lived `OAuth2` bearer access token.
    pub access_token: String,
    /// Optional rotated or initial refresh token.
    pub refresh_token: Option<String>,
    /// Access token lifetime in seconds.
    pub expires_in: u64,
    /// Token type returned by Spotify (typically `"Bearer"`).
    pub token_type: Option<String>,
    /// Space-delimited granted OAuth scope string.
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpotifyTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    error_message: Option<String>,
}

type InflightReceiver = watch::Receiver<Option<Result<String>>>;
type InflightMap = Arc<Mutex<HashMap<String, InflightReceiver>>>;

struct InflightGuard {
    inflight: InflightMap,
    flight_key: String,
    active: bool,
}

impl InflightGuard {
    fn new(inflight: &InflightMap, flight_key: String) -> Self {
        Self {
            inflight: Arc::clone(inflight),
            flight_key,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.active {
            let mut guard = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            guard.remove(&self.flight_key);
        }
    }
}

/// Manages Spotify `OAuth2` PKCE exchanges and single-flight token refreshes.
#[derive(Debug)]
pub struct TokenManager {
    token_url: String,
    http_client: Client,
    cache: Arc<DualCache>,
    accounts: Arc<AccountStore>,
    startup_epoch: u64,
    inflight: InflightMap,
}

impl TokenManager {
    /// Creates a [`TokenManager`] using [`DEFAULT_TOKEN_URL`] and a `rustls`
    /// `reqwest` client with a 10-second connect timeout and 30-second timeout.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Http`] if the underlying HTTP client cannot be
    /// initialized.
    pub fn new(cache: Arc<DualCache>, accounts: Arc<AccountStore>) -> Result<Self> {
        let http_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .use_rustls_tls()
            .build()
            .map_err(|err| SpottyError::Http(err.to_string()))?;
        Ok(Self::with_http(
            DEFAULT_TOKEN_URL,
            http_client,
            cache,
            accounts,
            current_epoch_secs(),
        ))
    }

    /// Creates a [`TokenManager`] with an explicit token endpoint URL, HTTP
    /// client, and startup epoch (useful for `wiremock` tests).
    #[must_use]
    pub fn with_http(
        token_url: impl Into<String>,
        http_client: Client,
        cache: Arc<DualCache>,
        accounts: Arc<AccountStore>,
        startup_epoch: u64,
    ) -> Self {
        Self {
            token_url: token_url.into(),
            http_client,
            cache,
            accounts,
            startup_epoch,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns a reference to the shared `reqwest` HTTP client.
    #[must_use]
    pub fn http_client(&self) -> &Client {
        &self.http_client
    }

    /// Returns the configured `OAuth2` token endpoint URL.
    #[must_use]
    pub fn token_url(&self) -> &str {
        &self.token_url
    }

    /// Returns the startup epoch used to namespace cached access tokens.
    #[must_use]
    pub fn startup_epoch(&self) -> u64 {
        self.startup_epoch
    }

    /// Exchanges an authorization `code` and PKCE `code_verifier` for tokens.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::InvalidGrant`] if Spotify rejects the grant with
    /// `invalid_grant`, [`SpottyError::Http`] on transport or HTTP failures,
    /// or [`SpottyError::Json`] if the token response cannot be decoded.
    pub async fn exchange_pkce_code(
        &self,
        client_id: &IconCode,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<TokenBundle> {
        let form_params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id.as_str()),
            ("code_verifier", code_verifier),
        ];
        let payload = self.post_token_form(&form_params).await?;

        if payload.error.as_deref() == Some("invalid_grant") {
            return Err(SpottyError::InvalidGrant);
        }
        if let Some(err_msg) = extract_token_error(&payload) {
            return Err(SpottyError::Http(err_msg));
        }
        let Some(access_token) = payload.access_token.filter(|tok| !tok.is_empty()) else {
            return Err(SpottyError::Http(String::from(
                "token exchange response missing access_token",
            )));
        };

        Ok(TokenBundle {
            access_token,
            refresh_token: payload.refresh_token.filter(|rt| !rt.is_empty()),
            expires_in: payload.expires_in.unwrap_or(DEFAULT_EXPIRATION_SECS),
            token_type: payload.token_type,
            scope: payload.scope,
        })
    }

    /// Returns a valid access token for `(client_id, user_id)`, refreshing via
    /// single-flight coalescing when the cached startup-epoch token is absent.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::NoAccessToken`] when no refresh token exists,
    /// [`SpottyError::InvalidGrant`] when Spotify returns `invalid_grant` (after
    /// purging cached tokens and deleting the account in [`AccountStore`]), or
    /// [`SpottyError::Http`] / [`SpottyError::Cache`] on transport or storage
    /// errors.
    pub async fn access_token_for(&self, client_id: &IconCode, user_id: &str) -> Result<String> {
        let at_key = self.access_token_key(client_id, user_id);
        if let Some(cached) = self.read_cached_token(&at_key)? {
            return Ok(cached);
        }

        let flight_key = format!("{}:{}", client_id.as_str(), user_id);
        let (leader_tx, mut follower_rx, mut inflight_guard) = {
            let mut guard = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing_rx) = guard.get(&flight_key) {
                (None, Some(existing_rx.clone()), None)
            } else if let Some(cached) = self.read_cached_token(&at_key)? {
                return Ok(cached);
            } else {
                let (tx, rx) = watch::channel(None);
                guard.insert(flight_key.clone(), rx);
                let drop_guard = InflightGuard::new(&self.inflight, flight_key.clone());
                (Some(tx), None, Some(drop_guard))
            }
        };

        if let Some(mut rx) = follower_rx.take() {
            loop {
                if let Some(result) = rx.borrow().as_ref() {
                    return result.clone();
                }
                if rx.changed().await.is_err() {
                    return Err(SpottyError::Http(String::from(
                        "in-flight token refresh terminated unexpectedly",
                    )));
                }
            }
        }

        let Some(tx) = leader_tx else {
            return Err(SpottyError::NoAccessToken);
        };

        let outcome = self.refresh_leader(client_id, user_id).await;
        {
            let mut guard = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            guard.remove(&flight_key);
        }
        if let Some(mut drop_guard) = inflight_guard.take() {
            drop_guard.disarm();
        }
        let _ignored = tx.send(Some(outcome.clone()));
        outcome
    }

    /// Evicts the cached startup-epoch access token for `(client_id, user_id)`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] if deleting from [`DualCache`] fails.
    pub fn invalidate_access_token(&self, client_id: &IconCode, user_id: &str) -> Result<()> {
        let at_key = self.access_token_key(client_id, user_id);
        let _removed = self.cache.remove_ephemeral(&at_key)?;
        Ok(())
    }

    /// Persists an access token (using a 300s refresh margin, or `expires_in / 2`
    /// when `expires_in <= 300`) and an optional refresh token (180-day TTL) into
    /// [`DualCache`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Json`] if writing to
    /// [`DualCache`] fails.
    pub fn cache_tokens(
        &self,
        client_id: &IconCode,
        user_id: &str,
        access_token: &str,
        expires_in: u64,
        refresh_token: Option<&str>,
    ) -> Result<()> {
        let at_key = self.access_token_key(client_id, user_id);
        let at_ttl_secs = compute_access_token_ttl_secs(expires_in);
        let at_val = Value::String(access_token.to_owned());
        self.cache
            .put_ephemeral(&at_key, &at_val, Duration::from_secs(at_ttl_secs))?;

        if let Some(rt) = refresh_token.filter(|token| !token.is_empty()) {
            let rt_key = refresh_token_key(client_id, user_id);
            let rt_val = Value::String(rt.to_owned());
            self.cache.put_ephemeral(
                &rt_key,
                &rt_val,
                Duration::from_secs(REFRESH_TOKEN_TTL_SECS),
            )?;
        }

        Ok(())
    }

    async fn refresh_leader(&self, client_id: &IconCode, user_id: &str) -> Result<String> {
        let rt_key = refresh_token_key(client_id, user_id);
        let Some(refresh_token) = self.read_cached_token(&rt_key)? else {
            return Err(SpottyError::NoAccessToken);
        };

        let form_params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", client_id.as_str()),
        ];
        let payload = self.post_token_form(&form_params).await?;

        if payload.error.as_deref() == Some("invalid_grant") {
            error!(
                user_id = %user_id,
                "Spotify rejected refresh token with invalid_grant; resetting account"
            );
            self.purge_account(client_id, user_id)?;
            return Err(SpottyError::InvalidGrant);
        }

        if let Some(err_msg) = extract_token_error(&payload) {
            return Err(SpottyError::Http(err_msg));
        }

        let Some(access_token) = payload.access_token.filter(|tok| !tok.is_empty()) else {
            return Err(SpottyError::Http(String::from(
                "refresh token response missing access_token",
            )));
        };

        let expires_in = payload.expires_in.unwrap_or(DEFAULT_EXPIRATION_SECS);
        let rotated_rt = payload.refresh_token.as_deref();
        self.cache_tokens(client_id, user_id, &access_token, expires_in, rotated_rt)?;
        info!(user_id = %user_id, "Refreshed Spotify access token");

        Ok(access_token)
    }

    async fn post_token_form(&self, form_params: &[(&str, &str)]) -> Result<SpotifyTokenResponse> {
        let response = self
            .http_client
            .post(&self.token_url)
            .header("Accept", "application/json")
            .form(form_params)
            .send()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        if let Ok(parsed) = serde_json::from_str::<SpotifyTokenResponse>(&body) {
            if !status.is_success() && parsed.error.is_none() {
                return Err(SpottyError::Http(format!(
                    "token endpoint returned HTTP {}: {body}",
                    status.as_u16()
                )));
            }
            return Ok(parsed);
        }

        if !status.is_success() {
            return Err(SpottyError::Http(format!(
                "token endpoint returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        Err(SpottyError::Json(format!(
            "failed to parse token response JSON: {body}"
        )))
    }

    fn purge_account(&self, client_id: &IconCode, user_id: &str) -> Result<()> {
        let at_key = self.access_token_key(client_id, user_id);
        let rt_key = refresh_token_key(client_id, user_id);
        let _at_removed = self.cache.remove_ephemeral(&at_key)?;
        let _rt_removed = self.cache.remove_ephemeral(&rt_key)?;
        let account_id = AccountId::from_username(user_id);
        self.accounts.delete_account(&account_id)?;
        Ok(())
    }

    fn read_cached_token(&self, key: &str) -> Result<Option<String>> {
        let Some(Value::String(token)) = self.cache.ephemeral_value(key)? else {
            return Ok(None);
        };
        if token.is_empty() {
            return Ok(None);
        }
        Ok(Some(token))
    }

    fn access_token_key(&self, client_id: &IconCode, user_id: &str) -> String {
        format!(
            "spotty_access_token_{}_{}_{}",
            self.startup_epoch,
            client_id.as_str(),
            user_id
        )
    }
}

fn compute_access_token_ttl_secs(expires_in: u64) -> u64 {
    if expires_in > ACCESS_TOKEN_REFRESH_MARGIN_SECS {
        expires_in - ACCESS_TOKEN_REFRESH_MARGIN_SECS
    } else if expires_in > 1 {
        expires_in / 2
    } else {
        expires_in
    }
}

fn refresh_token_key(client_id: &IconCode, user_id: &str) -> String {
    format!("spotty_refresh_token_{}_{}", client_id.as_str(), user_id)
}

fn extract_token_error(payload: &SpotifyTokenResponse) -> Option<String> {
    let err = payload.error.as_deref()?;
    let detail = payload
        .error_description
        .as_deref()
        .or(payload.error_message.as_deref());
    match detail {
        Some(desc) if !desc.is_empty() => Some(format!("{err}: {desc}")),
        _ => Some(err.to_owned()),
    }
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use futures_util::future::join_all;
    use reqwest::Client;
    use serde_json::json;
    use spotty_cache::{AccountId, AccountStore, DualCache};
    use spotty_types::{IconCode, SpottyError};
    use tempfile::TempDir;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        PkceChallenge, TokenManager, PKCE_ALPHABET, PKCE_VERIFIER_LEN, SPOTIFY_API_SCOPES,
        SPOTIFY_AUTH_SCOPES,
    };

    fn test_manager(mock_uri: &str, tmp: &TempDir) -> (Arc<TokenManager>, Arc<AccountStore>) {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open test DualCache"));
        let accounts = Arc::new(AccountStore::new(tmp.path()));
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client");
        let manager = Arc::new(TokenManager::with_http(
            format!("{mock_uri}/api/token"),
            client,
            cache,
            Arc::clone(&accounts),
            1_700_000_000,
        ));
        (manager, accounts)
    }

    #[test]
    fn scope_constants_match_legacy_counts() {
        assert_eq!(SPOTIFY_API_SCOPES.len(), 13);
        assert_eq!(SPOTIFY_AUTH_SCOPES.len(), 21);
    }

    #[test]
    fn pkce_challenge_generates_128_char_verifier_and_s256_digest() {
        let pkce = PkceChallenge::generate();
        assert_eq!(pkce.verifier.len(), PKCE_VERIFIER_LEN);
        assert!(
            pkce.verifier.bytes().all(|b| PKCE_ALPHABET.contains(&b)),
            "PKCE verifier contained invalid character: {}",
            pkce.verifier
        );
        assert!(!pkce.challenge_s256.contains('='));
        assert!(!pkce.challenge_s256.contains('+'));
        assert!(!pkce.challenge_s256.contains('/'));

        // RFC 7636 Appendix B test vector.
        let rfc_vec = PkceChallenge::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            rfc_vec.challenge_s256,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[tokio::test]
    async fn ten_concurrent_refresh_callers_coalesce_into_single_post() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(60))
                    .set_body_json(json!({
                        "access_token": "fresh-access-token",
                        "refresh_token": "rotated-refresh-token",
                        "expires_in": 3600
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("temp dir");
        let (manager, _) = test_manager(&server.uri(), &tmp);
        let client_id = IconCode::default();

        // Seed only a refresh token (expires_in=0 skips caching an access token).
        manager
            .cache_tokens(&client_id, "alice", "", 0, Some("seed-refresh-token"))
            .expect("seed refresh token");

        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let cid = client_id.clone();
                async move { mgr.access_token_for(&cid, "alice").await }
            })
            .collect();

        let results = join_all(tasks).await;
        for (idx, res) in results.into_iter().enumerate() {
            let tok = res.unwrap_or_else(|e| panic!("caller {idx} failed: {e:?}"));
            assert_eq!(tok, "fresh-access-token");
        }

        // Subsequent call hits cached startup-epoch access token with 0 HTTP calls.
        let cached = manager
            .access_token_for(&client_id, "alice")
            .await
            .expect("cached access token");
        assert_eq!(cached, "fresh-access-token");
    }

    #[tokio::test]
    async fn invalid_grant_purges_tokens_and_deletes_account_folder() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "Refresh token revoked"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("temp dir");
        let (manager, accounts) = test_manager(&server.uri(), &tmp);
        let client_id = IconCode::default();
        let user_id = "revoked-user";

        // Seed account credentials folder on disk.
        let account_id = AccountId::from_username(user_id);
        let creds_path = accounts.account_dir(&account_id).join("credentials.json");
        fs::create_dir_all(creds_path.parent().expect("parent")).expect("create account dir");
        fs::write(&creds_path, br#"{"username":"revoked-user"}"#).expect("write creds");
        assert!(accounts.account_dir(&account_id).exists());

        manager
            .cache_tokens(&client_id, user_id, "", 0, Some("dead-refresh-token"))
            .expect("seed refresh token");

        let err = manager
            .access_token_for(&client_id, user_id)
            .await
            .expect_err("invalid_grant must return error");
        assert_eq!(err, SpottyError::InvalidGrant);
        assert!(
            !accounts.account_dir(&account_id).exists(),
            "invalid_grant must delete account folder"
        );

        // Next lookup finds no refresh token and returns NoAccessToken without HTTP.
        let followup = manager
            .access_token_for(&client_id, user_id)
            .await
            .expect_err("purged account must have no refresh token");
        assert_eq!(followup, SpottyError::NoAccessToken);
    }

    #[tokio::test]
    async fn exchange_pkce_code_returns_token_bundle() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code_verifier=my-verifier"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "pkce-at",
                "refresh_token": "pkce-rt",
                "expires_in": 3600,
                "token_type": "Bearer",
                "scope": "user-read-private"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("temp dir");
        let (manager, _) = test_manager(&server.uri(), &tmp);
        let client_id = IconCode::default();

        let bundle = manager
            .exchange_pkce_code(
                &client_id,
                "auth-code-123",
                "https://api.lms-community.org/auth/callback",
                "my-verifier",
            )
            .await
            .expect("PKCE code exchange should succeed");

        assert_eq!(bundle.access_token, "pkce-at");
        assert_eq!(bundle.refresh_token.as_deref(), Some("pkce-rt"));
        assert_eq!(bundle.expires_in, 3600);
        assert_eq!(bundle.token_type.as_deref(), Some("Bearer"));
    }

    #[tokio::test]
    async fn cancelled_leader_cleans_up_inflight_key_for_subsequent_callers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(150))
                    .set_body_json(json!({
                        "access_token": "recovered-access-token",
                        "refresh_token": "rotated-rt",
                        "expires_in": 3600
                    })),
            )
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("temp dir");
        let (manager, _) = test_manager(&server.uri(), &tmp);
        let client_id = IconCode::default();
        manager
            .cache_tokens(&client_id, "bob", "", 0, Some("seed-rt"))
            .expect("seed refresh token");

        // Cancel the leader future mid-flight via a short timeout.
        let timed_out = tokio::time::timeout(
            Duration::from_millis(20),
            manager.access_token_for(&client_id, "bob"),
        )
        .await;
        assert!(timed_out.is_err(), "first call should time out mid-flight");

        // Subsequent caller must not get stuck on a leaked inflight entry.
        let recovered = manager
            .access_token_for(&client_id, "bob")
            .await
            .expect("subsequent caller should recover and succeed");
        assert_eq!(recovered, "recovered-access-token");
    }

    #[test]
    fn short_lived_expires_in_uses_proportional_ttl_margin() {
        assert_eq!(super::compute_access_token_ttl_secs(3600), 3300);
        assert_eq!(super::compute_access_token_ttl_secs(300), 150);
        assert_eq!(super::compute_access_token_ttl_secs(60), 30);
        assert_eq!(super::compute_access_token_ttl_secs(1), 1);
        assert_eq!(super::compute_access_token_ttl_secs(0), 0);
    }
}
