//! HTTP transport, RFC 3986 query serializer, conditional `ETag` caching, and
//! dual 429 rate-limit gate.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use md5::Md5;
use rand::Rng as _;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use spotty_cache::{CachedHttpEntry, DualCache};
use spotty_types::{IconCode, Result, SpottyError};
use tracing::{debug, warn};

use crate::token::TokenManager;

/// Default Spotify Web API v1 base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://api.spotify.com/v1";

/// Default HTTP request timeout in seconds (`API.pm:1225`).
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

const DEFAULT_CACHE_TTL_SECS: u64 = 60;
const USER_ENDPOINT_TTL_SECS: u64 = 3600;
const EDITORIAL_PLAYLIST_DEFAULT_ICON_TTL_SECS: u64 = 8 * 3600;
const DEFAULT_RETRY_AFTER_SECS: u64 = 5;
const MAX_RETRY_AFTER_SECS: u64 = 120;
const MAX_SCANNER_GATE_WAITS: usize = 4;

/// Policy governing how HTTP 429 rate-limit responses are handled.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitPolicy {
    /// Interactive LMS server mode: trip the shared cooldown gate and fail fast.
    FailFastServer,
    /// Background library scanner mode: sleep `retry_after + 1s` and retry once.
    SleepAndRetryScanner,
}

/// Controls HTTP response caching and conditional revalidation behavior.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Serve fresh cache hits and revalidate expired entries via `ETag`/`IMS`.
    UseCache,
    /// Skip cache reads and writes for live endpoints such as `me/player`.
    BypassCache,
    /// Serve existing cached entries (even if stale) without revalidating.
    SkipRevalidate,
}

/// Shared monotonic cooldown gate replacing `spotty_rate_limit_exceeded`.
#[derive(Debug, Default)]
pub struct RateLimitGate {
    cooldown_until: Mutex<Option<Instant>>,
}

impl RateLimitGate {
    /// Creates an untripped [`RateLimitGate`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `Some(remaining_secs)` if a cooldown window is currently active.
    #[must_use]
    pub fn is_tripped(&self) -> Option<u64> {
        let mut guard = self
            .cooldown_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let deadline = (*guard)?;
        let now = Instant::now();
        if deadline <= now {
            *guard = None;
            return None;
        }
        let remaining = deadline.duration_since(now);
        let secs = remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0));
        Some(secs.max(1))
    }

    /// Arms the cooldown gate for at least `secs.max(1)` seconds from now.
    pub fn trip(&self, secs: u64) {
        let clamped = secs.max(1);
        let new_deadline = Instant::now() + Duration::from_secs(clamped);
        let mut guard = self
            .cooldown_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = match *guard {
            Some(existing) if existing > new_deadline => Some(existing),
            _ => Some(new_deadline),
        };
    }

    /// Clears any active cooldown window.
    pub fn clear(&self) {
        let mut guard = self
            .cooldown_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = None;
    }

    pub(crate) fn clear_if_expired(&self) {
        let mut guard = self
            .cooldown_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(deadline) = *guard {
            if deadline <= Instant::now() {
                *guard = None;
            }
        }
    }
}

/// HTTP response returned by [`SpotifyTransport`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponse {
    /// HTTP status code (`200` for fresh hits or `304 Not Modified` cache hits).
    pub status: u16,
    /// Raw UTF-8 JSON (or empty) response body.
    pub body: String,
    /// `true` when served from [`DualCache`] or revalidated via `304`.
    pub from_cache: bool,
}

impl TransportResponse {
    /// Deserializes `body` as `T`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Json`] if `body` is not valid JSON for `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        let parsed: T = serde_json::from_str(&self.body)?;
        Ok(parsed)
    }
}

/// Spotify Web API v1 `reqwest` transport with RFC 3986 serialization,
/// conditional caching, and 429 rate-limit handling.
#[derive(Debug)]
pub struct SpotifyTransport {
    http_client: Client,
    base_url: String,
    tokens: Arc<TokenManager>,
    cache: Arc<DualCache>,
    rate_limit_gate: Arc<RateLimitGate>,
    rate_limit_policy: RateLimitPolicy,
    client_id: IconCode,
    user_id: String,
}

