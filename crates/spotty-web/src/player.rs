//! Per-player Spotty settings controller (`Settings/Player.pm`).

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use spotty_api::{NormalizedPlaylist, SpotifyClient, SPOTIFY_LIMIT};
use spotty_audio::HelperCapabilities;
use spotty_opml::sort_home_items;
use spotty_types::{
    ExplicitFilterMode, PlayerPrefs, PodcastEpisodeOrder, ReplayGainPolicy, Result,
};

const HOME_ITEM_PREF_PREFIX: &str = "pref_homeItem_";

/// View model entry for a single Spotify Home menu section toggle.
#[non_exhaustive]
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct HomeMenuItemOption {
    /// Spotify playlist or Home section identifier.
    pub id: String,
    /// Display label (`"{name}"` or `"{name} - {description}"`).
    pub name: String,
    /// Whether this item is currently hidden for the player.
    pub disabled: bool,
}

impl HomeMenuItemOption {
    /// Creates a new [`HomeMenuItemOption`].
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, disabled: bool) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            disabled,
        }
    }
}

/// View model rendered by the per-player settings page (`player.html`).
#[non_exhaustive]
#[derive(Serialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct PlayerSettingsViewModel {
    /// Whether the Spotty helper supports Spotify autoplay continuation.
    #[serde(rename = "canAutoplay")]
    pub can_autoplay: bool,
    /// Sorted list of Home menu section toggles for this player.
    #[serde(rename = "homeItems")]
    pub home_items: Vec<HomeMenuItemOption>,
    /// Whether per-player `ReplayGain` volume normalization is enabled.
    pub replaygain: bool,
    /// Explicit content filter mode (`0` allow, `1` hide, `2` strict).
    #[serde(rename = "filterExplicitContent")]
    pub filter_explicit_content: u8,
    /// Whether podcast episodes are listed newest-first.
    #[serde(rename = "reversePodcastOrder")]
    pub reverse_podcast_order: bool,
}

/// Controller for per-player Spotty settings (`Settings/Player.pm`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerSettingsController {
    can_autoplay: bool,
    home_limit: usize,
}

impl PlayerSettingsController {
    /// Creates a [`PlayerSettingsController`] from helper capabilities.
    #[must_use]
    pub fn new(capabilities: &HelperCapabilities) -> Self {
        Self {
            can_autoplay: capabilities.autoplay,
            home_limit: SPOTIFY_LIMIT,
        }
    }

    /// Overrides the maximum number of Home items fetched per load.
    #[must_use]
    pub fn with_home_limit(mut self, home_limit: usize) -> Self {
        self.home_limit = home_limit;
        self
    }

    /// Returns whether autoplay capability is enabled for the settings view.
    #[must_use]
    pub fn can_autoplay(&self) -> bool {
        self.can_autoplay
    }

    /// Fetches Home playlists and builds a [`PlayerSettingsViewModel`].
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if [`SpotifyClient::home`] fails.
    pub async fn load_view(
        &self,
        client: &SpotifyClient,
        prefs: &PlayerPrefs,
    ) -> Result<PlayerSettingsViewModel> {
        let playlists = client.home(self.home_limit).await?;
        Ok(self.build_view_model(playlists, prefs))
    }

    /// Persists submitted `pref_homeItem_<id>` toggles and player preferences
    /// into `prefs`, returning the refreshed [`PlayerSettingsViewModel`].
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if [`SpotifyClient::home`] fails.
    pub async fn save_prefs(
        &self,
        client: &SpotifyClient,
        prefs: &mut PlayerPrefs,
        params: &HashMap<String, String>,
    ) -> Result<PlayerSettingsViewModel> {
        let playlists = client.home(self.home_limit).await?;
        apply_player_scalar_prefs(prefs, params);
        apply_home_item_toggles(&playlists, prefs, params);
        Ok(self.build_view_model(playlists, prefs))
    }

    /// Builds a [`PlayerSettingsViewModel`] from already-fetched Home items.
    #[must_use]
    pub fn build_view_model(
        &self,
        playlists: Vec<NormalizedPlaylist>,
        prefs: &PlayerPrefs,
    ) -> PlayerSettingsViewModel {
        let home_items = build_home_menu_items(playlists, &prefs.ignore_home_items);
        PlayerSettingsViewModel {
            can_autoplay: self.can_autoplay,
            home_items,
            replaygain: prefs.replaygain == ReplayGainPolicy::ClientEnabled,
            filter_explicit_content: explicit_filter_to_u8(prefs.explicit_filter),
            reverse_podcast_order: prefs.podcast_order == PodcastEpisodeOrder::ReverseNewestFirst,
        }
    }
}

