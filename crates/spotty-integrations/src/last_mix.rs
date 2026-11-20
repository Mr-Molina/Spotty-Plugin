//! `LastMix` service integration (`LastMix.pm`).

use spotty_api::{SearchKind, SpotifyClient};
use spotty_cache::AccountStore;
use spotty_types::{ClientId, Result};

/// LMS protocol name exposed by `Plugins::Spotty::LastMix` (`LastMix.pm:61`).
pub const LASTMIX_PROTOCOL: &str = "spotify";

const LASTMIX_SEARCH_LIMIT: usize = 5;

/// Track candidate returned to `LastMix` (`LastMix.pm:46-50`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastMixCandidate {
    /// Track title (`$track->{name}`).
    pub title: String,
    /// Primary artist display name (`$track->{artists}->[0]->{name}`).
    pub artist: String,
    /// Canonical `spotify:track:<id>` URI (`$track->{uri}`).
    pub url: String,
}

impl LastMixCandidate {
    /// Creates a new [`LastMixCandidate`].
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        artist: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            artist: artist.into(),
            url: url.into(),
        }
    }
}

/// `LastMix` lookup service for matching tracks on Spotify.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default)]
pub struct LastMixService;

impl LastMixService {
    /// Returns `true` when `client_id` is present and Spotty has at least one
    /// valid account or player credential (`LastMix.pm:13-21`).
    #[must_use]
    pub fn is_enabled(accounts: &AccountStore, client_id: Option<&ClientId>) -> bool {
        let Some(cid) = client_id else {
            return false;
        };
        if cid.as_str().is_empty() {
            return false;
        }
        if accounts.has_player_credentials() {
            return true;
        }
        accounts
            .all_credentials()
            .is_ok_and(|creds| !creds.is_empty())
    }

    /// Searches Spotify for up to 5 track candidates matching `artist` and
    /// `title` (`LastMix.pm:30-58`).
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if the Spotify search request
    /// fails.
    pub async fn lookup_candidates(
        client: &SpotifyClient,
        artist: &str,
        title: &str,
    ) -> Result<Vec<LastMixCandidate>> {
        let artist_trimmed = artist.trim();
        let title_trimmed = title.trim();
        if artist_trimmed.is_empty() || title_trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let mut parts = Vec::with_capacity(2);
        if !artist_trimmed.is_empty() {
            let escaped_artist = artist_trimmed.replace('"', "\\\"");
            parts.push(format!("artist:\"{escaped_artist}\""));
        }
        if !title_trimmed.is_empty() {
            let escaped_title = title_trimmed.replace('"', "\\\"");
            parts.push(format!("track:\"{escaped_title}\""));
        }
        let query = parts.join(" ");
        let results = client
            .search(
                &query,
                SearchKind::Track,
                LASTMIX_SEARCH_LIMIT,
                LASTMIX_SEARCH_LIMIT,
            )
            .await?;

        let mut candidates = Vec::new();
        for track in results.tracks {
            if track.uri.is_empty() || track.name.is_empty() {
                continue;
            }
            let Some(first_artist) = track.artists.first() else {
                continue;
            };
            if first_artist.name.is_empty() {
                continue;
            }
            candidates.push(LastMixCandidate {
                title: track.name,
                artist: first_artist.name.clone(),
                url: track.uri,
            });
        }
        Ok(candidates)
    }