impl SpotifyTransport {
    /// Creates a [`SpotifyTransport`] targeting [`DEFAULT_API_BASE_URL`] with
    /// `rustls-tls`, gzip compression, a 30-second timeout, and TLS verification
    /// enabled.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Http`] if building the `reqwest` client fails.
    pub fn new(
        tokens: Arc<TokenManager>,
        cache: Arc<DualCache>,
        rate_limit_gate: Arc<RateLimitGate>,
        client_id: IconCode,
        user_id: impl Into<String>,
    ) -> Result<Self> {
        Self::with_base_url(
            DEFAULT_API_BASE_URL,
            tokens,
            cache,
            rate_limit_gate,
            client_id,
            user_id,
        )
    }

    /// Creates a [`SpotifyTransport`] with an explicit API `base_url` (for
    /// `wiremock` integration tests).
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::Http`] if building the `reqwest` client fails.
    pub fn with_base_url(
        base_url: impl Into<String>,
        tokens: Arc<TokenManager>,
        cache: Arc<DualCache>,
        rate_limit_gate: Arc<RateLimitGate>,
        client_id: IconCode,
        user_id: impl Into<String>,
    ) -> Result<Self> {
        let http_client = tokens.http_client().clone();

        Ok(Self {
            http_client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            tokens,
            cache,
            rate_limit_gate,
            rate_limit_policy: RateLimitPolicy::FailFastServer,
            client_id,
            user_id: user_id.into(),
        })
    }

    /// Sets the HTTP 429 [`RateLimitPolicy`] on this transport.
    #[must_use]
    pub fn with_rate_limit_policy(mut self, policy: RateLimitPolicy) -> Self {
        self.rate_limit_policy = policy;
        self
    }

    /// Returns the configured Spotify API base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the configured [`RateLimitPolicy`].
    #[must_use]
    pub fn rate_limit_policy(&self) -> RateLimitPolicy {
        self.rate_limit_policy
    }

    /// Returns a reference to the shared [`RateLimitGate`].
    #[must_use]
    pub fn rate_limit_gate(&self) -> &Arc<RateLimitGate> {
        &self.rate_limit_gate
    }

    /// Performs a cached or live `GET` request against `path`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::RateLimited`] on active cooldowns or HTTP 429
    /// exhaustion, [`SpottyError::NoAccessToken`] / [`SpottyError::InvalidGrant`]
    /// on auth failures, or [`SpottyError::Http`] / [`SpottyError::Cache`] on
    /// transport or storage errors.
    pub async fn get(
        &self,
        path: &str,
        params: &[(String, String)],
        cache_policy: CachePolicy,
    ) -> Result<TransportResponse> {
        self.request(Method::GET, path, params, None, cache_policy)
            .await
    }

    /// Executes an authenticated HTTP request against Spotify Web API `path`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError::RateLimited`], [`SpottyError::NoAccessToken`],
    /// [`SpottyError::InvalidGrant`], [`SpottyError::Http`], or
    /// [`SpottyError::Cache`].
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        body: Option<String>,
        cache_policy: CachePolicy,
    ) -> Result<TransportResponse> {
        let clean_path = path.trim_start_matches('/');
        for _ in 0..MAX_SCANNER_GATE_WAITS {
            let Some(remaining) = self.rate_limit_gate.is_tripped() else {
                break;
            };
            match self.rate_limit_policy {
                RateLimitPolicy::FailFastServer => {
                    return Err(SpottyError::RateLimited {
                        retry_after_secs: remaining,
                        endpoint: clean_path.to_owned(),
                    });
                }
                RateLimitPolicy::SleepAndRetryScanner => {
                    let wait = Duration::from_secs(remaining.min(MAX_RETRY_AFTER_SECS))
                        + jitter_duration();
                    tokio::time::sleep(wait).await;
                }
            }
        }
        if let Some(remaining) = self.rate_limit_gate.is_tripped() {
            return Err(SpottyError::RateLimited {
                retry_after_secs: remaining,
                endpoint: clean_path.to_owned(),
            });
        }

        let token = self
            .tokens
            .access_token_for(&self.client_id, &self.user_id)
            .await?;

        let query = serialize_query(params);
        let relative_url = if query.is_empty() {
            clean_path.to_owned()
        } else {
            format!("{clean_path}?{query}")
        };

        let use_http_cache = method == Method::GET && cache_policy != CachePolicy::BypassCache;
        let cache_key = if use_http_cache {
            Some(compute_http_cache_key(&relative_url, &self.user_id))
        } else {
            None
        };

        let mut stale_entry = None;
        if let Some(key) = &cache_key {
            if let Some(fresh) = self.cache.http_entry(key)? {
                return Ok(TransportResponse {
                    status: 200,
                    body: fresh.body,
                    from_cache: true,
                });
            }
            stale_entry = self.cache.http_entry_stale(key)?;
            if cache_policy == CachePolicy::SkipRevalidate {
                if let Some(stale) = stale_entry {
                    return Ok(TransportResponse {
                        status: 200,
                        body: stale.body,
                        from_cache: true,
                    });
                }
            }
        }

        let full_url = format!("{}/{}", self.base_url, relative_url);
        let mut outcome = self
            .send_with_rate_limit_policy(
                &method,
                &full_url,
                clean_path,
                &token,
                body.as_deref(),
                stale_entry.as_ref(),
            )
            .await?;

        if outcome.status == StatusCode::UNAUTHORIZED {
            self.tokens
                .invalidate_access_token(&self.client_id, &self.user_id)?;
            let refreshed = self
                .tokens
                .access_token_for(&self.client_id, &self.user_id)
                .await?;
            outcome = self
                .send_with_rate_limit_policy(
                    &method,
                    &full_url,
                    clean_path,
                    &refreshed,
                    body.as_deref(),
                    stale_entry.as_ref(),
                )
                .await?;
        }

        if outcome.status == StatusCode::NOT_MODIFIED {
            if let Some(stale) = stale_entry {
                let ttl = self.resolve_cache_ttl(clean_path, outcome.cache_control.as_deref());
                let updated = CachedHttpEntry::new(
                    stale.body.clone(),
                    outcome.etag.or(stale.etag),
                    outcome.last_modified.or(stale.last_modified),
                    current_epoch_secs().saturating_add(ttl),
                );
                if let Some(key) = &cache_key {
                    self.cache.put_http_entry(key, &updated)?;
                }
                return Ok(TransportResponse {
                    status: 200,
                    body: stale.body,
                    from_cache: true,
                });
            }
        }

        if !outcome.status.is_success() {
            return Err(SpottyError::Http(format!(
                "HTTP {} from {clean_path}: {}",
                outcome.status.as_u16(),
                outcome.body
            )));
        }

        if let Some(key) = &cache_key {
            let ttl = self.resolve_cache_ttl(clean_path, outcome.cache_control.as_deref());
            let entry = CachedHttpEntry::new(
                outcome.body.clone(),
                outcome.etag,
                outcome.last_modified,
                current_epoch_secs().saturating_add(ttl),
            );
            self.cache.put_http_entry(key, &entry)?;
        }

        Ok(TransportResponse {
            status: outcome.status.as_u16(),
            body: outcome.body,
            from_cache: false,
        })
    }

    /// Resolves cache TTL from `Cache-Control: max-age` and Spotify route
    /// fallbacks (`60s` default, `3600s` `/me` & user playlists, `28800s`/`3600s`
    /// editorial playlists).
    #[must_use]
    pub fn resolve_cache_ttl(&self, clean_path: &str, cache_control: Option<&str>) -> u64 {
        if let Some(max_age) = cache_control.and_then(parse_max_age) {
            if max_age > 0 {
                return max_age;
            }
        }

        if let Some(playlist_id) = extract_playlist_tracks_id(clean_path) {
            return self.playlist_tracks_ttl(playlist_id);
        }

        if clean_path == "me" {
            return USER_ENDPOINT_TTL_SECS;
        }

        DEFAULT_CACHE_TTL_SECS
    }

    fn playlist_tracks_ttl(&self, playlist_id: &str) -> u64 {
        let owner_key = format!("playlist_owner_{playlist_id}");
        let owner = self
            .cache
            .ephemeral_value(&owner_key)
            .ok()
            .flatten()
            .and_then(|val| match val {
                Value::String(s) => Some(s),
                _ => None,
            });

        if matches!(owner.as_deref(), Some("spotify" | "spotifycharts")) {
            if self.client_id.is_default_icon() {
                return EDITORIAL_PLAYLIST_DEFAULT_ICON_TTL_SECS;
            }
            return USER_ENDPOINT_TTL_SECS;
        }

        USER_ENDPOINT_TTL_SECS
    }

    async fn send_with_rate_limit_policy(
        &self,
        method: &Method,
        full_url: &str,
        clean_path: &str,
        token: &str,
        body: Option<&str>,
        stale_entry: Option<&CachedHttpEntry>,
    ) -> Result<RawHttpOutcome> {
        let first = self
            .send_once(method, full_url, token, body, stale_entry)
            .await?;

        if first.status != StatusCode::TOO_MANY_REQUESTS {
            if first.status.is_success() || first.status == StatusCode::NOT_MODIFIED {
                self.rate_limit_gate.clear_if_expired();
            }
            return Ok(first);
        }

        let retry_after = first
            .retry_after_secs
            .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
            .min(MAX_RETRY_AFTER_SECS);
        match self.rate_limit_policy {
            RateLimitPolicy::FailFastServer => {
                warn!(
                    endpoint = %clean_path,
                    retry_after_secs = retry_after,
                    "Spotify API returned 429; tripping RateLimitGate"
                );
                self.rate_limit_gate.trip(retry_after);
                Err(SpottyError::RateLimited {
                    retry_after_secs: retry_after,
                    endpoint: clean_path.to_owned(),
                })
            }
            RateLimitPolicy::SleepAndRetryScanner => {
                let wait_secs = retry_after.saturating_add(1).min(MAX_RETRY_AFTER_SECS);
                self.rate_limit_gate.trip(wait_secs);
                debug!(
                    endpoint = %clean_path,
                    wait_secs,
                    "Scanner received 429; sleeping before single retry"
                );
                tokio::time::sleep(Duration::from_secs(wait_secs) + jitter_duration()).await;
                let second = self
                    .send_once(method, full_url, token, body, stale_entry)
                    .await?;
                if second.status == StatusCode::TOO_MANY_REQUESTS {
                    let second_retry = second
                        .retry_after_secs
                        .unwrap_or(retry_after)
                        .min(MAX_RETRY_AFTER_SECS);
                    self.rate_limit_gate.trip(second_retry);
                    return Err(SpottyError::RateLimited {
                        retry_after_secs: second_retry,
                        endpoint: clean_path.to_owned(),
                    });
                }
                if second.status.is_success() || second.status == StatusCode::NOT_MODIFIED {
                    self.rate_limit_gate.clear_if_expired();
                }
                Ok(second)
            }
        }
    }

    async fn send_once(
        &self,
        method: &Method,
        full_url: &str,
        token: &str,
        body: Option<&str>,
        stale_entry: Option<&CachedHttpEntry>,
    ) -> Result<RawHttpOutcome> {
        let mut req = self
            .http_client
            .request(method.clone(), full_url)
            .header("Accept", "application/json")
            .bearer_auth(token);

        if let Some(entry) = stale_entry {
            if let Some(etag) = &entry.etag {
                req = req.header("If-None-Match", strip_gzip_etag(etag));
            }
            if let Some(last_modified) = &entry.last_modified {
                req = req.header("If-Modified-Since", last_modified);
            }
        }

        if let Some(payload) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(payload.to_owned());
        }

        let response = req
            .send()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        let status = response.status();
        let headers = response.headers();
        let etag = header_str(headers, "etag");
        let last_modified = header_str(headers, "last-modified");
        let cache_control = header_str(headers, "cache-control");
        let retry_after_secs = header_str(headers, "retry-after")
            .and_then(|raw| parse_retry_after_at(&raw, current_epoch_secs()));

        let text = response
            .text()
            .await
            .map_err(|err| SpottyError::Http(err.to_string()))?;

        Ok(RawHttpOutcome {
            status,
            body: text,
            etag,
            last_modified,
            cache_control,
            retry_after_secs,
        })
    }
}

