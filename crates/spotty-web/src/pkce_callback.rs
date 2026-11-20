//! PKCE `api.lms-community.org` OAuth relay and callback handler.

use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use spotty_api::{
    normalize_user_profile, PkceChallenge, TokenManager, DEFAULT_API_BASE_URL, SPOTIFY_AUTH_SCOPES,
};
use spotty_audio::materialize_oauth_credentials;
use spotty_cache::{AccountId, AccountStore, DualCache, AUTHENTICATE_DIR};
use spotty_types::{IconCode, Result, SpottyError};
use tracing::{error, info, warn};
use url::Url;

/// Relay preparation endpoint (`Settings/Callback.pm::REGISTER_CALLBACK_URL`).
pub const LMS_COMMUNITY_PREPARE_URL: &str = "https://api.lms-community.org/auth/prepare";

/// Relay OAuth callback endpoint (`Settings/Callback.pm::CALLBACK_URL`).
pub const LMS_COMMUNITY_CALLBACK_URL: &str = "https://api.lms-community.org/auth/callback";

/// Default Spotify Web Accounts authorization endpoint.
pub const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";

/// Ephemeral cache key storing the active PKCE `code_verifier`.
pub const PKCE_VERIFIER_CACHE_KEY: &str = "spotty_auth_code_verifier";

/// HTTP header identifying the LMS plugin package to the relay service.
pub const LMS_PLUGIN_ID_HEADER: &str = "X-LMS-Plugin-ID";

/// Default plugin identifier sent in [`LMS_PLUGIN_ID_HEADER`].
pub const DEFAULT_PLUGIN_ID: &str = "Plugins::Spotty::Plugin";

/// Expected local LMS callback path for `lms_redirect_uri` validation.
pub const LMS_CALLBACK_PATH: &str = "/plugins/Spotty/settings/callback";

/// Skin override forced by `feedbackPage` so Material skin does not break.
pub const CALLBACK_SKIN_OVERRIDE: &str = "Default";

const PREPARE_TIMEOUT_SECS: u64 = 10;
const PKCE_VERIFIER_TTL_SECS: u64 = 900;
const CSRF_TOKEN_LEN: usize = 32;

#[derive(Debug, Deserialize, Serialize)]
struct OAuthStatePayload {
    nonce: String,
    csrf: String,
}

#[derive(Debug, Deserialize)]
struct RelayPrepareResponse {
    nonce: Option<String>,
    error: Option<String>,
}

/// Template view-model for `HTML/EN/plugins/Spotty/settings/callback.html`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CallbackViewModel {
    /// Non-empty authentication error message, if the OAuth exchange failed.
    #[serde(rename = "auth_error")]
    pub auth_error: Option<String>,
    /// `true` when the authenticated account is on Spotify Free/Open tier.
    #[serde(rename = "freeProduct")]
    pub free_product: bool,
    /// Skin override (`"Default"`) expected by `Settings::Callback::feedbackPage`.
    #[serde(rename = "skinOverride")]
    pub skin_override: &'static str,
}

impl CallbackViewModel {
    fn success(free_product: bool) -> Self {
        Self {
            auth_error: None,
            free_product,
            skin_override: CALLBACK_SKIN_OVERRIDE,
        }
    }

    fn failure(message: impl Into<String>) -> Self {
        Self {
            auth_error: Some(escape_html(&message.into())),
            free_product: false,
            skin_override: CALLBACK_SKIN_OVERRIDE,
        }
    }
}

/// Coordinates PKCE OAuth relay registration and token callback exchange.
#[derive(Debug)]
pub struct PkceCallbackController {
    prepare_url: String,
    callback_url: String,
    authorize_url: String,
    api_base_url: String,
    plugin_id: String,
    http_client: Client,
    cache: Arc<DualCache>,
    accounts: Arc<AccountStore>,
    tokens: Arc<TokenManager>,
}

