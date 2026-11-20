//! Deterministic async paginator and indexed batch executor over
//! [`SpotifyTransport`].

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::future::try_join_all;
use serde_json::Value;
use spotty_types::Result;
use tokio::sync::Semaphore;

use crate::transport::{CachePolicy, SpotifyTransport};

/// Default maximum concurrent requests dispatched by [`Pipeline`].
pub const DEFAULT_MAX_CONCURRENCY: usize = 6;

/// Default maximum sequential pagination iterations before halting.
pub const DEFAULT_MAX_ITERATIONS: usize = 250;

const AFTER_CURSOR_LEN: usize = 22;

/// Extracted page returned by offset-paginated Spotify endpoints.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct OffsetPage<T> {
    /// Decoded items for this page.
    pub items: Vec<T>,
    /// Total items reported by the endpoint.
    pub total: usize,
    /// Optional `next` pagination URL.
    pub next: Option<String>,
}

impl<T> OffsetPage<T> {
    /// Creates an [`OffsetPage`] from its components.
    #[must_use]
    pub fn new(items: Vec<T>, total: usize, next: Option<String>) -> Self {
        Self { items, total, next }
    }
}

/// Extracted page returned by cursor-paginated (`after=`) Spotify endpoints.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CursorPage<T> {
    /// Decoded items for this page.
    pub items: Vec<T>,
    /// Optional `next` pagination URL containing `after=<22-char>`.
    pub next: Option<String>,
    /// Optional explicit `cursors.after` token.
    pub after: Option<String>,
}

impl<T> CursorPage<T> {
    /// Creates a [`CursorPage`] from its components.
    #[must_use]
    pub fn new(items: Vec<T>, next: Option<String>, after: Option<String>) -> Self {
        Self { items, next, after }
    }
}

/// Deterministic async paginator and batch chunk executor over
/// [`SpotifyTransport`].
#[derive(Debug, Clone)]
pub struct Pipeline {
    transport: Arc<SpotifyTransport>,
    max_concurrency: usize,
    max_iterations: usize,
}

impl Pipeline {
    /// Creates a [`Pipeline`] wrapping `transport` with default concurrency
    /// and iteration bounds.
    #[must_use]
    pub fn new(transport: Arc<SpotifyTransport>) -> Self {
        Self {
            transport,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_iterations: DEFAULT_MAX_ITERATIONS,
        }
    }