impl Default for PlayerSettingsController {
    fn default() -> Self {
        Self::new(&HelperCapabilities::default())
    }
}

/// Filters, sorts, and formats Home playlists into [`HomeMenuItemOption`]s.
#[must_use]
pub fn build_home_menu_items(
    playlists: Vec<NormalizedPlaylist>,
    ignore_home_items: &HashSet<String>,
) -> Vec<HomeMenuItemOption> {
    let mut filtered = Vec::with_capacity(playlists.len());
    for item in playlists {
        if item.id.trim().is_empty() || item.name.trim().is_empty() {
            continue;
        }
        filtered.push(item);
    }

    sort_home_items(&mut filtered, |item| &item.name);

    let mut options = Vec::with_capacity(filtered.len());
    for item in filtered {
        let disabled = ignore_home_items.contains(&item.id);
        let display_name = if item.description.is_empty() {
            item.name
        } else {
            format!("{} - {}", item.name, item.description)
        };
        options.push(HomeMenuItemOption {
            id: item.id,
            name: display_name,
            disabled,
        });
    }
    options
}

/// Updates `prefs.ignore_home_items` from submitted `pref_homeItem_<id>` keys.
pub fn apply_home_item_toggles(
    playlists: &[NormalizedPlaylist],
    prefs: &mut PlayerPrefs,
    params: &HashMap<String, String>,
) {
    for item in playlists {
        if item.id.trim().is_empty() || item.name.trim().is_empty() {
            continue;
        }
        let param_key = format!("{HOME_ITEM_PREF_PREFIX}{}", item.id);
        let enabled = params
            .get(&param_key)
            .is_some_and(|val| is_truthy_param(val));
        if enabled {
            prefs.ignore_home_items.remove(&item.id);
        } else {
            prefs.ignore_home_items.insert(item.id.clone());
        }
    }
}

fn apply_player_scalar_prefs(prefs: &mut PlayerPrefs, params: &HashMap<String, String>) {
    if let Some(val) = lookup_pref_param(params, "replaygain") {
        prefs.replaygain = if is_truthy_param(val) {
            ReplayGainPolicy::ClientEnabled
        } else {
            ReplayGainPolicy::Disabled
        };
    }

    if let Some(val) = lookup_pref_param(params, "filterExplicitContent") {
        prefs.explicit_filter = match val.trim() {
            "1" => ExplicitFilterMode::HideExplicit,
            "2" => ExplicitFilterMode::StrictMaster,
            _ => ExplicitFilterMode::AllowAll,
        };
    }

    if let Some(val) = lookup_pref_param(params, "reversePodcastOrder") {
        prefs.podcast_order = if is_truthy_param(val) {
            PodcastEpisodeOrder::ReverseNewestFirst
        } else {
            PodcastEpisodeOrder::OldestFirst
        };
    }
}

fn lookup_pref_param<'a>(params: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    let prefixed = format!("pref_{name}");
    params
        .get(&prefixed)
        .or_else(|| params.get(name))
        .map(String::as_str)
}

fn is_truthy_param(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
}

