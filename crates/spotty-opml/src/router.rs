//! 26-route asynchronous OPML feed router (`OPML.pm:handleFeed`).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use serde::Deserialize;
use serde::Serialize;
use spotty_api::AlbumHydration;
use spotty_api::ArtistsScope;
use spotty_api::FeaturedPlaylists;
use spotty_api::PlaylistTracksOptions;
use spotty_api::SearchKind;
use spotty_api::SpotifyClient;
use spotty_api::TrackCacheLookup;
use spotty_api::SPOTIFY_LIMIT;
use spotty_cache::MetadataWriteMode;
use spotty_playlist_folders::FolderEntry;
use spotty_types::AccountSwitcherVisibility;
use spotty_types::AlbumScope;
use spotty_types::GlobalPrefs;
use spotty_types::PlayerPrefs;
use spotty_types::Result;
use spotty_types::SortOrder;
use spotty_types::SpotifyEntityKind;
use spotty_types::SpotifyUri;
use spotty_types::SpottyError;

use crate::artist_view::fetch_artist_overview;
use crate::formatters::format_album_list;
use crate::formatters::format_artist_list;
use crate::formatters::format_episodes_list;
use crate::formatters::format_playlist_list;
use crate::formatters::format_podcast_list;
use crate::formatters::format_track_list;
use crate::formatters::OpmlIndexBucket;
use crate::formatters::OpmlItemKind;
use crate::formatters::OpmlNode;
use crate::formatters::TrackListOptions;
use crate::formatters::TrackNumbering;
use crate::formatters::DEFAULT_SORT_ARTICLES;
use crate::home::sort_home_items;
use crate::home::top_chart_uri;
use crate::home::IMG_ACCOUNT;
use crate::home::IMG_ALBUM;
use crate::home::IMG_ARTIST;
use crate::home::IMG_GENRES;
use crate::home::IMG_HOME;
use crate::home::IMG_INBOX;
use crate::home::IMG_PLAYLIST;
use crate::home::IMG_PODCAST;
use crate::home::IMG_SEARCH;
use crate::home::IMG_SONGS;
use crate::home::IMG_TOP_TRACKS;
use crate::home::IMG_TRANSFER;
use crate::home::IMG_WHATS_NEW;
use crate::menus::add_track_to_playlist_action;
use crate::menus::add_track_to_playlist_menu;
use crate::menus::search_category_nodes;
use crate::menus::track_or_artist_radio;
use crate::menus::transfer_playback_menu;
use crate::menus::PodcastCapability;
use crate::menus::RadioSeedKind;
use crate::search_lru::RecentSearchesStore;

const MAX_LIBRARY_ITEMS: usize = 500;
const HOME_HERO_PREFIX: &str = "home_heroes_";
const EMPTY_LIBRARY_PROMPT: &str =
    "Add albums to your music, or follow an artist or playlist to populate your music library.";

/// Controls whether custom-client advanced features (such as `MySongs`) are
/// exposed on the root menu (`OPML.pm:_enableAdvancedFeatures`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdvancedFeatures {
    /// Custom Spotify Client ID configured (`!hasDefaultIcon()`).
    Enabled,
    /// Default plugin icon ID; advanced features omitted.
    #[default]
    Disabled,
}

/// Target entity type for [`OpmlRoute::Search`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SearchTarget {
    /// Search albums.
    Album,
    /// Search artists.
    Artist,
    /// Search tracks.
    Track,
    /// Search playlists.
    Playlist,
    /// Search podcast shows.
    Show,
    /// Search podcast episodes.
    Episode,
    /// Look up a Spotify user profile (`/v1/users/{id}`).
    User,
}

/// Account metadata entry used for multi-account root menus and switching.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterAccount {
    /// Canonical account name / ID key.
    pub id: String,
    /// Human-readable display name shown in LMS menus.
    pub display_name: String,
}

impl RouterAccount {
    /// Creates a new [`RouterAccount`].
    #[must_use]
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
        }
    }
}

/// All 26 Spotty OPML browse/action routes plus `MaterialSkin` `HomeHero` tags
/// and the account-scoped `WithAccount` wrapper (`OPML.pm`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpmlRoute {
    /// Top-level Spotty menu (`handleFeed`).
    Root,
    /// Personalized Home mixes (`home`).
    Home,
    /// Catalog, URI, or user search (`search`).
    Search {
        /// Search query text or Spotify URI / URL.
        query: String,
        /// Optional entity filter (`None` runs global search + tracks).
        kind: Option<SearchTarget>,
        /// Whether this invocation came from recent search history.
        is_recent: bool,
    },
    /// Recent search history list (`recentSearches`).
    RecentSearches,
    /// New album releases (`whatsNew`).
    WhatsNew,
    /// Country Top Tracks chart (`topTracks`, fixed `format_track_list`).
    TopTracks,
    /// Browse categories (`categories`).
    Categories,
    /// Category playlists (`category`).
    Category {
        /// Spotify category ID.
        id: String,
    },
    /// User's saved tracks (`mySongs`).
    MySongs,
    /// User's saved albums (`myAlbums`).
    MyAlbums,
    /// User's followed/library artists (`myArtists`).
    MyArtists,
    /// User's playlists or another user's public playlists (`playlists`).
    MyPlaylists {
        /// Optional target Spotify user ID (`None` uses the active user).
        user_id: Option<String>,
    },
    /// User's saved podcast shows (`shows`).
    MyShows,
    /// Podcast show episode list and metadata (`show`).
    Show {
        /// Show ID or `spotify:show:<id>` URI.
        id_or_uri: String,
        /// Optional show title from parent listing.
        name: Option<String>,
        /// Optional show description from parent listing.
        description: Option<String>,
        /// Optional ISO language codes from parent listing.
        languages: Vec<String>,
    },
    /// Save podcast show(s) to the user's library (`addShowToLibrary`).
    AddShowToLibrary {
        /// Show IDs or URIs to save.
        ids: Vec<String>,
    },
    /// Album track listing and metadata (`album`).
    Album {
        /// Album URI or ID.
        uri_or_id: String,
    },
    /// Save album(s) to the user's library (`addAlbumToLibrary`).
    AddAlbumToLibrary {
        /// Album IDs or URIs to save.
        ids: Vec<String>,
    },
    /// Concurrent 3-way artist overview (`artist`).
    Artist {
        /// Artist URI or ID.
        uri_or_id: String,
        /// Optional override for album visibility scope.
        album_scope: Option<AlbumScope>,
    },
    /// Artists related to an artist (`relatedArtists`).
    RelatedArtists {
        /// Artist URI or ID.
        uri_or_id: String,
    },
    /// Follow an artist (`followArtist`).
    FollowArtist {
        /// Artist URI or ID.
        uri_or_id: String,
        /// Artist display name for confirmation message.
        name: Option<String>,
    },
    /// Playlist track listing (`playlist`).
    Playlist {
        /// Playlist URI or ID.
        uri_or_id: String,
    },
    /// Spotify Connect playback transfer menu (`transferPlaylist`).
    TransferPlaylist,
    /// Recommendations seeded by a track (`trackRadio`).
    TrackRadio {
        /// Track URI or ID.
        uri_or_id: String,
    },
    /// Recommendations seeded by an artist (`artistRadio`).
    ArtistRadio {
        /// Artist URI or ID.
        uri_or_id: String,
    },
    /// Add a track to a playlist menu or action (`addTrackToPlaylist`).
    AddTrackToPlaylist {
        /// Track URI to add.
        track_uri: String,
        /// Target playlist URI (`None` lists user-owned playlists).
        playlist_uri: Option<String>,
    },
    /// Account selector menu or switch action (`selectAccount`).
    SelectAccount {
        /// Target account ID (`None` lists switchable accounts).
        account_id: Option<String>,
    },
    /// `MaterialSkin` Home Hero shortcut tag (`home_heroes_*`).
    HomeHero(String),
    /// Account-scoped route wrapper (`_withAccount`).
    WithAccount {
        /// Account identifier to execute `route` under.
        account_name: String,
        /// Wrapped OPML route.
        route: Box<OpmlRoute>,
    },
}