struct RawHttpOutcome {
    status: StatusCode,
    body: String,
    etag: Option<String>,
    last_modified: Option<String>,
    cache_control: Option<String>,
    retry_after_secs: Option<u64>,
}

/// Serializes query parameters deterministically in sorted key/pair order using
/// RFC 3986 percent-encoding (encoding `~` as `%7E` and skipping `_`-prefixed
/// internal keys).
#[must_use]
pub fn serialize_query(params: &[(String, String)]) -> String {
    let mut pairs: Vec<(&str, &str)> = Vec::with_capacity(params.len());
    for (key, value) in params {
        if key.starts_with('_') {
            continue;
        }
        pairs.push((key.as_str(), value.as_str()));
    }
    pairs.sort_unstable();

    let mut encoded = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        encoded.push(format!("{}={}", encode_rfc3986(key), encode_rfc3986(value)));
    }
    encoded.sort_unstable();
    encoded.join("&")
}

fn encode_rfc3986(input: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let is_unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.');
        if is_unreserved {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub(crate) fn compute_http_cache_key(relative_url: &str, user_scope: &str) -> String {
    use md5::Digest as _;

    let mut hasher = Md5::new();
    hasher.update(relative_url.as_bytes());
    if is_user_scoped_path(relative_url) {
        hasher.update(user_scope.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(32);
    for byte in digest {
        let _ = fmt::write(&mut hex, format_args!("{byte:02x}"));
    }
    hex
}

fn is_user_scoped_path(relative_url: &str) -> bool {
    let path_only = relative_url.split('?').next().unwrap_or(relative_url);
    path_only == "me"
        || path_only.starts_with("me/")
        || path_only == "browse"
        || path_only.starts_with("browse/")
        || path_only == "playlists"
        || path_only.starts_with("playlists/")
}

fn jitter_duration() -> Duration {
    let ms = rand::thread_rng().gen_range(0..500);
    Duration::from_millis(ms)
}

fn parse_retry_after_at(raw: &str, now_epoch_secs: u64) -> Option<u64> {
    let trimmed = raw.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(secs.min(MAX_RETRY_AFTER_SECS));
    }
    let target_epoch = parse_http_date_epoch(trimmed)?;
    let delta = target_epoch.saturating_sub(now_epoch_secs);
    Some(delta.min(MAX_RETRY_AFTER_SECS))
}

fn parse_http_date_epoch(raw: &str) -> Option<u64> {
    // Supports IMF-fixdate ("Wed, 21 Oct 2015 07:28:00 GMT") and RFC 850.
    let without_weekday = raw.split_once(',').map_or(raw, |(_, rest)| rest).trim();
    let parts: Vec<&str> = without_weekday.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u64 = parts[0].parse().ok()?;
    let month: u64 = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[2].parse().ok()?;
    let mut hms = parts[3].split(':');
    let hour: u64 = hms.next()?.parse().ok()?;
    let min: u64 = hms.next()?.parse().ok()?;
    let sec: u64 = hms.next()?.parse().ok()?;
    if hms.next().is_some()
        || year < 1970
        || month == 0
        || day == 0
        || hour > 23
        || min > 59
        || sec > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec)
}

fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    let mut y = i64::try_from(year).ok()?;
    let m = i64::try_from(month).ok()?;
    let d = i64::try_from(day).ok()?;
    if m <= 2 {
        y -= 1;
    }
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let unix_days = era * 146_097 + doe - 719_468;
    u64::try_from(unix_days).ok()
}

fn strip_gzip_etag(etag: &str) -> String {
    if let Some(prefix) = etag.strip_suffix("-gzip\"") {
        format!("{prefix}\"")
    } else if let Some(prefix) = etag.strip_suffix("-gzip") {
        prefix.to_owned()
    } else {
        etag.to_owned()
    }
}

fn parse_max_age(cache_control: &str) -> Option<u64> {
    for directive in cache_control.split(',') {
        let trimmed = directive.trim();
        let Some(val) = trimmed.strip_prefix("max-age=") else {
            continue;
        };
        if let Ok(secs) = val.trim().parse::<u64>() {
            return Some(secs);
        }
    }
    None
}

fn extract_playlist_tracks_id(clean_path: &str) -> Option<&str> {
    let rest = clean_path.strip_prefix("playlists/")?;
    let id = rest.strip_suffix("/tracks")?;
    if id.len() == 22 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Some(id)
    } else {
        None
    }
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|val| val.to_str().ok())
        .map(str::to_owned)
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::json;
    use spotty_cache::{AccountStore, CachedHttpEntry, DualCache};
    use spotty_types::{IconCode, SpottyError};
    use tempfile::TempDir;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        compute_http_cache_key, serialize_query, CachePolicy, RateLimitGate, RateLimitPolicy,
        SpotifyTransport,
    };
    use crate::token::TokenManager;

    fn build_transport(
        mock_uri: &str,
        tmp: &TempDir,
        policy: RateLimitPolicy,
    ) -> (SpotifyTransport, Arc<DualCache>, Arc<RateLimitGate>) {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let accounts = Arc::new(AccountStore::new(tmp.path()));
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build client");
        let tokens = Arc::new(TokenManager::with_http(
            format!("{mock_uri}/api/token"),
            client,
            Arc::clone(&cache),
            accounts,
            1_700_000_000,
        ));
        let client_id = IconCode::default();
        tokens
            .cache_tokens(&client_id, "alice", "bearer-tok-1", 3600, None)
            .expect("seed access token");

        let gate = Arc::new(RateLimitGate::new());
        let transport = SpotifyTransport::with_base_url(
            format!("{mock_uri}/v1"),
            tokens,
            Arc::clone(&cache),
            Arc::clone(&gate),
            client_id,
            "alice",
        )
        .expect("build SpotifyTransport")
        .with_rate_limit_policy(policy);

        (transport, cache, gate)
    }

    #[test]
    fn serialize_query_sorts_skips_internal_and_encodes_tilde() {
        let params = vec![
            (String::from("_nocache"), String::from("1")),
            (String::from("z_key"), String::from("a~b c")),
            (String::from("alpha"), String::from("1,2")),
            (String::from("_token"), String::from("secret")),
        ];
        let encoded = serialize_query(&params);
        assert_eq!(encoded, "alpha=1%2C2&z_key=a%7Eb%20c");
    }

    #[test]
    fn cache_key_salts_me_browse_and_playlists_with_user_scope() {
        let me_a = compute_http_cache_key("me/albums?limit=50", "alice");
        let me_b = compute_http_cache_key("me/albums?limit=50", "bob");
        assert_ne!(me_a, me_b, "me* keys must be user-salted");

        let browse_a = compute_http_cache_key("browse/categories", "alice");
        let browse_b = compute_http_cache_key("browse/categories", "bob");
        assert_ne!(browse_a, browse_b, "browse* keys must be user-salted");

        let pl_a = compute_http_cache_key("playlists/123/tracks?market=from_token", "alice");
        let pl_b = compute_http_cache_key("playlists/123/tracks?market=from_token", "bob");
        assert_ne!(pl_a, pl_b, "playlists/* keys must be user-salted");

        let track_a = compute_http_cache_key("tracks?ids=123", "alice");
        let track_b = compute_http_cache_key("tracks?ids=123", "bob");
        assert_eq!(track_a, track_b, "non-user keys must be shared");
    }

    #[test]
    fn route_fallback_ttls_match_spotify_rules() {
        let tmp = TempDir::new().expect("tempdir");
        let (transport, cache, _) =
            build_transport("http://127.0.0.1:9", &tmp, RateLimitPolicy::FailFastServer);

        assert_eq!(
            transport.resolve_cache_ttl("tracks", Some("public, max-age=120")),
            120
        );
        assert_eq!(transport.resolve_cache_ttl("tracks", None), 60);
        assert_eq!(transport.resolve_cache_ttl("me", Some("max-age=0")), 3600);

        let user_pl = "1234567890123456789012";
        cache
            .put_ephemeral(
                &format!("playlist_owner_{user_pl}"),
                &json!("alice"),
                Duration::from_secs(3600),
            )
            .expect("seed user playlist owner");
        assert_eq!(
            transport.resolve_cache_ttl(&format!("playlists/{user_pl}/tracks"), None),
            3600
        );

        let editorial_pl = "37i9dQZF1DXcBWIGoYBM5M";
        cache
            .put_ephemeral(
                &format!("playlist_owner_{editorial_pl}"),
                &json!("spotify"),
                Duration::from_secs(3600),
            )
            .expect("seed editorial playlist owner");
        assert_eq!(
            transport.resolve_cache_ttl(&format!("playlists/{editorial_pl}/tracks"), None),
            28_800
        );
    }

    #[tokio::test]
    async fn rate_limit_429_fail_fast_trips_gate_and_blocks_followup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (transport, _, gate) =
            build_transport(&server.uri(), &tmp, RateLimitPolicy::FailFastServer);

        let err = transport
            .get("search", &[], CachePolicy::BypassCache)
            .await
            .expect_err("429 must fail fast");
        assert_eq!(
            err,
            SpottyError::RateLimited {
                retry_after_secs: 2,
                endpoint: String::from("search"),
            }
        );
        assert!(gate.is_tripped().is_some(), "RateLimitGate must be tripped");

        // Follow-up request fails immediately without hitting mock server.
        let blocked = transport
            .get("search", &[], CachePolicy::BypassCache)
            .await
            .expect_err("tripped gate must short-circuit");
        assert!(matches!(blocked, SpottyError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn rate_limit_429_sleep_and_retry_scanner_retries_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/albums"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (transport, _, _) =
            build_transport(&server.uri(), &tmp, RateLimitPolicy::SleepAndRetryScanner);

        let resp = transport
            .get("me/albums", &[], CachePolicy::BypassCache)
            .await
            .expect("scanner retry should succeed on second attempt");
        assert_eq!(resp.status, 200);
        assert!(!resp.from_cache);
    }

    #[tokio::test]
    async fn conditional_get_304_strips_gzip_etag_and_serves_cached_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("If-None-Match", "\"etag-v1\""))
            .respond_with(ResponseTemplate::new(304).insert_header("Cache-Control", "max-age=3600"))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (transport, cache, _) =
            build_transport(&server.uri(), &tmp, RateLimitPolicy::FailFastServer);

        let cache_key = compute_http_cache_key("me", "alice");
        let expired_entry = CachedHttpEntry::new(
            r#"{"id":"alice","country":"US"}"#,
            Some(String::from("\"etag-v1-gzip\"")),
            Some(String::from("Wed, 19 Nov 2026 12:00:00 GMT")),
            1,
        );
        cache
            .put_http_entry(&cache_key, &expired_entry)
            .expect("seed expired HTTP entry");

        let resp = transport
            .get("me", &[], CachePolicy::UseCache)
            .await
            .expect("304 revalidation should succeed");
        assert!(resp.from_cache);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, r#"{"id":"alice","country":"US"}"#);
    }

    #[test]
    fn retry_after_parses_http_date_caps_max_and_clamps_trip_zero() {
        let now = 1_700_000_000u64;
        assert_eq!(super::parse_retry_after_at("15", now), Some(15));
        assert_eq!(
            super::parse_retry_after_at("9999", now),
            Some(super::MAX_RETRY_AFTER_SECS)
        );
        // 1_700_000_010 == Tue, 14 Nov 2023 22:13:30 GMT
        assert_eq!(
            super::parse_retry_after_at("Tue, 14 Nov 2023 22:13:30 GMT", now),
            Some(10)
        );

        let gate = RateLimitGate::new();
        gate.trip(0);
        assert!(
            gate.is_tripped().is_some(),
            "trip(0) must arm gate for at least 1s"
        );
    }

    #[tokio::test]
    async fn server_500_does_not_clear_active_rate_limit_gate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (transport, _, gate) =
            build_transport(&server.uri(), &tmp, RateLimitPolicy::FailFastServer);

        // First call fails with 500; verify clear_if_expired does not clear an
        // active future cooldown.
        let outcome = transport.get("search", &[], CachePolicy::BypassCache).await;
        assert!(outcome.is_err());

        gate.trip(10);
        gate.clear_if_expired();
        assert!(
            gate.is_tripped().is_some(),
            "clear_if_expired must not clear active future cooldown"
        );
    }

    #[tokio::test]
    async fn unauthorized_401_evicts_cached_token_and_retries_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "fresh-tok-2",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("Authorization", "Bearer bearer-tok-1"))
            .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .and(header("Authorization", "Bearer fresh-tok-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "alice"})))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (transport, _, _) =
            build_transport(&server.uri(), &tmp, RateLimitPolicy::FailFastServer);
        transport
            .tokens
            .cache_tokens(
                &transport.client_id,
                &transport.user_id,
                "bearer-tok-1",
                3600,
                Some("seed-rt"),
            )
            .expect("seed tokens");

        let resp = transport
            .get("me", &[], CachePolicy::BypassCache)
            .await
            .expect("401 should evict token and succeed on retry");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, r#"{"id":"alice"}"#);
    }
}