fn explicit_filter_to_u8(mode: ExplicitFilterMode) -> u8 {
    match mode {
        ExplicitFilterMode::AllowAll => 0,
        ExplicitFilterMode::HideExplicit => 1,
        ExplicitFilterMode::StrictMaster => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::json;
    use spotty_api::{
        Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
        PERSONAL_MIX_CATEGORY,
    };
    use spotty_cache::{AccountStore, DualCache};
    use spotty_types::IconCode;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn build_test_spotify_client(mock_uri: &str, tmp: &TempDir) -> SpotifyClient {
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
            .expect("seed test token");

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

    #[tokio::test]
    async fn load_view_filters_sorts_and_formats_home_items() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let client = build_test_spotify_client(&server.uri(), &tmp);

        let home_path = format!("/v1/browse/categories/{PERSONAL_MIX_CATEGORY}/playlists");
        Mock::given(method("GET"))
            .and(path(home_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "playlists": {
                    "total": 5,
                    "next": null,
                    "items": [
                        {
                            "id": "misc1",
                            "uri": "spotify:playlist:misc1",
                            "name": "Chill Vibes",
                            "description": "",
                            "owner": {"id": "spotify"},
                            "images": []
                        },
                        {
                            "id": "",
                            "uri": "spotify:playlist:empty_id",
                            "name": "Should Be Filtered",
                            "owner": {"id": "spotify"},
                            "images": []
                        },
                        {
                            "id": "dw1",
                            "uri": "spotify:playlist:dw1",
                            "name": "Discover Weekly",
                            "description": "Updated Mondays",
                            "owner": {"id": "spotify"},
                            "images": []
                        },
                        {
                            "id": "empty_name",
                            "uri": "spotify:playlist:empty_name",
                            "name": "   ",
                            "owner": {"id": "spotify"},
                            "images": []
                        },
                        {
                            "id": "dm1",
                            "uri": "spotify:playlist:dm1",
                            "name": "Daily Mix 1",
                            "description": "Indie & Rock",
                            "owner": {"id": "spotify"},
                            "images": []
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut prefs = PlayerPrefs::default();
        prefs.replaygain = ReplayGainPolicy::ClientEnabled;
        prefs.explicit_filter = ExplicitFilterMode::HideExplicit;
        prefs.podcast_order = PodcastEpisodeOrder::ReverseNewestFirst;
        prefs.ignore_home_items.insert(String::from("misc1"));

        let controller = PlayerSettingsController::default();
        let view = controller
            .load_view(&client, &prefs)
            .await
            .expect("load_view must succeed");

        assert!(view.can_autoplay);
        assert!(view.replaygain);
        assert_eq!(view.filter_explicit_content, 1);
        assert!(view.reverse_podcast_order);
        assert_eq!(
            view.home_items,
            vec![
                HomeMenuItemOption::new("dm1", "Daily Mix 1 - Indie & Rock", false),
                HomeMenuItemOption::new("dw1", "Discover Weekly - Updated Mondays", false),
                HomeMenuItemOption::new("misc1", "Chill Vibes", true),
            ],
            "items must be filtered, priority-sorted, and formatted: {:?}",
            view.home_items
        );
    }

    #[tokio::test]
    async fn save_prefs_persists_ignore_home_items_toggles() {
        let server = MockServer::start().await;
        let tmp = TempDir::new().expect("tempdir allocation must succeed");
        let client = build_test_spotify_client(&server.uri(), &tmp);

        let home_path = format!("/v1/browse/categories/{PERSONAL_MIX_CATEGORY}/playlists");
        Mock::given(method("GET"))
            .and(path(home_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "playlists": {
                    "total": 2,
                    "next": null,
                    "items": [
                        {
                            "id": "dm1",
                            "uri": "spotify:playlist:dm1",
                            "name": "Daily Mix 1",
                            "owner": {"id": "spotify"},
                            "images": []
                        },
                        {
                            "id": "rr1",
                            "uri": "spotify:playlist:rr1",
                            "name": "Release Radar",
                            "owner": {"id": "spotify"},
                            "images": []
                        }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut prefs = PlayerPrefs::default();
        prefs.ignore_home_items.insert(String::from("dm1"));

        let mut params = HashMap::new();
        params.insert(String::from("pref_homeItem_dm1"), String::from("1"));
        params.insert(
            String::from("pref_filterExplicitContent"),
            String::from("2"),
        );
        params.insert(String::from("pref_replaygain"), String::from("1"));

        let controller = PlayerSettingsController::default();
        let view = controller
            .save_prefs(&client, &mut prefs, &params)
            .await
            .expect("save_prefs must succeed");

        assert!(
            !prefs.ignore_home_items.contains("dm1"),
            "checked pref_homeItem_dm1 must remove dm1 from ignore_home_items"
        );
        assert!(
            prefs.ignore_home_items.contains("rr1"),
            "unchecked pref_homeItem_rr1 must add rr1 to ignore_home_items"
        );
        assert_eq!(prefs.explicit_filter, ExplicitFilterMode::StrictMaster);
        assert_eq!(prefs.replaygain, ReplayGainPolicy::ClientEnabled);

        assert_eq!(
            view.home_items,
            vec![
                HomeMenuItemOption::new("dm1", "Daily Mix 1", false),
                HomeMenuItemOption::new("rr1", "Release Radar", true),
            ]
        );
        assert_eq!(view.filter_explicit_content, 2);
        assert!(view.replaygain);
    }
}