    /// Sets the maximum parallel request concurrency (clamped to `>= 1`).
    #[must_use]
    pub fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency.max(1);
        self
    }

    /// Sets the maximum pagination iteration bound (clamped to `>= 1`).
    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations.max(1);
        self
    }

    pub(crate) fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// Probes page `0`, propagates [`CachePolicy::SkipRevalidate`] when page `0`
    /// came from cache, and fans out remaining offset pages concurrently under a
    /// [`Semaphore`], sorting results deterministically by `usize` offset.
    ///
    /// # Errors
    ///
    /// Returns any transport, JSON parsing, or extractor error encountered on
    /// page `0` or any parallel page.
    pub async fn fetch_offset_parallel<T, F>(
        &self,
        path: &str,
        base_params: &[(String, String)],
        limit: usize,
        max_items: usize,
        cache_policy: CachePolicy,
        extract: F,
    ) -> Result<Vec<T>>
    where
        T: Send,
        F: Fn(&Value) -> Result<OffsetPage<T>> + Send + Sync,
    {
        let effective_max = max_items.max(1);
        let page_size = limit.max(1).min(effective_max);
        let page0_params = with_offset_and_limit(base_params, 0, page_size);
        let resp0 = self
            .transport
            .get(path, &page0_params, cache_policy)
            .await?;
        let json0: Value = serde_json::from_str(&resp0.body)?;
        let page0 = extract(&json0)?;

        let target_total = page0.total.min(effective_max);
        let raw_items_empty = json0
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty);
        if target_total <= page_size || raw_items_empty {
            return Ok(page0.items);
        }

        let follow_policy = if resp0.from_cache && cache_policy == CachePolicy::UseCache {
            CachePolicy::SkipRevalidate
        } else {
            cache_policy
        };

        let mut offsets = Vec::new();
        let mut offset = page_size;
        while offset < target_total && offsets.len() + 1 < self.max_iterations {
            offsets.push(offset);
            offset = offset.saturating_add(page_size);
        }

        let semaphore = Arc::new(Semaphore::new(self.max_concurrency));
        let extract_ref = &extract;
        let futures = offsets.into_iter().map(|page_offset| {
            let sem = Arc::clone(&semaphore);
            let params = with_offset_and_limit(base_params, page_offset, page_size);
            async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|err| spotty_types::SpottyError::Io(err.to_string()))?;
                let resp = self.transport.get(path, &params, follow_policy).await?;
                let json: Value = serde_json::from_str(&resp.body)?;
                let page = extract_ref(&json)?;
                Ok::<(usize, Vec<T>), spotty_types::SpottyError>((page_offset, page.items))
            }
        });

        let mut fetched_pages = try_join_all(futures).await?;
        fetched_pages.push((0, page0.items));
        fetched_pages.sort_by_key(|(page_offset, _)| *page_offset);

        let mut all_items = Vec::new();
        for (_, items) in fetched_pages {
            all_items.extend(items);
        }
        Ok(all_items)
    }

    /// Sequentially follows `after=<22-char>` cursors until exhausted or
    /// `max_iterations` is reached.
    ///
    /// # Errors
    ///
    /// Returns any transport, JSON parsing, or extractor error encountered on
    /// any cursor page.
    pub async fn fetch_cursor_serial<T, F>(
        &self,
        path: &str,
        base_params: &[(String, String)],
        cache_policy: CachePolicy,
        extract: F,
    ) -> Result<Vec<T>>
    where
        F: Fn(&Value) -> Result<CursorPage<T>>,
    {
        let mut all_items = Vec::new();
        let mut seen_cursors = HashSet::new();
        let mut current_after: Option<String> = None;
        let mut current_policy = cache_policy;

        for _ in 0..self.max_iterations {
            let params = match &current_after {
                Some(cursor) => with_after_cursor(base_params, cursor),
                None => base_params.to_vec(),
            };
            let resp = self.transport.get(path, &params, current_policy).await?;
            if resp.from_cache && current_policy == CachePolicy::UseCache {
                current_policy = CachePolicy::SkipRevalidate;
            }

            let json: Value = serde_json::from_str(&resp.body)?;
            let page = extract(&json)?;
            all_items.extend(page.items);

            let Some(next_cursor) =
                extract_after_cursor(page.next.as_deref(), page.after.as_deref())
            else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                break;
            }
            current_after = Some(next_cursor);
        }

        Ok(all_items)
    }

    /// Executes explicitly indexed `(usize, Vec<(String, String)>)` chunks
    /// concurrently and reassembles results strictly in ascending `usize` order.
    ///
    /// # Errors
    ///
    /// Returns any transport, JSON parsing, or extractor error encountered by
    /// any chunk request.
    pub async fn execute_indexed_chunks<T, F>(
        &self,
        path: &str,
        chunks: &[(usize, Vec<(String, String)>)],
        cache_policy: CachePolicy,
        extract: F,
    ) -> Result<Vec<T>>
    where
        T: Send,
        F: Fn(&Value) -> Result<Vec<T>> + Send + Sync,
    {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let semaphore = Arc::new(Semaphore::new(self.max_concurrency));
        let extract_ref = &extract;
        let futures = chunks.iter().map(|(chunk_idx, params)| {
            let sem = Arc::clone(&semaphore);
            let idx = *chunk_idx;
            let owned_params = params.clone();
            async move {
                let _permit = sem
                    .acquire()
                    .await
                    .map_err(|err| spotty_types::SpottyError::Io(err.to_string()))?;
                let resp = self
                    .transport
                    .get(path, &owned_params, cache_policy)
                    .await?;
                let json: Value = serde_json::from_str(&resp.body)?;
                let items = extract_ref(&json)?;
                Ok::<(usize, Vec<T>), spotty_types::SpottyError>((idx, items))
            }
        });

        let mut indexed = try_join_all(futures).await?;
        indexed.sort_by_key(|(idx, _)| *idx);

        let mut out = Vec::new();
        for (_, items) in indexed {
            out.extend(items);
        }
        Ok(out)
    }
}

/// Extracts a 22-character alphanumeric `after` cursor from `explicit_after`
/// or a `next` URL (ignoring URLs containing `offset=`).
#[must_use]
pub fn extract_after_cursor(
    next_url: Option<&str>,
    explicit_after: Option<&str>,
) -> Option<String> {
    if let Some(after) = explicit_after {
        if is_valid_cursor_token(after) {
            return Some(after.to_owned());
        }
    }

    let url = next_url?;
    if url.contains("offset=") {
        return None;
    }

    let query = url.split('?').nth(1).unwrap_or(url);
    for pair in query.split('&') {
        let Some(val) = pair.strip_prefix("after=") else {
            continue;
        };
        if is_valid_cursor_token(val) {
            return Some(val.to_owned());
        }
    }
    None
}