impl OpmlRoute {
    /// Parses a `MaterialSkin` `menu` tag (`home_heroes_*`) into
    /// [`OpmlRoute::HomeHero`].
    #[must_use]
    pub fn from_menu_tag(menu: &str) -> Option<Self> {
        let trimmed = menu.trim();
        if trimmed.starts_with(HOME_HERO_PREFIX) {
            Some(Self::HomeHero(String::from(trimmed)))
        } else {
            None
        }
    }
}

/// Structured OPML feed response returned by [`OpmlRouter::handle_feed`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpmlFeedResponse {
    /// Optional feed title.
    pub name: Option<String>,
    /// Returned OPML items.
    pub items: Vec<OpmlNode>,
    /// Optional alphabetical jump-bar buckets.
    pub index_list: Vec<OpmlIndexBucket>,
    /// Whether `featured_playlists` indicated custom-client API limitations.
    pub custom_client_limitations: bool,
    /// Optional LMS window navigation hint (`"parent"`, `"grandparent"`, etc.).
    pub next_window: Option<String>,
}

/// Asynchronous 26-route OPML feed dispatcher (`OPML.pm:handleFeed`).
#[derive(Debug)]
pub struct OpmlRouter {
    client: SpotifyClient,
    global_prefs: GlobalPrefs,
    player_prefs: PlayerPrefs,
    podcasts: PodcastCapability,
    advanced_features: AdvancedFeatures,
    accounts: Vec<RouterAccount>,
    account_clients: HashMap<String, SpotifyClient>,
    active_account: Mutex<String>,
    recent_searches: Mutex<RecentSearchesStore>,
    folder_hierarchy: Option<HashMap<String, FolderEntry>>,
    custom_client_limitations: AtomicBool,
}

impl OpmlRouter {
    /// Creates an [`OpmlRouter`] bound to `client`, `global_prefs`, and
    /// `player_prefs`.
    #[must_use]
    pub fn new(
        client: SpotifyClient,
        global_prefs: GlobalPrefs,
        player_prefs: PlayerPrefs,
    ) -> Self {
        let active = player_prefs
            .account_id
            .clone()
            .unwrap_or_else(|| String::from(client.user_id()));
        Self {
            client,
            global_prefs,
            player_prefs,
            podcasts: PodcastCapability::Enabled,
            advanced_features: AdvancedFeatures::Disabled,
            accounts: Vec::new(),
            account_clients: HashMap::new(),
            active_account: Mutex::new(active),
            recent_searches: Mutex::new(RecentSearchesStore::new()),
            folder_hierarchy: None,
            custom_client_limitations: AtomicBool::new(false),
        }
    }

    /// Configures podcast menu capability.
    #[must_use]
    pub fn with_podcasts(mut self, podcasts: PodcastCapability) -> Self {
        self.podcasts = podcasts;
        self
    }

    /// Configures custom-client advanced features (`MySongs` visibility).
    #[must_use]
    pub fn with_advanced_features(mut self, advanced_features: AdvancedFeatures) -> Self {
        self.advanced_features = advanced_features;
        self
    }

    /// Configures available accounts for multi-account root menus and
    /// [`OpmlRoute::SelectAccount`].
    #[must_use]
    pub fn with_accounts(mut self, accounts: Vec<RouterAccount>) -> Self {
        self.accounts = accounts;
        self
    }

    /// Registers an account-specific [`SpotifyClient`] used by
    /// [`OpmlRoute::WithAccount`].
    #[must_use]
    pub fn with_account_client(
        mut self,
        account_name: impl Into<String>,
        client: SpotifyClient,
    ) -> Self {
        self.account_clients
            .insert(account_name.into().to_ascii_lowercase(), client);
        self
    }

    /// Seeds the recent-searches LRU store.
    #[must_use]
    pub fn with_recent_searches(mut self, store: RecentSearchesStore) -> Self {
        self.recent_searches = Mutex::new(store);
        self
    }

    /// Sets an optional desktop playlist folder hierarchy for `MyPlaylists`
    /// and `AddTrackToPlaylist`.
    #[must_use]
    pub fn with_folder_hierarchy(mut self, hierarchy: HashMap<String, FolderEntry>) -> Self {
        self.folder_hierarchy = Some(hierarchy);
        self
    }

    /// Returns whether the last featured-playlists probe detected custom-client
    /// API limitations.
    #[must_use]
    pub fn custom_client_limitations(&self) -> bool {
        self.custom_client_limitations.load(Ordering::Relaxed)
    }

    /// Runs `f` with a read-only view of the router's recent-searches store.
    ///
    /// # Panics
    ///
    /// Panics if the recent-searches mutex was poisoned by a prior panic.
    pub fn with_recent_searches_store<R>(&self, f: impl FnOnce(&RecentSearchesStore) -> R) -> R {
        let guard = self
            .recent_searches
            .lock()
            .expect("recent searches mutex is never poisoned");
        f(&guard)
    }