impl PkceCallbackController {
    /// Creates a [`PkceCallbackController`] targeting the production LMS relay
    /// and Spotify endpoints with a 10-second relay request timeout.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Http`] if building the `reqwest` client fails.
    pub fn new(
        cache: Arc<DualCache>,
        accounts: Arc<AccountStore>,
        tokens: Arc<TokenManager>,
    ) -> Result<Self> {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(PREPARE_TIMEOUT_SECS))
            .gzip(true)
            .use_rustls_tls()
            .build()
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        Ok(Self::with_endpoints(
            LMS_COMMUNITY_PREPARE_URL,
            LMS_COMMUNITY_CALLBACK_URL,
            SPOTIFY_AUTHORIZE_URL,
            DEFAULT_API_BASE_URL,
            http_client,
            cache,
            accounts,
            tokens,
        ))
    }

    /// Creates a [`PkceCallbackController`] with explicit relay, authorize, and
    /// Spotify API base URLs (intended for `wiremock` integration tests).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_endpoints(
        prepare_url: impl Into<String>,
        callback_url: impl Into<String>,
        authorize_url: impl Into<String>,
        api_base_url: impl Into<String>,
        http_client: Client,
        cache: Arc<DualCache>,
        accounts: Arc<AccountStore>,
        tokens: Arc<TokenManager>,
    ) -> Self {
        Self {
            prepare_url: prepare_url.into(),
            callback_url: callback_url.into(),
            authorize_url: authorize_url.into(),
            api_base_url: api_base_url.into().trim_end_matches('/').to_owned(),
            plugin_id: String::from(DEFAULT_PLUGIN_ID),
            http_client,
            cache,
            accounts,
            tokens,
        }
    }

    /// Overrides the `X-LMS-Plugin-ID` header value sent to the relay endpoint.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = plugin_id.into();
        self
    }

    /// Returns the configured relay preparation URL.
    #[must_use]
    pub fn prepare_url(&self) -> &str {
        &self.prepare_url
    }

    /// Returns the configured relay OAuth callback URL (`redirect_uri`).
    #[must_use]
    pub fn callback_url(&self) -> &str {
        &self.callback_url
    }

    /// Registers the local LMS callback URL with `api.lms-community.org`,
    /// generates and caches a 128-character PKCE verifier, and builds the
    /// Spotify `authorize` 302 redirect URL.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Http`], [`SpottyError::InvalidUri`],
    /// [`SpottyError::Json`], or [`SpottyError::Cache`] if relay registration,
    /// nonce extraction, URL construction, or verifier caching fails.
    pub async fn prepare_oauth_redirect(
        &self,
        client_id: &IconCode,
        lms_redirect_uri: &str,
        user_agent: &str,
    ) -> Result<String> {
        let validated_redirect = validate_lms_redirect_uri(lms_redirect_uri)?;
        let payload = json!({
            "url": validated_redirect,
            "ua": user_agent,
        });

        let response = self
            .http_client
            .post(&self.prepare_url)
            .header(LMS_PLUGIN_ID_HEADER, &self.plugin_id)
            .timeout(Duration::from_secs(PREPARE_TIMEOUT_SECS))
            .json(&payload)
            .send()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|val| val.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let body = response
            .text()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        if !status.is_success() {
            return Err(SpottyError::Http(format!(
                "relay prepare returned HTTP {}: {body}",
                status.as_u16()
            )));
        }
        if !content_type.to_ascii_lowercase().contains("json") {
            return Err(SpottyError::Http(String::from("Failed to get token")));
        }

        let parsed: RelayPrepareResponse = serde_json::from_str(&body)?;
        if let Some(err_msg) = parsed.error.filter(|msg| !msg.is_empty()) {
            return Err(SpottyError::Http(err_msg));
        }
        let Some(nonce) = parsed.nonce.filter(|val| !val.is_empty()) else {
            return Err(SpottyError::Http(String::from(
                "relay prepare response missing nonce",
            )));
        };

        let pkce = PkceChallenge::generate();
        let csrf_source = PkceChallenge::generate().verifier;
        let csrf = csrf_source
            .get(..CSRF_TOKEN_LEN)
            .unwrap_or(csrf_source.as_str())
            .to_owned();
        let cache_key = pkce_verifier_cache_key(&csrf);
        self.cache.put_ephemeral(
            &cache_key,
            &Value::String(pkce.verifier),
            Duration::from_secs(PKCE_VERIFIER_TTL_SECS),
        )?;

        let state_payload = OAuthStatePayload { nonce, csrf };
        let state_json = serde_json::to_vec(&state_payload)?;
        let state = STANDARD.encode(state_json);
        let scope_str = SPOTIFY_AUTH_SCOPES.join(" ");

        let mut auth_url = Url::parse(&self.authorize_url)
            .map_err(|err| SpottyError::InvalidUri(err.to_string()))?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", client_id.as_str())
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.callback_url)
            .append_pair("code_challenge", &pkce.challenge_s256)
            .append_pair("code_challenge_method", "S256")
            .append_pair("scope", &scope_str)
            .append_pair("state", &state);

        Ok(auth_url.to_string())
    }

    /// Legacy OAuth callback entry point kept for backwards compatibility.
    ///
    /// Fails closed because OAuth CSRF `state` validation is mandatory; callers
    /// should use [`Self::handle_oauth_callback_verified`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Io`] if local cache or
    /// credential materialization fails unexpectedly.
    pub async fn handle_oauth_callback(
        &self,
        client_id: &IconCode,
        code: Option<&str>,
    ) -> Result<CallbackViewModel> {
        self.handle_oauth_callback_verified(client_id, code, None, None, None)
            .await
    }

    /// Validates OAuth `error` and `state` CSRF parameters, atomically consumes
    /// the per-flow PKCE verifier, exchanges `?code`, fetches `/v1/me` profile
    /// metadata, caches tokens, and materializes
    /// `<spotty_dir>/__AUTHENTICATE__/credentials.json`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Cache`] or [`SpottyError::Io`] if local cache or
    /// credential materialization fails unexpectedly.
    pub async fn handle_oauth_callback_verified(
        &self,
        client_id: &IconCode,
        code: Option<&str>,
        state: Option<&str>,
        error: Option<&str>,
        error_description: Option<&str>,
    ) -> Result<CallbackViewModel> {
        if let Some(err_code) = error.map(str::trim).filter(|val| !val.is_empty()) {
            warn!(oauth_error = err_code, "OAuth callback returned error");
            return Ok(CallbackViewModel::failure(format_oauth_error(
                err_code,
                error_description,
            )));
        }

        let Some(raw_state) = state.map(str::trim).filter(|val| !val.is_empty()) else {
            error!("OAuth callback invoked without required state parameter");
            return Ok(CallbackViewModel::failure("Missing OAuth state"));
        };

        let Some(parsed_state) = parse_oauth_state(raw_state) else {
            error!("OAuth callback invoked with malformed state parameter");
            return Ok(CallbackViewModel::failure("Invalid OAuth state"));
        };

        let cache_key = pkce_verifier_cache_key(&parsed_state.csrf);
        let Some(Value::String(code_verifier)) = self.cache.ephemeral_value(&cache_key)? else {
            error!("OAuth callback missing or expired PKCE code_verifier");
            return Ok(CallbackViewModel::failure("Invalid or expired OAuth state"));
        };
        let _consumed = self.cache.remove_ephemeral(&cache_key)?;

        let Some(auth_code) = code.map(str::trim).filter(|val| !val.is_empty()) else {
            error!("OAuth callback invoked without authorization code");
            return Ok(CallbackViewModel::failure("Missing authorization code"));
        };

        let bundle = match self
            .tokens
            .exchange_pkce_code(client_id, auth_code, &self.callback_url, &code_verifier)
            .await
        {
            Ok(bundle) => bundle,
            Err(err) => {
                error!(error = %err, "PKCE token exchange failed");
                return Ok(CallbackViewModel::failure("Spotify token exchange failed"));
            }
        };

        let profile_json = match self.fetch_me_json(&bundle.access_token).await {
            Ok(json) => json,
            Err(err) => {
                error!(error = %err, "Failed to get user profile");
                return Ok(CallbackViewModel::failure(
                    "Failed to fetch Spotify user profile",
                ));
            }
        };

        let profile = match normalize_user_profile(&profile_json) {
            Ok(profile) if !profile.id.is_empty() => profile,
            Ok(_) => {
                error!("Spotify /v1/me profile missing user id");
                return Ok(CallbackViewModel::failure(
                    "Spotify profile response missing user id",
                ));
            }
            Err(err) => {
                error!(error = %err, "Failed to normalize /v1/me profile");
                return Ok(CallbackViewModel::failure("Invalid Spotify user profile"));
            }
        };

        let display_name = profile.display_name.as_deref().unwrap_or("no display name");
        let product = profile.product.as_deref().unwrap_or("no product info");
        warn!(
            user_id = %profile.id,
            display_name,
            product,
            "Authenticated Spotify user"
        );

        self.tokens.cache_tokens(
            client_id,
            &profile.id,
            &bundle.access_token,
            bundle.expires_in,
            bundle.refresh_token.as_deref(),
        )?;

        let auth_id = AccountId::parse(AUTHENTICATE_DIR)?;
        let auth_dir = self.accounts.account_dir(&auth_id);
        let _creds_path =
            materialize_oauth_credentials(&auth_dir, &bundle.access_token, &profile.id)?;

        let free_product = profile.product.as_deref().is_some_and(|tier| {
            tier.eq_ignore_ascii_case("free") || tier.eq_ignore_ascii_case("open")
        });
        info!(user_id = %profile.id, free_product, "Materialized OAuth credentials");

        Ok(CallbackViewModel::success(free_product))
    }

    async fn fetch_me_json(&self, access_token: &str) -> Result<Value> {
        let me_url = format!("{}/me", self.api_base_url);
        let response = self
            .http_client
            .get(&me_url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;
        if !status.is_success() {
            return Err(SpottyError::Http(format!(
                "Spotify /v1/me returned HTTP {}: {body}",
                status.as_u16()
            )));
        }

        let parsed: Value = serde_json::from_str(&body)?;
        Ok(parsed)
    }
}