fn is_valid_cursor_token(token: &str) -> bool {
    token.len() == AFTER_CURSOR_LEN && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn with_offset_and_limit(
    base_params: &[(String, String)],
    offset: usize,
    limit: usize,
) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(base_params.len() + 2);
    for (k, v) in base_params {
        if k == "offset" || k == "limit" {
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    out.push((String::from("limit"), limit.to_string()));
    out.push((String::from("offset"), offset.to_string()));
    out
}

fn with_after_cursor(base_params: &[(String, String)], cursor: &str) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(base_params.len() + 1);
    for (k, v) in base_params {
        if k == "after" {
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    out.push((String::from("after"), cursor.to_owned()));
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::{json, Value};
    use spotty_cache::{AccountStore, CachedHttpEntry, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{CursorPage, OffsetPage, Pipeline};
    use crate::token::TokenManager;
    use crate::transport::{CachePolicy, RateLimitGate, SpotifyTransport};

    fn build_pipeline(mock_uri: &str, tmp: &TempDir) -> (Pipeline, Arc<DualCache>) {
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
            .cache_tokens(&client_id, "alice", "tok-pipe", 3600, None)
            .expect("seed token");

        let gate = Arc::new(RateLimitGate::new());
        let transport = Arc::new(
            SpotifyTransport::with_base_url(
                format!("{mock_uri}/v1"),
                tokens,
                Arc::clone(&cache),
                gate,
                client_id,
                "alice",
            )
            .expect("build transport"),
        );

        (Pipeline::new(transport), cache)
    }

    fn extract_u64_items(val: &Value) -> Vec<u64> {
        let Some(arr) = val["items"].as_array() else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            if let Some(n) = item.as_u64() {
                out.push(n);
            }
        }
        out
    }

    #[tokio::test]
    async fn fetch_offset_parallel_4_pages_175_items_and_propagates_skip_revalidate() {
        let server = MockServer::start().await;

        let page_ranges = [
            (0usize, 0u64..50, 10),
            (50, 50..100, 40),
            (100, 100..150, 5),
            (150, 150..175, 20),
        ];
        for (offset, range, delay_ms) in page_ranges {
            let items: Vec<u64> = range.collect();
            let next = if offset < 150 {
                Some(format!(
                    "{}/v1/me/tracks?limit=50&offset={}",
                    server.uri(),
                    offset + 50
                ))
            } else {
                None
            };
            Mock::given(method("GET"))
                .and(path("/v1/me/tracks"))
                .and(query_param("limit", "50"))
                .and(query_param("offset", offset.to_string()))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(delay_ms))
                        .insert_header("Cache-Control", "max-age=3600")
                        .insert_header("ETag", format!("\"etag-{offset}\""))
                        .set_body_json(json!({
                            "items": items,
                            "total": 175,
                            "next": next,
                        })),
                )
                .expect(1)
                .mount(&server)
                .await;
        }

        let tmp = TempDir::new().expect("tempdir");
        let (pipeline, cache) = build_pipeline(&server.uri(), &tmp);

        let items = pipeline
            .fetch_offset_parallel("me/tracks", &[], 50, 200, CachePolicy::UseCache, |val| {
                let items = extract_u64_items(val);
                let total = usize::try_from(val["total"].as_u64().unwrap_or(0)).unwrap_or(0);
                let next = val["next"].as_str().map(str::to_owned);
                Ok(OffsetPage::new(items, total, next))
            })
            .await
            .expect("4-page pagination should succeed");

        let expected: Vec<u64> = (0..175).collect();
        assert_eq!(items, expected);

        // Mark page 1..3 entries expired in cache while page 0 remains fresh;
        // SkipRevalidate propagation must serve them with zero extra HTTP hits.
        for offset in [50usize, 100, 150] {
            let rel = format!("me/tracks?limit=50&offset={offset}");
            let key = crate::transport::compute_http_cache_key(&rel, "alice");
            let existing = cache
                .http_entry(&key)
                .expect("read cached page")
                .expect("page must be cached");
            let expired = CachedHttpEntry::new(existing.body, existing.etag, None, 1);
            cache
                .put_http_entry(&key, &expired)
                .expect("write expired page");
        }

        let cached_items = pipeline
            .fetch_offset_parallel("me/tracks", &[], 50, 200, CachePolicy::UseCache, |val| {
                let items = extract_u64_items(val);
                let total = usize::try_from(val["total"].as_u64().unwrap_or(0)).unwrap_or(0);
                let next = val["next"].as_str().map(str::to_owned);
                Ok(OffsetPage::new(items, total, next))
            })
            .await
            .expect("cached page 0 must propagate SkipRevalidate to pages 1..3");
        assert_eq!(cached_items, expected);
    }

    #[tokio::test]
    async fn fetch_cursor_serial_follows_22_char_after_chain() {
        let server = MockServer::start().await;
        let c1 = "0123456789abcdefghijkl";
        let c2 = "ABCDEFGHIJKLMNOPQRSTUV";

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .and(query_param_is_missing("after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [1, 2],
                "next": format!("{}/v1/me/following?type=artist&after={c1}", server.uri())
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .and(query_param("after", c1))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [3, 4],
                "next": format!("{}/v1/me/following?type=artist&after={c2}", server.uri())
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/following"))
            .and(query_param("type", "artist"))
            .and(query_param("after", c2))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [5],
                "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (pipeline, _) = build_pipeline(&server.uri(), &tmp);
        let params = vec![(String::from("type"), String::from("artist"))];

        let items = pipeline
            .fetch_cursor_serial("me/following", &params, CachePolicy::BypassCache, |val| {
                let items = extract_u64_items(val);
                let next = val["next"].as_str().map(str::to_owned);
                Ok(CursorPage::new(items, next, None))
            })
            .await
            .expect("cursor pagination should succeed");

        assert_eq!(items, vec![1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn execute_indexed_chunks_reassembles_strictly_by_usize_index() {
        let server = MockServer::start().await;

        // Chunk 0 finishes slowest (60ms) while chunk 2 finishes immediately.
        let chunk_defs = [
            ("id0", 60u64, vec![10u64, 11]),
            ("id1", 25u64, vec![20u64, 21]),
            ("id2", 0u64, vec![30u64, 31]),
        ];
        for (ids, delay_ms, items) in chunk_defs {
            Mock::given(method("GET"))
                .and(path("/v1/tracks"))
                .and(query_param("ids", ids))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(delay_ms))
                        .set_body_json(json!({ "items": items })),
                )
                .expect(1)
                .mount(&server)
                .await;
        }

        let tmp = TempDir::new().expect("tempdir");
        let (pipeline, _) = build_pipeline(&server.uri(), &tmp);

        // Pass chunks in scrambled vector order to prove sorting by usize index.
        let chunks = vec![
            (2usize, vec![(String::from("ids"), String::from("id2"))]),
            (0usize, vec![(String::from("ids"), String::from("id0"))]),
            (1usize, vec![(String::from("ids"), String::from("id1"))]),
        ];

        let assembled = pipeline
            .execute_indexed_chunks("tracks", &chunks, CachePolicy::BypassCache, |val| {
                Ok(extract_u64_items(val))
            })
            .await
            .expect("indexed chunk execution should succeed");

        assert_eq!(assembled, vec![10, 11, 20, 21, 30, 31]);
    }

    #[tokio::test]
    async fn fetch_offset_parallel_continues_when_page0_filters_to_empty() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/me/tracks"))
            .and(query_param("limit", "2"))
            .and(query_param("offset", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [1, 2],
                "total": 4,
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me/tracks"))
            .and(query_param("limit", "2"))
            .and(query_param("offset", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [3, 4],
                "total": 4,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir");
        let (pipeline, _) = build_pipeline(&server.uri(), &tmp);

        let items = pipeline
            .fetch_offset_parallel("me/tracks", &[], 2, 10, CachePolicy::BypassCache, |val| {
                let raw = extract_u64_items(val);
                let total = usize::try_from(val["total"].as_u64().unwrap_or(0)).unwrap_or(0);
                // Filter out page 0 items completely; page 1 items must still be fetched.
                let filtered = raw.into_iter().filter(|&n| n > 2).collect();
                Ok(OffsetPage::new(filtered, total, None))
            })
            .await
            .expect("pagination should continue past empty filtered page 0");

        assert_eq!(items, vec![3, 4]);
    }

    #[tokio::test]
    async fn fetch_offset_parallel_respects_exact_max_iterations_including_page0() {
        let server = MockServer::start().await;

        for offset in [0usize, 2] {
            Mock::given(method("GET"))
                .and(path("/v1/me/tracks"))
                .and(query_param("limit", "2"))
                .and(query_param("offset", offset.to_string()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "items": [offset as u64, offset as u64 + 1],
                    "total": 10,
                })))
                .expect(1)
                .mount(&server)
                .await;
        }

        let tmp = TempDir::new().expect("tempdir");
        let (pipeline, _) = build_pipeline(&server.uri(), &tmp);
        let bounded = pipeline.with_max_iterations(2);

        let items = bounded
            .fetch_offset_parallel("me/tracks", &[], 2, 10, CachePolicy::BypassCache, |val| {
                let items = extract_u64_items(val);
                let total = usize::try_from(val["total"].as_u64().unwrap_or(0)).unwrap_or(0);
                Ok(OffsetPage::new(items, total, None))
            })
            .await
            .expect("max_iterations=2 should fetch page 0 and page 1 only");

        assert_eq!(items, vec![0, 1, 2, 3]);
    }
}