    /// Runs `f` with the currently active account identifier.
    ///
    /// # Panics
    ///
    /// Panics if the active-account mutex was poisoned by a prior panic.
    pub fn with_active_account<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        let guard = self
            .active_account
            .lock()
            .expect("active account mutex is never poisoned");
        f(&guard)
    }

    /// Dispatches `route` asynchronously and builds an [`OpmlFeedResponse`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] if the underlying Spotify Web API call or URI
    /// parsing fails, or if an unknown `HomeHero` tag is supplied.
    ///
    /// # Panics
    ///
    /// Panics if internal account or search-history mutexes were poisoned by a
    /// prior panic.
    pub async fn handle_feed(&self, route: OpmlRoute) -> Result<OpmlFeedResponse> {
        let (target_client, resolved_route) = match route {
            OpmlRoute::WithAccount {
                account_name,
                route: inner,
            } => {
                {
                    let mut guard = self
                        .active_account
                        .lock()
                        .expect("active account mutex is never poisoned");
                    *guard = account_name.clone();
                }
                let selected = self
                    .account_clients
                    .get(&account_name.to_ascii_lowercase())
                    .unwrap_or(&self.client);
                selected.me().await?;
                (selected, *inner)
            }
            other => (&self.client, other),
        };

        self.dispatch_route(target_client, resolved_route).await
    }

    async fn dispatch_route(
        &self,
        client: &SpotifyClient,
        route: OpmlRoute,
    ) -> Result<OpmlFeedResponse> {
        match route {
            OpmlRoute::HomeHero(tag) => self.handle_home_hero(client, &tag).await,
            OpmlRoute::Root => self.handle_root(client).await,
            OpmlRoute::Home => self.handle_home(client).await,
            OpmlRoute::Search {
                query,
                kind,
                is_recent,
            } => self.handle_search(client, &query, kind, is_recent).await,
            OpmlRoute::RecentSearches => Ok(self.handle_recent_searches()),
            OpmlRoute::WhatsNew => self.handle_whats_new(client).await,
            OpmlRoute::TopTracks => self.handle_top_tracks(client).await,
            OpmlRoute::Categories => self.handle_categories(client).await,
            OpmlRoute::Category { id } => self.handle_category(client, &id).await,
            OpmlRoute::MySongs => self.handle_my_songs(client).await,
            OpmlRoute::MyAlbums => self.handle_my_albums(client).await,
            OpmlRoute::MyArtists => self.handle_my_artists(client).await,
            OpmlRoute::MyPlaylists { user_id } => {
                self.handle_my_playlists(client, user_id.as_deref()).await
            }
            OpmlRoute::MyShows => self.handle_my_shows(client).await,
            OpmlRoute::Show {
                id_or_uri,
                name: _,
                description,
                languages,
            } => {
                self.handle_show(client, &id_or_uri, description.as_deref(), &languages)
                    .await
            }
            OpmlRoute::AddShowToLibrary { ids } => {
                let mut refs = Vec::with_capacity(ids.len());
                for id in &ids {
                    refs.push(id.as_str());
                }
                client.add_show_to_library(&refs).await?;
                Ok(OpmlFeedResponse {
                    items: vec![OpmlNode::new("Music has been added", OpmlItemKind::Text)],
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::Album { uri_or_id } => self.handle_album(client, &uri_or_id).await,
            OpmlRoute::AddAlbumToLibrary { ids } => {
                let mut refs = Vec::with_capacity(ids.len());
                for id in &ids {
                    refs.push(id.as_str());
                }
                client.add_album_to_library(&refs).await?;
                Ok(OpmlFeedResponse {
                    items: vec![OpmlNode::new("Music has been added", OpmlItemKind::Text)],
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::Artist {
                uri_or_id,
                album_scope,
            } => {
                let scope = album_scope.unwrap_or(self.global_prefs.album_scope);
                let overview = fetch_artist_overview(
                    client,
                    &uri_or_id,
                    scope,
                    self.player_prefs.explicit_filter,
                    SortOrder::NaturalOrder,
                )
                .await?;
                Ok(OpmlFeedResponse {
                    name: Some(overview.name),
                    items: overview.items,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::RelatedArtists { uri_or_id } => {
                let artists = client.related_artists(&uri_or_id).await?;
                let list = format_artist_list(
                    &artists,
                    SortOrder::NaturalOrder,
                    self.global_prefs.album_scope,
                    DEFAULT_SORT_ARTICLES,
                    "Followers:",
                );
                Ok(OpmlFeedResponse {
                    items: list.items,
                    index_list: list.index_list,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::FollowArtist { uri_or_id, name } => {
                client.follow_artist(&[uri_or_id.as_str()]).await?;
                let label = match name {
                    Some(artist_name) if !artist_name.is_empty() => {
                        format!("Following this artist: {artist_name}")
                    }
                    _ => String::from("Following this artist:"),
                };
                Ok(OpmlFeedResponse {
                    items: vec![OpmlNode::new(label, OpmlItemKind::Text)],
                    next_window: Some(String::from("parent")),
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::Playlist { uri_or_id } => self.handle_playlist(client, &uri_or_id).await,
            OpmlRoute::TransferPlaylist => {
                let items = transfer_playback_menu(client).await?;
                Ok(OpmlFeedResponse {
                    items,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::TrackRadio { uri_or_id } => {
                let items = track_or_artist_radio(
                    client,
                    RadioSeedKind::Track,
                    &uri_or_id,
                    self.player_prefs.explicit_filter,
                )
                .await?;
                Ok(OpmlFeedResponse {
                    items,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::ArtistRadio { uri_or_id } => {
                let items = track_or_artist_radio(
                    client,
                    RadioSeedKind::Artist,
                    &uri_or_id,
                    self.player_prefs.explicit_filter,
                )
                .await?;
                Ok(OpmlFeedResponse {
                    items,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::AddTrackToPlaylist {
                track_uri,
                playlist_uri,
            } => {
                let items = match playlist_uri {
                    Some(pl) => add_track_to_playlist_action(client, &pl, &track_uri).await?,
                    None => {
                        add_track_to_playlist_menu(
                            client,
                            &track_uri,
                            self.folder_hierarchy.as_ref(),
                        )
                        .await?
                    }
                };
                Ok(OpmlFeedResponse {
                    items,
                    ..OpmlFeedResponse::default()
                })
            }
            OpmlRoute::SelectAccount { account_id } => {
                self.handle_select_account(client, account_id.as_deref())
                    .await
            }
            OpmlRoute::WithAccount {
                account_name,
                route: inner,
            } => {
                let selected = self
                    .account_clients
                    .get(&account_name.to_ascii_lowercase())
                    .unwrap_or(client);
                Box::pin(self.dispatch_route(selected, *inner)).await
            }
        }
    }

    async fn handle_home_hero(
        &self,
        client: &SpotifyClient,
        tag: &str,
    ) -> Result<OpmlFeedResponse> {
        let suffix = tag.strip_prefix(HOME_HERO_PREFIX).unwrap_or(tag);
        match suffix {
            "spotty" => self.handle_root(client).await,
            "home" => self.handle_home(client).await,
            "whatsnew" => self.handle_whats_new(client).await,
            "toptracks" => self.handle_top_tracks(client).await,
            "popularplaylists" => {
                let featured = client.featured_playlists(None, SPOTIFY_LIMIT).await?;
                let limited = is_limited(&featured);
                self.custom_client_limitations
                    .store(limited, Ordering::Relaxed);
                let items = if limited {
                    Vec::new()
                } else {
                    format_playlist_list(&featured.playlists, client.user_id(), None, "by")
                };
                Ok(OpmlFeedResponse {
                    items,
                    custom_client_limitations: limited,
                    ..OpmlFeedResponse::default()
                })
            }
            _ => Err(SpottyError::InvalidUri(format!(
                "unknown home_heroes tag: {tag}"
            ))),
        }
    }

    async fn handle_root(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let featured = client.featured_playlists(None, SPOTIFY_LIMIT).await?;
        let limited = is_limited(&featured);
        self.custom_client_limitations
            .store(limited, Ordering::Relaxed);

        let mut items = Vec::new();
        let has_recent = self.with_recent_searches_store(|s| !s.is_empty());
        if has_recent {
            items.push(route_node(
                "Search",
                OpmlItemKind::Link,
                IMG_SEARCH,
                "recent_searches",
            ));
        } else {
            items.push(route_node(
                "Search",
                OpmlItemKind::Search,
                IMG_SEARCH,
                "search",
            ));
        }

        items.push(route_node(
            "What's New",
            OpmlItemKind::Link,
            IMG_WHATS_NEW,
            "whats_new",
        ));

        let mut top_tracks_node = route_node(
            "Top Tracks",
            OpmlItemKind::Playlist,
            IMG_TOP_TRACKS,
            "top_tracks",
        );
        top_tracks_node.passthrough.insert(
            String::from("uri"),
            String::from(top_chart_uri(client.country())),
        );
        items.push(top_tracks_node);

        if !limited {
            items.push(route_node(
                "Genres and Moods",
                OpmlItemKind::Link,
                IMG_GENRES,
                "categories",
            ));

            if let Some(headline) = featured.message.as_deref() {
                let mut inbox = OpmlNode::new(headline, OpmlItemKind::Outline);
                inbox.image = Some(String::from(IMG_INBOX));
                inbox.items =
                    format_playlist_list(&featured.playlists, client.user_id(), None, "by");
                items.push(inbox);
            }
        }

        let personal = self.build_personal_items();
        let home_node =
            (!limited).then(|| route_node("Home", OpmlItemKind::Link, IMG_HOME, "home"));

        let multi_account = self.accounts.len() > 1;
        if self.global_prefs.account_switcher == AccountSwitcherVisibility::Hidden && multi_account
        {
            let mut sorted_accounts = self.accounts.clone();
            sorted_accounts.sort_by_key(|a| a.id.to_ascii_lowercase());
            for account in sorted_accounts {
                let mut account_items = Vec::with_capacity(personal.len() + 1);
                if let Some(home) = &home_node {
                    account_items.push(wrap_with_account(home, &account.id));
                }
                for item in &personal {
                    account_items.push(wrap_with_account(item, &account.id));
                }
                let mut account_node = OpmlNode::new(account.display_name, OpmlItemKind::Outline);
                account_node.image = Some(String::from(IMG_ACCOUNT));
                account_node.items = account_items;
                items.push(account_node);
            }
        } else {
            if let Some(home) = home_node {
                items.insert(0, home);
            }
            items.extend(personal);
        }

        items.push(route_node(
            "Transfer Playback",
            OpmlItemKind::Link,
            IMG_TRANSFER,
            "transfer_playlist",
        ));

        if self.global_prefs.account_switcher == AccountSwitcherVisibility::VisibleInMenu
            && multi_account
        {
            let current_display = self.with_active_account(|active| {
                for acc in &self.accounts {
                    if acc.id == active {
                        return acc.display_name.clone();
                    }
                }
                String::from(active)
            });
            let mut account_menu = OpmlNode::new("Account", OpmlItemKind::Outline);
            account_menu.image = Some(String::from(IMG_ACCOUNT));
            account_menu.items = vec![
                OpmlNode::new(current_display, OpmlItemKind::Text),
                route_node(
                    "Select Account",
                    OpmlItemKind::Link,
                    IMG_ACCOUNT,
                    "select_account",
                ),
            ];
            items.push(account_menu);
        }

        Ok(OpmlFeedResponse {
            items,
            custom_client_limitations: limited,
            ..OpmlFeedResponse::default()
        })
    }

    fn build_personal_items(&self) -> Vec<OpmlNode> {
        let mut personal = Vec::new();
        if self.advanced_features == AdvancedFeatures::Enabled {
            personal.push(route_node(
                "Songs",
                OpmlItemKind::Playlist,
                IMG_SONGS,
                "my_songs",
            ));
        }
        personal.push(route_node(
            "Albums",
            OpmlItemKind::Link,
            IMG_ALBUM,
            "my_albums",
        ));
        personal.push(route_node(
            "Artists",
            OpmlItemKind::Link,
            IMG_ARTIST,
            "my_artists",
        ));
        personal.push(route_node(
            "Playlists",
            OpmlItemKind::Link,
            IMG_PLAYLIST,
            "my_playlists",
        ));
        if self.podcasts == PodcastCapability::Enabled {
            personal.push(route_node(
                "Podcasts",
                OpmlItemKind::Link,
                IMG_PODCAST,
                "my_shows",
            ));
        }
        personal
    }

    async fn handle_home(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let raw_items = client.home(SPOTIFY_LIMIT).await?;
        let mut filtered = Vec::with_capacity(raw_items.len());
        for item in raw_items {
            if item.name.is_empty() || item.id.is_empty() {
                continue;
            }
            if self.global_prefs.ignore_home_items.contains(&item.id)
                || self.player_prefs.ignore_home_items.contains(&item.id)
            {
                continue;
            }
            filtered.push(item);
        }

        sort_home_items(&mut filtered, |pl| pl.name.as_str());

        let mut desc_by_uri: HashMap<&str, &str> = HashMap::with_capacity(filtered.len());
        for item in &filtered {
            if !item.description.is_empty() {
                desc_by_uri.insert(item.uri.as_str(), item.description.as_str());
            }
        }

        let mut nodes = format_playlist_list(&filtered, client.user_id(), None, "by");
        for node in &mut nodes {
            if let Some(uri) = node.url.as_deref() {
                if let Some(desc) = desc_by_uri.get(uri) {
                    node.line2 = Some(String::from(*desc));
                }
            }
        }

        Ok(OpmlFeedResponse {
            items: nodes,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_search(
        &self,
        client: &SpotifyClient,
        query: &str,
        kind: Option<SearchTarget>,
        is_recent: bool,
    ) -> Result<OpmlFeedResponse> {
        use std::str::FromStr as _;

        if let Ok(parsed) = SpotifyUri::from_str(query) {
            let colon_uri = parsed.to_colon_uri();
            match parsed.kind() {
                SpotifyEntityKind::Playlist => {
                    return self.handle_playlist(client, &colon_uri).await
                }
                SpotifyEntityKind::Album => return self.handle_album(client, &colon_uri).await,
                SpotifyEntityKind::Artist => {
                    let overview = fetch_artist_overview(
                        client,
                        &colon_uri,
                        self.global_prefs.album_scope,
                        self.player_prefs.explicit_filter,
                        SortOrder::NaturalOrder,
                    )
                    .await?;
                    return Ok(OpmlFeedResponse {
                        name: Some(overview.name),
                        items: overview.items,
                        ..OpmlFeedResponse::default()
                    });
                }
                SpotifyEntityKind::Show => {
                    return self.handle_show(client, &colon_uri, None, &[]).await;
                }
                SpotifyEntityKind::Track | SpotifyEntityKind::Episode => {
                    let maybe = client
                        .track_cached(&colon_uri, TrackCacheLookup::FetchIfMissing)
                        .await?;
                    let tracks: Vec<_> = maybe.into_iter().collect();
                    let items = format_track_list(
                        &tracks,
                        self.player_prefs.explicit_filter,
                        &TrackListOptions::default(),
                    );
                    return Ok(OpmlFeedResponse {
                        items,
                        ..OpmlFeedResponse::default()
                    });
                }
                SpotifyEntityKind::User | _ => {}
            }
        }

        if kind == Some(SearchTarget::User) {
            let profile = client.user(query).await?;
            let title = match &profile.display_name {
                Some(dn) if !dn.is_empty() => format!("{dn} ({})", profile.id),
                _ => profile.id.clone(),
            };
            let mut profile_node = OpmlNode::new(title, OpmlItemKind::Text);
            profile_node.image = if profile.image.is_empty() {
                Some(String::from(IMG_ACCOUNT))
            } else {
                Some(profile.image)
            };

            let mut pl_node = route_node(
                "Playlists",
                OpmlItemKind::Link,
                IMG_PLAYLIST,
                "my_playlists",
            );
            pl_node.passthrough.insert(String::from("user"), profile.id);

            return Ok(OpmlFeedResponse {
                items: vec![profile_node, pl_node],
                ..OpmlFeedResponse::default()
            });
        }

        let search_kind = match kind {
            Some(SearchTarget::Album) => SearchKind::Album,
            Some(SearchTarget::Artist) => SearchKind::Artist,
            Some(SearchTarget::Playlist) => SearchKind::Playlist,
            Some(SearchTarget::Show) => SearchKind::Show,
            Some(SearchTarget::Episode) => SearchKind::Episode,
            Some(SearchTarget::Track) | None => SearchKind::Track,
            Some(SearchTarget::User) => SearchKind::Track,
        };

        let results = client
            .search(query, search_kind, SPOTIFY_LIMIT, SPOTIFY_LIMIT)
            .await?;

        match kind {
            None => {
                let mut items = Vec::new();
                for cat in search_category_nodes(query, self.podcasts) {
                    if cat.passthrough.get("type").map(String::as_str) != Some("track") {
                        items.push(cat);
                    }
                }
                items.extend(format_track_list(
                    &results.tracks,
                    self.player_prefs.explicit_filter,
                    &TrackListOptions::default(),
                ));
                if !is_recent {
                    let mut guard = self
                        .recent_searches
                        .lock()
                        .expect("recent searches mutex is never poisoned");
                    guard.add(query);
                }
                Ok(OpmlFeedResponse {
                    items,
                    ..OpmlFeedResponse::default()
                })
            }
            Some(SearchTarget::Album) => {
                let out = format_album_list(
                    &results.albums,
                    SortOrder::NaturalOrder,
                    DEFAULT_SORT_ARTICLES,
                    "by",
                );
                Ok(OpmlFeedResponse {
                    items: out.items,
                    index_list: out.index_list,
                    ..OpmlFeedResponse::default()
                })
            }
            Some(SearchTarget::Artist) => {
                let out = format_artist_list(
                    &results.artists,
                    SortOrder::NaturalOrder,
                    self.global_prefs.album_scope,
                    DEFAULT_SORT_ARTICLES,
                    "Followers:",
                );
                Ok(OpmlFeedResponse {
                    items: out.items,
                    index_list: out.index_list,
                    ..OpmlFeedResponse::default()
                })
            }
            Some(SearchTarget::Track) => Ok(OpmlFeedResponse {
                items: format_track_list(
                    &results.tracks,
                    self.player_prefs.explicit_filter,
                    &TrackListOptions::default(),
                ),
                ..OpmlFeedResponse::default()
            }),
            Some(SearchTarget::Playlist) => Ok(OpmlFeedResponse {
                items: format_playlist_list(&results.playlists, client.user_id(), None, "by"),
                ..OpmlFeedResponse::default()
            }),
            Some(SearchTarget::Show) => {
                let out = format_podcast_list(&results.shows);
                Ok(OpmlFeedResponse {
                    items: out.items,
                    index_list: out.index_list,
                    ..OpmlFeedResponse::default()
                })
            }
            Some(SearchTarget::Episode) => Ok(OpmlFeedResponse {
                items: format_episodes_list(
                    &results.episodes,
                    self.player_prefs.explicit_filter,
                    self.player_prefs.podcast_order,
                    None,
                ),
                ..OpmlFeedResponse::default()
            }),
            Some(SearchTarget::User) => Ok(OpmlFeedResponse::default()),
        }
    }

    fn handle_recent_searches(&self) -> OpmlFeedResponse {
        let mut items = Vec::new();
        items.push(route_node(
            "New Search",
            OpmlItemKind::Search,
            IMG_SEARCH,
            "search",
        ));

        self.with_recent_searches_store(|store| {
            let total = store.len();
            for (rev_idx, term) in store.mru_items().into_iter().enumerate() {
                let oldest_idx = total - 1 - rev_idx;
                let mut passthrough = BTreeMap::new();
                passthrough.insert(String::from("route"), String::from("search"));
                passthrough.insert(String::from("query"), String::from(term));
                passthrough.insert(String::from("recent"), String::from("1"));
                passthrough.insert(String::from("deleteMenu"), oldest_idx.to_string());

                let mut node = OpmlNode::new(term, OpmlItemKind::Link);
                node.passthrough = passthrough;
                items.push(node);
            }
        });

        OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        }
    }

    async fn handle_whats_new(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let albums = client.new_releases(SPOTIFY_LIMIT).await?;
        let formatted = format_album_list(
            &albums,
            SortOrder::NaturalOrder,
            DEFAULT_SORT_ARTICLES,
            "by",
        );
        Ok(OpmlFeedResponse {
            items: formatted.items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_top_tracks(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        // Review Focus #4: TopTracks formats tracks via format_track_list,
        // fixing the OPML.pm:604 `tracksList` typo.
        let chart_uri = top_chart_uri(client.country());
        let tracks = client
            .playlist_tracks(chart_uri, &PlaylistTracksOptions::default())
            .await?;
        let items = format_track_list(
            &tracks,
            self.player_prefs.explicit_filter,
            &TrackListOptions::default(),
        );
        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_categories(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let categories = client.categories(None, SPOTIFY_LIMIT).await?;
        let mut items = Vec::with_capacity(categories.len());
        for category in categories {
            let mut passthrough = BTreeMap::new();
            passthrough.insert(String::from("route"), String::from("category"));
            passthrough.insert(String::from("id"), category.id);
            passthrough.insert(String::from("title"), category.name.clone());

            let mut node = OpmlNode::new(category.name, OpmlItemKind::Link);
            node.image = (!category.image.is_empty()).then_some(category.image);
            node.passthrough = passthrough;
            items.push(node);
        }
        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_category(&self, client: &SpotifyClient, id: &str) -> Result<OpmlFeedResponse> {
        let playlists = client.category_playlists(id, SPOTIFY_LIMIT).await?;
        Ok(OpmlFeedResponse {
            items: format_playlist_list(&playlists, client.user_id(), None, "by"),
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_my_songs(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let tracks = client
            .my_songs(
                MetadataWriteMode::MergeExisting,
                self.global_prefs.sort_songs,
                MAX_LIBRARY_ITEMS,
            )
            .await?;
        let mut items = format_track_list(
            &tracks,
            self.player_prefs.explicit_filter,
            &TrackListOptions::default(),
        );
        if items.is_empty() {
            items.push(OpmlNode::new(
                "Please add songs to your music library to populate your this section.",
                OpmlItemKind::Text,
            ));
        }
        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_my_albums(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let albums = client
            .my_albums(
                MetadataWriteMode::MergeExisting,
                AlbumHydration::SummaryOnly,
                self.global_prefs.sort_albums,
                MAX_LIBRARY_ITEMS,
            )
            .await?;
        let mut out = format_album_list(
            &albums,
            self.global_prefs.sort_albums,
            DEFAULT_SORT_ARTICLES,
            "by",
        );
        if out.items.is_empty() {
            out.items
                .push(OpmlNode::new(EMPTY_LIBRARY_PROMPT, OpmlItemKind::Text));
        }
        Ok(OpmlFeedResponse {
            items: out.items,
            index_list: out.index_list,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_my_artists(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let lib = client
            .my_artists(
                ArtistsScope::IncludeAlbumArtists,
                self.global_prefs.sort_artists,
            )
            .await?;
        let mut out = format_artist_list(
            &lib.artists,
            self.global_prefs.sort_artists,
            self.global_prefs.album_scope,
            DEFAULT_SORT_ARTICLES,
            "Followers:",
        );
        if out.items.is_empty() {
            out.items
                .push(OpmlNode::new(EMPTY_LIBRARY_PROMPT, OpmlItemKind::Text));
        }
        Ok(OpmlFeedResponse {
            items: out.items,
            index_list: out.index_list,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_my_playlists(
        &self,
        client: &SpotifyClient,
        user_id: Option<&str>,
    ) -> Result<OpmlFeedResponse> {
        let (playlists, hierarchy) = match user_id {
            Some(uid) if !uid.trim().is_empty() => {
                (client.user_playlists(uid, MAX_LIBRARY_ITEMS).await?, None)
            }
            _ => (
                client.my_playlists(MAX_LIBRARY_ITEMS).await?,
                self.folder_hierarchy.as_ref(),
            ),
        };

        let mut items = format_playlist_list(&playlists, client.user_id(), hierarchy, "by");
        if items.is_empty() {
            items.push(OpmlNode::new(EMPTY_LIBRARY_PROMPT, OpmlItemKind::Text));
        }
        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_my_shows(&self, client: &SpotifyClient) -> Result<OpmlFeedResponse> {
        let shows = client.my_shows(MAX_LIBRARY_ITEMS).await?;
        let out = format_podcast_list(&shows);
        Ok(OpmlFeedResponse {
            items: out.items,
            index_list: out.index_list,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_show(
        &self,
        client: &SpotifyClient,
        id_or_uri: &str,
        description: Option<&str>,
        languages: &[String],
    ) -> Result<OpmlFeedResponse> {
        let episodes = client
            .episodes(id_or_uri, SPOTIFY_LIMIT, MAX_LIBRARY_ITEMS)
            .await?;
        let mut items = format_episodes_list(
            &episodes,
            self.player_prefs.explicit_filter,
            self.player_prefs.podcast_order,
            None,
        );

        let mut add_pass = BTreeMap::new();
        add_pass.insert(String::from("route"), String::from("add_show_to_library"));
        add_pass.insert(String::from("id"), String::from(id_or_uri));
        let mut add_node = OpmlNode::new("Add podcast to library", OpmlItemKind::Link);
        add_node.passthrough = add_pass;
        items.push(add_node);

        if let Some(desc) = description.filter(|d| !d.is_empty()) {
            let mut desc_node = OpmlNode::new("Description", OpmlItemKind::Outline);
            desc_node.items = vec![OpmlNode::new(desc, OpmlItemKind::Textarea)];
            items.push(desc_node);
        }

        if !languages.is_empty() {
            let mut upper = Vec::with_capacity(languages.len());
            for lang in languages {
                upper.push(lang.to_ascii_uppercase());
            }
            let lang_text = upper.join(", ");
            items.push(OpmlNode::new(
                format!("Language: {lang_text}"),
                OpmlItemKind::Text,
            ));
        }

        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_album(
        &self,
        client: &SpotifyClient,
        uri_or_id: &str,
    ) -> Result<OpmlFeedResponse> {
        let album = client.album(uri_or_id).await?;
        let opts = TrackListOptions::default()
            .with_numbering(TrackNumbering::TrackNumber)
            .with_fallback_image(album.image.clone());
        let mut items = format_track_list(&album.tracks, self.player_prefs.explicit_filter, &opts);

        let mut add_pass = BTreeMap::new();
        add_pass.insert(String::from("route"), String::from("add_album_to_library"));
        add_pass.insert(String::from("id"), album.id.clone());
        add_pass.insert(String::from("name"), album.name.clone());
        let mut add_node = OpmlNode::new("Add album to library", OpmlItemKind::Link);
        add_node.passthrough = add_pass;
        items.push(add_node);

        let mut artists_by_name: BTreeMap<String, String> = BTreeMap::new();
        for track in &album.tracks {
            for artist in &track.artists {
                if artist.uri.is_empty() {
                    continue;
                }
                artists_by_name.insert(artist.name.clone(), artist.uri.clone());
            }
        }
        for (artist_name, artist_uri) in artists_by_name {
            let mut passthrough = BTreeMap::new();
            passthrough.insert(String::from("route"), String::from("artist"));
            passthrough.insert(String::from("uri"), artist_uri.clone());
            let mut node = OpmlNode::new(format!("Artist: {artist_name}"), OpmlItemKind::Link);
            node.url = Some(artist_uri);
            node.image = Some(String::from(IMG_ARTIST));
            node.passthrough = passthrough;
            items.push(node);
        }

        if let Some(year) = extract_year(&album.release_date) {
            items.push(OpmlNode::new(format!("Year: {year}"), OpmlItemKind::Text));
        }

        Ok(OpmlFeedResponse {
            name: Some(album.name),
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_playlist(
        &self,
        client: &SpotifyClient,
        uri_or_id: &str,
    ) -> Result<OpmlFeedResponse> {
        let tracks = client
            .playlist_tracks(uri_or_id, &PlaylistTracksOptions::default())
            .await?;
        let items = format_track_list(
            &tracks,
            self.player_prefs.explicit_filter,
            &TrackListOptions::default(),
        );
        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }

    async fn handle_select_account(
        &self,
        client: &SpotifyClient,
        account_id: Option<&str>,
    ) -> Result<OpmlFeedResponse> {
        if let Some(target_id) = account_id {
            {
                let mut guard = self
                    .active_account
                    .lock()
                    .expect("active account mutex is never poisoned");
                *guard = String::from(target_id);
            }
            let target_client = self
                .account_clients
                .get(&target_id.to_ascii_lowercase())
                .unwrap_or(client);
            target_client.me().await?;
            return Ok(OpmlFeedResponse {
                next_window: Some(String::from("grandparent")),
                ..OpmlFeedResponse::default()
            });
        }

        let current = self.with_active_account(str::to_owned);
        let mut items = Vec::new();
        for account in &self.accounts {
            if account.id == current {
                continue;
            }
            let mut passthrough = BTreeMap::new();
            passthrough.insert(String::from("route"), String::from("select_account"));
            passthrough.insert(String::from("id"), account.id.clone());
            let mut node = OpmlNode::new(account.display_name.clone(), OpmlItemKind::Link);
            node.passthrough = passthrough;
            items.push(node);
        }

        Ok(OpmlFeedResponse {
            items,
            ..OpmlFeedResponse::default()
        })
    }
}

fn is_limited(featured: &FeaturedPlaylists) -> bool {
    featured.playlists.is_empty() || featured.message.as_deref().is_none_or(str::is_empty)
}

fn route_node(label: &str, kind: OpmlItemKind, icon: &str, route: &str) -> OpmlNode {
    let mut passthrough = BTreeMap::new();
    passthrough.insert(String::from("route"), String::from(route));
    let mut node = OpmlNode::new(label, kind);
    node.image = Some(String::from(icon));
    node.passthrough = passthrough;
    node
}

fn wrap_with_account(node: &OpmlNode, account_id: &str) -> OpmlNode {
    let mut wrapped = node.clone();
    if let Some(inner_route) = wrapped.passthrough.get("route").cloned() {
        wrapped
            .passthrough
            .insert(String::from("wrapped_route"), inner_route);
    }
    wrapped
        .passthrough
        .insert(String::from("route"), String::from("with_account"));
    wrapped
        .passthrough
        .insert(String::from("account"), String::from(account_id));
    wrapped
}

fn extract_year(release_date: &str) -> Option<&str> {
    let trimmed = release_date.trim();
    if trimmed.len() >= 4 && trimmed.as_bytes()[..4].iter().all(u8::is_ascii_digit) {
        Some(&trimmed[..4])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use super::*;
    use crate::artist_view::test_support::build_test_client;
    use crate::artist_view::test_support::build_test_client_for_user;

    #[tokio::test]
    async fn root_hides_home_and_genres_when_custom_client_limitations_detected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/browse/featured-playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "playlists": {"total": 0, "next": null, "items": []}
            })))
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);
        let router = OpmlRouter::new(client, GlobalPrefs::default(), PlayerPrefs::default());

        let resp = router
            .handle_feed(OpmlRoute::Root)
            .await
            .expect("Root feed should succeed under custom client limitations");

        assert!(
            resp.custom_client_limitations,
            "expected custom_client_limitations=true"
        );
        assert!(
            router.custom_client_limitations(),
            "router should persist custom_client_limitations flag"
        );

        let names: Vec<&str> = resp.items.iter().map(|n| n.name.as_str()).collect();
        assert!(
            !names.contains(&"Home"),
            "Home must be hidden when limited: {names:?}"
        );
        assert!(
            !names.contains(&"Genres and Moods"),
            "Genres and Moods must be hidden when limited: {names:?}"
        );
        assert!(names.contains(&"Search"));
        assert!(names.contains(&"Top Tracks"));
        assert!(names.contains(&"Transfer Playback"));
    }

    #[tokio::test]
    async fn root_shows_home_genres_and_featured_when_featured_playlists_present() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/browse/featured-playlists"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": "Thursday Spotlight",
                "playlists": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "fp11111111111111111111",
                        "uri": "spotify:playlist:fp11111111111111111111",
                        "name": "Jazz Vibes",
                        "owner": {"id": "spotify"},
                        "collaborative": false,
                        "tracks": {"total": 50}
                    }]
                }
            })))
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);
        let router = OpmlRouter::new(client, GlobalPrefs::default(), PlayerPrefs::default())
            .with_advanced_features(AdvancedFeatures::Enabled);

        let resp = router
            .handle_feed(OpmlRoute::Root)
            .await
            .expect("Root feed should succeed");
        assert!(!resp.custom_client_limitations);

        let names: Vec<&str> = resp.items.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"Home"), "Home expected: {names:?}");
        assert!(
            names.contains(&"Genres and Moods"),
            "Genres expected: {names:?}"
        );
        assert!(
            names.contains(&"Thursday Spotlight"),
            "Featured inbox expected: {names:?}"
        );
        assert!(
            names.contains(&"Songs"),
            "AdvancedFeatures::Enabled should show Songs: {names:?}"
        );
    }

    #[tokio::test]
    async fn top_tracks_and_home_hero_toptracks_format_via_format_track_list() {
        let server = MockServer::start().await;
        let us_chart_id = "37i9dQZEVXbLRQDuF5jeBp";

        Mock::given(method("GET"))
            .and(path(format!("/v1/playlists/{us_chart_id}/tracks")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 1,
                "next": null,
                "items": [{
                    "added_at": "2025-01-01T00:00:00Z",
                    "track": {
                        "id": "tt11111111111111111111",
                        "uri": "spotify:track:tt11111111111111111111",
                        "name": "Chart Hit",
                        "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "Top Artist"}],
                        "album": {
                            "id": "al1",
                            "uri": "spotify:album:al1",
                            "name": "Top Album",
                            "album_type": "album",
                            "images": []
                        },
                        "duration_ms": 195000,
                        "track_number": 1,
                        "disc_number": 1,
                        "explicit": false,
                        "is_playable": true
                    }
                }]
            })))
            .mount(&server)
            .await;

        let tmp = TempDir::new().expect("tempdir creation must succeed");
        let client = build_test_client(&server.uri(), &tmp);
        let router = OpmlRouter::new(client, GlobalPrefs::default(), PlayerPrefs::default());

        let top_resp = router
            .handle_feed(OpmlRoute::TopTracks)
            .await
            .expect("TopTracks route should succeed");
        assert_eq!(top_resp.items.len(), 1);
        assert_eq!(
            top_resp.items[0].url.as_deref(),
            Some("spotify://track:tt11111111111111111111")
        );
        assert_eq!(
            top_resp.items[0].name,
            "Chart Hit by Top Artist from Top Album"
        );

        let hero_route = OpmlRoute::from_menu_tag("home_heroes_toptracks").expect("tag must parse");
        let hero_resp = router
            .handle_feed(hero_route)
            .await
            .expect("HomeHero toptracks should succeed");
        assert_eq!(hero_resp.items, top_resp.items);
    }

    #[tokio::test]
    async fn home_hero_tags_and_with_account_wrapper_dispatch_correctly() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path(
                "/v1/browse/categories/0JQ5DAt0tbjZptfcdMSKl3/playlists",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "playlists": {
                    "total": 2,
                    "next": null,
                    "items": [
                        {
                            "id": "h111111111111111111111",
                            "uri": "spotify:playlist:h111111111111111111111",
                            "name": "Release Radar",
                            "description": "New releases for you",
                            "owner": {"id": "spotify"},
                            "collaborative": false,
                            "tracks": {"total": 30}
                        },
                        {
                            "id": "h222222222222222222222",
                            "uri": "spotify:playlist:h222222222222222222222",
                            "name": "Daily Mix 1",
                            "description": "Your daily mix",
                            "owner": {"id": "spotify"},
                            "collaborative": false,
                            "tracks": {"total": 50}
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "bob",
                "uri": "spotify:user:bob",
                "display_name": "Bob"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/browse/new-releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "albums": {
                    "total": 1,
                    "next": null,
                    "items": [{
                        "id": "nr11111111111111111111",
                        "uri": "spotify:album:nr11111111111111111111",
                        "name": "Fresh Drop",
                        "album_type": "album",
                        "artists": [{"id": "a1", "uri": "spotify:artist:a1", "name": "New Artist"}],
                        "images": []
                    }]
                }
            })))
            .mount(&server)
            .await;

        let tmp_alice = TempDir::new().expect("tempdir creation must succeed");
        let tmp_bob = TempDir::new().expect("tempdir creation must succeed");
        let alice_client = build_test_client_for_user(&server.uri(), &tmp_alice, "alice");
        let bob_client = build_test_client_for_user(&server.uri(), &tmp_bob, "bob");

        let router = OpmlRouter::new(alice_client, GlobalPrefs::default(), PlayerPrefs::default())
            .with_accounts(vec![
                RouterAccount::new("alice", "Alice"),
                RouterAccount::new("bob", "Bob"),
            ])
            .with_account_client("bob", bob_client);

        let hero_home = router
            .handle_feed(OpmlRoute::HomeHero(String::from("home_heroes_home")))
            .await
            .expect("home_heroes_home should succeed");
        assert_eq!(hero_home.items.len(), 2);
        // sort_home_items places Daily Mix before Release Radar.
        assert_eq!(hero_home.items[0].name, "Daily Mix 1");
        assert_eq!(hero_home.items[1].name, "Release Radar");

        let wrapped = router
            .handle_feed(OpmlRoute::WithAccount {
                account_name: String::from("bob"),
                route: Box::new(OpmlRoute::HomeHero(String::from("home_heroes_whatsnew"))),
            })
            .await
            .expect("WithAccount wrapped route should succeed");
        assert_eq!(wrapped.items.len(), 1);
        assert_eq!(wrapped.items[0].line1.as_deref(), Some("Fresh Drop"));
        router.with_active_account(|active| {
            assert_eq!(active, "bob", "WithAccount must update active account");
        });
    }
}