    /// Resolves the best `LastMix` candidate for `artist` and `title`.
    ///
    /// Returns `Ok(None)` cleanly on empty search results (fixing the
    /// double-callback bug in `LastMix.pm:33-35`).
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if the Spotify search request
    /// fails.
    pub async fn best_match(
        client: &SpotifyClient,
        artist: &str,
        title: &str,
    ) -> Result<Option<LastMixCandidate>> {
        let candidates = Self::lookup_candidates(client, artist, title).await?;
        if candidates.is_empty() {
            return Ok(None);
        }

        let artist_lower = artist.trim().to_lowercase();
        let title_lower = title.trim().to_lowercase();

        for candidate in &candidates {
            if candidate.artist.to_lowercase() == artist_lower
                && candidate.title.to_lowercase() == title_lower
            {
                return Ok(Some(candidate.clone()));
            }
        }

        if !artist_lower.is_empty() && !title_lower.is_empty() {
            for candidate in &candidates {
                if candidate.artist.to_lowercase().contains(&artist_lower)
                    && candidate.title.to_lowercase().contains(&title_lower)
                {
                    return Ok(Some(candidate.clone()));
                }
            }
        }

        Ok(candidates.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::json;
    use spotty_api::{
        Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
    };
    use spotty_cache::{AccountId, AccountStore, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn build_test_client(mock_uri: &str, tmp: &TempDir) -> SpotifyClient {
        let cache =
            Arc::new(DualCache::open(tmp.path().join("cache.redb")).expect("open DualCache"));
        let accounts = Arc::new(AccountStore::new(tmp.path()));
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build reqwest client");
        let tokens = Arc::new(TokenManager::with_http(
            format!("{mock_uri}/api/token"),
            http,
            Arc::clone(&cache),
            accounts,
            1_700_000_000,
        ));
        let client_id = IconCode::default();
        tokens
            .cache_tokens(&client_id, "alice", "tok-test", 3600, None)
            .expect("seed access token");

        let gate = Arc::new(RateLimitGate::new());
        let transport = Arc::new(
            SpotifyTransport::with_base_url(
                format!("{mock_uri}/v1"),
                tokens,
                Arc::clone(&cache),
                gate,
                client_id.clone(),
                "alice",
            )
            .expect("build SpotifyTransport"),
        );
        let pipeline = Pipeline::new(Arc::clone(&transport));

        SpotifyClient::new(
            transport,
            pipeline,
            cache,
            client_id,
            "alice",
            "US",
            RateLimitPolicy::FailFastServer,
        )
    }

    #[test]
    fn is_enabled_requires_client_id_and_credentials() {
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let accounts = AccountStore::new(tmp.path());
        let cid = ClientId::new("aa:bb:cc:dd:ee:ff");

        assert!(
            !LastMixService::is_enabled(&accounts, None),
            "missing client_id must disable LastMix"
        );
        assert!(
            !LastMixService::is_enabled(&accounts, Some(&cid)),
            "empty AccountStore must disable LastMix"
        );

        let account_id = AccountId::from_username("alice");
        let acct_dir = accounts.account_dir(&account_id);
        fs::create_dir_all(&acct_dir).expect("create account dir");
        fs::write(
            acct_dir.join("credentials.json"),
            r#"{"username":"alice","auth_type":1,"auth_data":"secret"}"#,
        )
        .expect("write credentials.json");

        let accounts = AccountStore::new(tmp.path());
        assert!(
            LastMixService::is_enabled(&accounts, Some(&cid)),
            "valid client_id and credentials must enable LastMix"
        );
        assert_eq!(LASTMIX_PROTOCOL, "spotify");
    }

    #[tokio::test]
    async fn best_match_returns_ok_none_cleanly_on_empty_search_results() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "track"))
            .and(query_param("q", "artist:\"Unknown\" track:\"Missing\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": { "total": 0, "next": null, "items": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let matched = LastMixService::best_match(&client, "Unknown", "Missing")
            .await
            .expect("empty search must return Ok(None) cleanly");
        assert!(
            matched.is_none(),
            "expected None on empty search results: {matched:?}"
        );
    }

    #[tokio::test]
    async fn best_match_returns_exact_candidate_when_present() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);

        Mock::given(method("GET"))
            .and(path("/v1/search"))
            .and(query_param("type", "track"))
            .and(query_param(
                "q",
                "artist:\"Daft Punk\" track:\"Veridis Quo\"",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tracks": {
                    "total": 2,
                    "next": null,
                    "items": [
                        {
                            "id": "t_remix",
                            "uri": "spotify:track:t_remix",
                            "name": "Veridis Quo Acoustic",
                            "is_playable": true,
                            "artists": [{
                                "id": "a1",
                                "uri": "spotify:artist:a1",
                                "name": "Daft Punk"
                            }],
                            "album": {
                                "id": "al1",
                                "uri": "spotify:album:al1",
                                "name": "Al",
                                "images": []
                            }
                        },
                        {
                            "id": "t_exact",
                            "uri": "spotify:track:t_exact",
                            "name": "Veridis Quo",
                            "is_playable": true,
                            "artists": [{
                                "id": "a1",
                                "uri": "spotify:artist:a1",
                                "name": "Daft Punk"
                            }],
                            "album": {
                                "id": "al2",
                                "uri": "spotify:album:al2",
                                "name": "Discovery",
                                "images": []
                            }
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let matched = LastMixService::best_match(&client, "Daft Punk", "Veridis Quo")
            .await
            .expect("best_match must succeed")
            .expect("expected matched candidate");
        assert_eq!(matched.title, "Veridis Quo");
        assert_eq!(matched.artist, "Daft Punk");
        assert_eq!(matched.url, "spotify:track:t_exact");
    }
}