fn pkce_verifier_cache_key(csrf: &str) -> String {
    format!("{PKCE_VERIFIER_CACHE_KEY}:{csrf}")
}

fn validate_lms_redirect_uri(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed)
        .map_err(|err| SpottyError::InvalidUri(format!("invalid lms_redirect_uri: {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SpottyError::InvalidUri(String::from(
            "lms_redirect_uri must use http or https scheme",
        )));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(SpottyError::InvalidUri(String::from(
            "lms_redirect_uri must include a host",
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(SpottyError::InvalidUri(String::from(
            "lms_redirect_uri must not embed userinfo",
        )));
    }
    if parsed.path() != LMS_CALLBACK_PATH {
        return Err(SpottyError::InvalidUri(format!(
            "lms_redirect_uri path must be {LMS_CALLBACK_PATH}"
        )));
    }
    Ok(parsed.to_string())
}

fn parse_oauth_state(raw_state: &str) -> Option<OAuthStatePayload> {
    let decoded = STANDARD.decode(raw_state).ok()?;
    let payload: OAuthStatePayload = serde_json::from_slice(&decoded).ok()?;
    if payload.nonce.trim().is_empty() || payload.csrf.trim().is_empty() {
        return None;
    }
    Some(payload)
}

fn format_oauth_error(err_code: &str, description: Option<&str>) -> String {
    let desc = description.map(str::trim).filter(|val| !val.is_empty());
    match desc {
        Some(detail) => format!("OAuth authorization failed ({err_code}): {detail}"),
        None => format!("OAuth authorization failed ({err_code})"),
    }
}

pub(crate) fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::TempDir;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::AuthLandingController;

    #[tokio::test]
    async fn full_pkce_oauth_round_trip_materializes_and_promotes_credentials() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let cache = Arc::new(DualCache::open(dir.path().join("cache.redb")).expect("cache"));
        let accounts = Arc::new(AccountStore::new(dir.path()));
        let tokens = Arc::new(TokenManager::with_http(
            format!("{}/api/token", server.uri()),
            Client::new(),
            Arc::clone(&cache),
            Arc::clone(&accounts),
            1_700_000_000,
        ));

        Mock::given(method("POST"))
            .and(path("/auth/prepare"))
            .and(header(LMS_PLUGIN_ID_HEADER, DEFAULT_PLUGIN_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "nonce": "relay-nonce-xyz"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let callback_relay_url = format!("{}/auth/callback", server.uri());
        let controller = PkceCallbackController::with_endpoints(
            format!("{}/auth/prepare", server.uri()),
            callback_relay_url.clone(),
            "https://accounts.spotify.com/authorize",
            format!("{}/v1", server.uri()),
            Client::new(),
            Arc::clone(&cache),
            Arc::clone(&accounts),
            Arc::clone(&tokens),
        );

        let client_id = IconCode::default();
        let redirect_url = controller
            .prepare_oauth_redirect(
                &client_id,
                "http://lms.local:9000/plugins/Spotty/settings/callback",
                "Mozilla/5.0",
            )
            .await
            .expect("prepare_oauth_redirect should succeed");

        let parsed_url = Url::parse(&redirect_url).expect("redirect URL must parse");
        let params: HashMap<String, String> = parsed_url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some(client_id.as_str())
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(callback_relay_url.as_str())
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        let scopes: Vec<&str> = params
            .get("scope")
            .expect("scope param present")
            .split(' ')
            .collect();
        assert_eq!(scopes.len(), 21, "must request all 21 SPOTIFY_AUTH_SCOPES");

        let raw_state = params.get("state").expect("state param present");
        let decoded_state = STANDARD.decode(raw_state).expect("valid base64 state");
        let state_json: Value = serde_json::from_slice(&decoded_state).expect("json state");
        assert_eq!(state_json["nonce"], "relay-nonce-xyz");
        let csrf = state_json["csrf"].as_str().expect("csrf token present");
        let cache_key = pkce_verifier_cache_key(csrf);

        let Value::String(cached_verifier) = cache
            .ephemeral_value(&cache_key)
            .expect("cache read")
            .expect("verifier cached")
        else {
            panic!("expected cached PKCE verifier string");
        };
        assert_eq!(cached_verifier.len(), 128);
        let expected_challenge = PkceChallenge::from_verifier(&cached_verifier).challenge_s256;
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some(expected_challenge.as_str()),
            "code_challenge must match S256 digest of cached verifier"
        );

        let expected_verifier_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code_verifier", &cached_verifier)
            .finish();
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=auth-code-123"))
            .and(body_string_contains(expected_verifier_body.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "bq-access-tok",
                "refresh_token": "aq-refresh-tok",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("Authorization", "Bearer bq-access-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "spotty-tester",
                "uri": "spotify:user:spotty-tester",
                "display_name": "Spotty Tester",
                "product": "free"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let callback_vm = controller
            .handle_oauth_callback_verified(
                &client_id,
                Some("auth-code-123"),
                Some(raw_state),
                None,
                None,
            )
            .await
            .expect("handle_oauth_callback_verified should succeed");

        assert_eq!(callback_vm.auth_error, None);
        assert!(callback_vm.free_product);
        assert_eq!(callback_vm.skin_override, "Default");

        let check = AuthLandingController::check_authenticate_status(&accounts);
        assert!(
            check.has_credentials,
            "hasCredentials polling must report true after callback materialization"
        );

        let promoted_id = AuthLandingController::cleanup_and_promote(&accounts)
            .expect("cleanup_and_promote should succeed")
            .expect("should promote __AUTHENTICATE__");
        assert_eq!(promoted_id, AccountId::from_username("spotty-tester"));
        assert!(
            accounts
                .account_dir(&promoted_id)
                .join("credentials.json")
                .is_file(),
            "<8-hex>/credentials.json must exist after promotion"
        );

        let cached_tok = tokens
            .access_token_for(&client_id, "spotty-tester")
            .await
            .expect("access token must be cached");
        assert_eq!(cached_tok, "bq-access-tok");
    }

    #[tokio::test]
    async fn callback_without_code_returns_auth_error_viewmodel() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Arc::new(DualCache::open(dir.path().join("cache.redb")).expect("cache"));
        let accounts = Arc::new(AccountStore::new(dir.path()));
        let tokens =
            Arc::new(TokenManager::new(Arc::clone(&cache), Arc::clone(&accounts)).expect("tm"));
        let controller = PkceCallbackController::new(cache, accounts, tokens).expect("ctrl");

        let vm = controller
            .handle_oauth_callback(&IconCode::default(), None)
            .await
            .expect("returns CallbackViewModel with error");

        assert!(vm.auth_error.is_some());
        assert_eq!(vm.skin_override, "Default");
    }

    #[tokio::test]
    async fn callback_exchanges_tilde_verifier_with_percent_encoding() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let cache = Arc::new(DualCache::open(dir.path().join("cache.redb")).expect("cache"));
        let accounts = Arc::new(AccountStore::new(dir.path()));
        let tokens = Arc::new(TokenManager::with_http(
            format!("{}/api/token", server.uri()),
            Client::new(),
            Arc::clone(&cache),
            Arc::clone(&accounts),
            1_700_000_001,
        ));

        let tilde_verifier = format!("{}~{}", "a".repeat(63), "b".repeat(64));
        let csrf = "csrf-tilde-test";
        let state = STANDARD.encode(
            serde_json::to_vec(&OAuthStatePayload {
                nonce: String::from("nonce-tilde"),
                csrf: String::from(csrf),
            })
            .expect("state json"),
        );
        cache
            .put_ephemeral(
                &pkce_verifier_cache_key(csrf),
                &Value::String(tilde_verifier.clone()),
                Duration::from_secs(300),
            )
            .expect("cache put");

        let expected_verifier_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code_verifier", &tilde_verifier)
            .finish();
        assert!(
            expected_verifier_body.contains("%7E"),
            "form encoder must percent-encode '~' as '%7E'"
        );

        Mock::given(method("POST"))
            .and(path("/api/token"))
            .and(body_string_contains(expected_verifier_body.as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tilde-tok",
                "refresh_token": "tilde-rt",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("Authorization", "Bearer tilde-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "tilde-user",
                "uri": "spotify:user:tilde-user",
                "display_name": "Tilde User",
                "product": "premium"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let controller = PkceCallbackController::with_endpoints(
            format!("{}/auth/prepare", server.uri()),
            format!("{}/auth/callback", server.uri()),
            "https://accounts.spotify.com/authorize",
            format!("{}/v1", server.uri()),
            Client::new(),
            Arc::clone(&cache),
            Arc::clone(&accounts),
            Arc::clone(&tokens),
        );

        let vm = controller
            .handle_oauth_callback_verified(
                &IconCode::default(),
                Some("tilde-code"),
                Some(&state),
                None,
                None,
            )
            .await
            .expect("callback should succeed");
        assert_eq!(vm.auth_error, None);
        assert!(!vm.free_product);
        assert_eq!(vm.skin_override, "Default");
    }

    #[tokio::test]
    async fn legacy_callback_fails_closed_without_state_and_replay_is_blocked() {
        let server = MockServer::start().await;
        let dir = TempDir::new().expect("temp dir");
        let cache = Arc::new(DualCache::open(dir.path().join("cache.redb")).expect("cache"));
        let accounts = Arc::new(AccountStore::new(dir.path()));
        let tokens =
            Arc::new(TokenManager::new(Arc::clone(&cache), Arc::clone(&accounts)).expect("tm"));

        Mock::given(method("POST"))
            .and(path("/auth/prepare"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "nonce": "nonce-1"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let controller = PkceCallbackController::with_endpoints(
            format!("{}/auth/prepare", server.uri()),
            format!("{}/auth/callback", server.uri()),
            "https://accounts.spotify.com/authorize",
            format!("{}/v1", server.uri()),
            Client::new(),
            Arc::clone(&cache),
            Arc::clone(&accounts),
            Arc::clone(&tokens),
        );

        let redirect_url = controller
            .prepare_oauth_redirect(
                &IconCode::default(),
                "http://lms.local:9000/plugins/Spotty/settings/callback",
                "Mozilla/5.0",
            )
            .await
            .expect("prepare succeeds");
        let parsed = Url::parse(&redirect_url).expect("url");
        let state = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state present");

        // Legacy method must fail closed without state.
        let legacy_vm = controller
            .handle_oauth_callback(&IconCode::default(), Some("code-123"))
            .await
            .expect("returns vm");
        assert_eq!(legacy_vm.auth_error.as_deref(), Some("Missing OAuth state"));

        // Mismatched state must be rejected.
        let bad_state = STANDARD.encode(br#"{"nonce":"nonce-1","csrf":"wrong"}"#);
        let bad_vm = controller
            .handle_oauth_callback_verified(
                &IconCode::default(),
                Some("code-123"),
                Some(&bad_state),
                None,
                None,
            )
            .await
            .expect("returns vm");
        assert_eq!(
            bad_vm.auth_error.as_deref(),
            Some("Invalid or expired OAuth state")
        );

        // Consuming valid state on a missing-code failure must prevent replay.
        let missing_code_vm = controller
            .handle_oauth_callback_verified(&IconCode::default(), None, Some(&state), None, None)
            .await
            .expect("returns vm");
        assert_eq!(
            missing_code_vm.auth_error.as_deref(),
            Some("Missing authorization code")
        );

        let replay_vm = controller
            .handle_oauth_callback_verified(
                &IconCode::default(),
                Some("code-123"),
                Some(&state),
                None,
                None,
            )
            .await
            .expect("returns vm");
        assert_eq!(
            replay_vm.auth_error.as_deref(),
            Some("Invalid or expired OAuth state")
        );
    }

    #[tokio::test]
    async fn prepare_rejects_unvalidated_lms_redirect_uri_and_escapes_oauth_errors() {
        let dir = TempDir::new().expect("temp dir");
        let cache = Arc::new(DualCache::open(dir.path().join("cache.redb")).expect("cache"));
        let accounts = Arc::new(AccountStore::new(dir.path()));
        let tokens =
            Arc::new(TokenManager::new(Arc::clone(&cache), Arc::clone(&accounts)).expect("tm"));
        let controller = PkceCallbackController::new(cache, accounts, tokens).expect("ctrl");

        let bad_scheme = controller
            .prepare_oauth_redirect(
                &IconCode::default(),
                "javascript:alert(1)//plugins/Spotty/settings/callback",
                "Mozilla/5.0",
            )
            .await;
        assert!(bad_scheme.is_err());

        let bad_path = controller
            .prepare_oauth_redirect(
                &IconCode::default(),
                "https://evil.example/steal",
                "Mozilla/5.0",
            )
            .await;
        assert!(bad_path.is_err());

        let err_vm = controller
            .handle_oauth_callback_verified(
                &IconCode::default(),
                None,
                None,
                Some("access_denied"),
                Some("<script>alert('xss')</script>"),
            )
            .await
            .expect("returns vm");
        let auth_err = err_vm.auth_error.expect("auth_error present");
        assert!(!auth_err.contains('<'));
        assert!(!auth_err.contains('>'));
        assert!(auth_err.contains("&lt;script&gt;"));
    }
}
