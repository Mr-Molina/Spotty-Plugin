//! Concurrent newline-delimited JSON-RPC 2.0 daemon and method dispatcher.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};
use spotty_api::{
    Pipeline, PlaylistTrackSort, PlaylistTracksOptions, RateLimitGate, RateLimitPolicy,
    SpotifyClient, SpotifyTransport, TokenManager, TrackCacheLookup,
};
use spotty_audio::{
    resolve_lms_track_gain, HelperCapabilities, ServerReplayGainMode, TranscodeTableBuilder,
};
use spotty_cache::{AccountStore, DualCache};
use spotty_importer::{online_library_needs_update, PlaylistSyncPolicy};
use spotty_integrations::{
    resolve_home_extra_menu, DstmRecommender, LastMixService, SeedTrackInput, HOME_EXTRAS,
};
use spotty_opml::{
    OpmlRoute, OpmlRouter, RecentSearchCliQuery, RecentSearchesStore, RouterAccount,
};
use spotty_types::{
    ApFallbackPolicy, ClientId, GlobalPrefs, IconCode, PlayerPrefs, ReplayGainPolicy, Result,
    SpotifyEntityKind, SpotifyUri, SpottyError,
};
use spotty_web::{
    check_player_credentials, AuthLandingController, BasicSettingsController, BasicSettingsForm,
    HelperDiagnostics, PkceCallbackController, PlayerAuthSupervisor, PlayerSettingsController,
    PlaylistFoldersSettingsController, SettingsPostOutcome,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::lifecycle::{LifecycleConfig, LifecycleSupervisor, DEFAULT_SWEEP_INTERVAL};
use crate::rpc_protocol::{
    default_endpoint_path, generate_bearer_token, remove_endpoint_file, write_endpoint_file,
    DaemonEndpointInfo, JsonRpcRequest, JsonRpcResponse, ERR_INTERNAL, ERR_INVALID_PARAMS,
    ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE, ERR_UNAUTHORIZED, SPOTTY_VERSION,
};

/// Default loopback TCP bind address using an OS-assigned ephemeral port.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:0";

/// Configuration for [`SpottyDaemon`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    /// Base cache directory containing `spotty/` state and `cache.redb`.
    pub cache_dir: PathBuf,
    /// Path where [`DaemonEndpointInfo`] is written atomically.
    pub endpoint_file: PathBuf,
    /// Socket bind address (`"127.0.0.1:0"` by default).
    pub bind_addr: String,
    /// Periodic maintenance interval for [`LifecycleSupervisor`].
    pub sweep_interval: Duration,
}

impl DaemonConfig {
    /// Creates a [`DaemonConfig`] using `<cache_dir>/spotty/daemon.endpoint`
    /// and `127.0.0.1:0`.
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        let cache_dir = cache_dir.into();
        let endpoint_file = default_endpoint_path(&cache_dir);
        Self {
            cache_dir,
            endpoint_file,
            bind_addr: String::from(DEFAULT_BIND_ADDR),
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
        }
    }

    /// Overrides the endpoint discovery file path.
    #[must_use]
    pub fn with_endpoint_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.endpoint_file = path.into();
        self
    }

    /// Overrides the TCP bind address.
    #[must_use]
    pub fn with_bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    /// Overrides the background maintenance sweep interval.
    #[must_use]
    pub fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }
}

/// Shared daemon state accessed by concurrent JSON-RPC connections.
#[derive(Debug)]
pub struct DaemonState {
    cache_dir: PathBuf,
    accounts: Arc<AccountStore>,
    cache: Arc<DualCache>,
    tokens: Arc<TokenManager>,
    rate_limit_gate: Arc<RateLimitGate>,
    global_prefs: Mutex<GlobalPrefs>,
    player_prefs: Mutex<HashMap<String, PlayerPrefs>>,
    recent_searches: Mutex<RecentSearchesStore>,
    disabled_formats: Mutex<Vec<String>>,
    player_auth_supervisor: PlayerAuthSupervisor,
    shutdown_tx: watch::Sender<bool>,
    token: String,
}

impl DaemonState {
    /// Initializes daemon state backed by `cache_dir` and opens `cache.redb`.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] if creating directories, opening `DualCache`, or
    /// constructing the HTTP `TokenManager` fails.
    pub fn initialize(
        cache_dir: &Path,
        token: String,
        shutdown_tx: watch::Sender<bool>,
    ) -> Result<Self> {
        let spotty_dir = cache_dir.join("spotty");
        std::fs::create_dir_all(&spotty_dir)?;

        let accounts = Arc::new(AccountStore::new(cache_dir));
        let cache = Arc::new(DualCache::open(spotty_dir.join("cache.redb"))?);
        let tokens = Arc::new(TokenManager::new(
            Arc::clone(&cache),
            Arc::clone(&accounts),
        )?);
        let rate_limit_gate = Arc::new(RateLimitGate::new());

        Ok(Self {
            cache_dir: cache_dir.to_path_buf(),
            accounts,
            cache,
            tokens,
            rate_limit_gate,
            global_prefs: Mutex::new(GlobalPrefs::default()),
            player_prefs: Mutex::new(HashMap::new()),
            recent_searches: Mutex::new(RecentSearchesStore::new()),
            disabled_formats: Mutex::new(Vec::new()),
            player_auth_supervisor: PlayerAuthSupervisor::with_librespot(),
            shutdown_tx,
            token,
        })
    }

    /// Runs a read-only closure against [`GlobalPrefs`].
    ///
    /// # Panics
    ///
    /// Panics if the `global_prefs` mutex was poisoned by a prior panic.
    pub fn with_global_prefs<R>(&self, f: impl FnOnce(&GlobalPrefs) -> R) -> R {
        let guard = self
            .global_prefs
            .lock()
            .expect("global_prefs mutex is never poisoned");
        f(&guard)
    }

    /// Runs a mutable closure against [`GlobalPrefs`].
    ///
    /// # Panics
    ///
    /// Panics if the `global_prefs` mutex was poisoned by a prior panic.
    pub fn with_mut_global_prefs<R>(&self, f: impl FnOnce(&mut GlobalPrefs) -> R) -> R {
        let mut guard = self
            .global_prefs
            .lock()
            .expect("global_prefs mutex is never poisoned");
        f(&mut guard)
    }

    /// Returns a reference to the [`DualCache`].
    #[must_use]
    pub fn cache(&self) -> &Arc<DualCache> {
        &self.cache
    }

    /// Returns a reference to the [`AccountStore`].
    #[must_use]
    pub fn accounts(&self) -> &Arc<AccountStore> {
        &self.accounts
    }

    /// Signals graceful daemon shutdown.
    pub fn trigger_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    fn build_client(&self, account_override: Option<&str>) -> Result<SpotifyClient> {
        let (icon_code, country) =
            self.with_global_prefs(|prefs| (prefs.icon_code.clone(), prefs.country.clone()));
        let user_id = self.resolve_account_user_id(account_override)?;
        let transport = Arc::new(SpotifyTransport::new(
            Arc::clone(&self.tokens),
            Arc::clone(&self.cache),
            Arc::clone(&self.rate_limit_gate),
            icon_code.clone(),
            user_id.as_str(),
        )?);
        let pipeline = Pipeline::new(Arc::clone(&transport));
        Ok(SpotifyClient::new(
            transport,
            pipeline,
            Arc::clone(&self.cache),
            icon_code,
            user_id,
            country,
            RateLimitPolicy::FailFastServer,
        ))
    }

    fn resolve_account_user_id(&self, account_override: Option<&str>) -> Result<String> {
        if let Some(explicit) = account_override
            .map(str::trim)
            .filter(|val| !val.is_empty())
        {
            return Ok(explicit.to_owned());
        }
        let creds = self.accounts.all_credentials()?;
        if let Some((username, _)) = creds.into_iter().next() {
            return Ok(username);
        }
        Ok(String::from("default"))
    }
}

/// Concurrent loopback JSON-RPC 2.0 server with atomic endpoint registration.
#[derive(Debug)]
pub struct SpottyDaemon {
    listener: TcpListener,
    local_addr: SocketAddr,
    endpoint_file: PathBuf,
    endpoint_info: DaemonEndpointInfo,
    state: Arc<DaemonState>,
    shutdown_rx: watch::Receiver<bool>,
    lifecycle: LifecycleSupervisor,
}

impl SpottyDaemon {
    /// Binds a TCP listener, initializes daemon state, writes the atomic
    /// endpoint file, and starts background maintenance.
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] if binding the socket, opening caches, or writing
    /// the endpoint file fails.
    pub async fn bind(config: DaemonConfig) -> Result<Self> {
        let listener = TcpListener::bind(&config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        let token = generate_bearer_token();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let state = Arc::new(DaemonState::initialize(
            &config.cache_dir,
            token.clone(),
            shutdown_tx,
        )?);

        let endpoint_info =
            DaemonEndpointInfo::new(local_addr.to_string(), token, process::id(), SPOTTY_VERSION);
        write_endpoint_file(&config.endpoint_file, &endpoint_info)?;

        let lifecycle_cfg =
            LifecycleConfig::new(&config.cache_dir).with_sweep_interval(config.sweep_interval);
        let lifecycle = LifecycleSupervisor::spawn(lifecycle_cfg, Some(Arc::clone(state.cache())));

        info!(
            addr = %local_addr,
            endpoint = %config.endpoint_file.display(),
            "SpottyDaemon bound and ready"
        );

        Ok(Self {
            listener,
            local_addr,
            endpoint_file: config.endpoint_file,
            endpoint_info,
            state,
            shutdown_rx,
            lifecycle,
        })
    }

    /// Returns the bound local socket address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Returns the endpoint discovery metadata written to disk.
    #[must_use]
    pub fn endpoint_info(&self) -> &DaemonEndpointInfo {
        &self.endpoint_info
    }

    /// Returns a shared handle to [`DaemonState`].
    #[must_use]
    pub fn state(&self) -> &Arc<DaemonState> {
        &self.state
    }

    /// Triggers graceful server shutdown.
    pub fn shutdown(&self) {
        self.state.trigger_shutdown();
    }

    /// Runs the accept loop until shutdown is requested via `system.shutdown`,
    /// Ctrl-C, or [`SpottyDaemon::shutdown`].
    ///
    /// # Errors
    ///
    /// Returns [`SpottyError`] if accepting sockets fails or endpoint cleanup
    /// fails.
    pub async fn serve(mut self) -> Result<()> {
        let mut active_tasks = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                biased;
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || *self.shutdown_rx.borrow() {
                        break;
                    }
                }
                accept_res = self.listener.accept() => {
                    match accept_res {
                        Ok((stream, peer)) => {
                            debug!(peer = %peer, "Accepted JSON-RPC client connection");
                            let conn_state = Arc::clone(&self.state);
                            active_tasks.spawn(async move {
                                if let Err(err) = handle_connection(stream, conn_state).await {
                                    warn!(peer = %peer, error = %err, "Client connection error");
                                }
                            });
                        }
                        Err(err) => {
                            warn!(error = %err, "TCP accept error");
                        }
                    }
                }
                Some(_) = active_tasks.join_next(), if !active_tasks.is_empty() => {}
            }
        }

        self.state.player_auth_supervisor.shutdown_and_wait().await;
        self.lifecycle.shutdown_and_wait().await;
        while active_tasks.join_next().await.is_some() {}
        remove_endpoint_file(&self.endpoint_file)?;
        info!("SpottyDaemon shut down cleanly");
        Ok(())
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<DaemonState>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(req) => process_request(req, &state).await,
            Err(err) => {
                JsonRpcResponse::error(Value::Null, ERR_PARSE, format!("Parse error: {err}"), None)
            }
        };

        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write_half.write_all(&encoded).await?;
        write_half.flush().await?;
    }

    Ok(())
}

async fn process_request(req: JsonRpcRequest, state: &DaemonState) -> JsonRpcResponse {
    if !req.is_valid_version() {
        return JsonRpcResponse::error(
            req.id,
            ERR_INVALID_REQUEST,
            "Invalid JSON-RPC request",
            None,
        );
    }
    if !req.is_authorized(&state.token) {
        return JsonRpcResponse::error(req.id, ERR_UNAUTHORIZED, "Unauthorized bearer token", None);
    }

    let id = req.id.clone();
    match dispatch_method(&req.method, req.params, state).await {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err(RpcDispatchError::InvalidParams(msg)) => {
            JsonRpcResponse::error(id, ERR_INVALID_PARAMS, msg, None)
        }
        Err(RpcDispatchError::MethodNotFound(method)) => JsonRpcResponse::error(
            id,
            ERR_METHOD_NOT_FOUND,
            format!("Method not found: {method}"),
            None,
        ),
        Err(RpcDispatchError::Internal(err)) => {
            JsonRpcResponse::error(id, ERR_INTERNAL, err.to_string(), None)
        }
    }
}

enum RpcDispatchError {
    InvalidParams(String),
    MethodNotFound(String),
    Internal(SpottyError),
}

impl From<SpottyError> for RpcDispatchError {
    fn from(err: SpottyError) -> Self {
        Self::Internal(err)
    }
}

impl From<serde_json::Error> for RpcDispatchError {
    fn from(err: serde_json::Error) -> Self {
        Self::InvalidParams(format!("Invalid parameters: {err}"))
    }
}

async fn dispatch_method(
    method: &str,
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    match method {
        "system.ping" => Ok(json!({
            "status": "ok",
            "version": SPOTTY_VERSION,
        })),
        "system.shutdown" => {
            state.trigger_shutdown();
            Ok(json!({ "status": "shutting_down" }))
        }
        "plugin.init" => handle_plugin_init(params, state),
        "opml.feed" => handle_opml_feed(params, state).await,
        "opml.recent_searches" => handle_recent_searches(params, state),
        "protocol.metadata" => handle_protocol_metadata(params, state).await,
        "protocol.explode_playlist" => handle_explode_playlist(params, state).await,
        "protocol.track_gain" => handle_track_gain(params, state),
        "importer.needs_update" => handle_importer_needs_update(params, state).await,
        "integrations.dstm" => handle_integrations_dstm(params, state).await,
        "integrations.home_extras" => handle_integrations_home_extras(params),
        "integrations.last_mix" => handle_integrations_last_mix(params, state).await,
        "settings.basic" => handle_settings_basic(params, state),
        "settings.auth_check" => handle_settings_auth_check(params, state),
        "settings.oauth_prepare" => handle_settings_oauth_prepare(params, state).await,
        "settings.oauth_callback" => handle_settings_oauth_callback(params, state).await,
        "settings.player" => handle_settings_player(params, state).await,
        "settings.player_auth_check" => handle_settings_player_auth_check(params, state),
        "settings.playlist_folders" => handle_settings_playlist_folders(params, state),
        _ => Err(RpcDispatchError::MethodNotFound(method.to_owned())),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PluginInitParams {
    prefs: Option<GlobalPrefs>,
    disabled_formats: Vec<String>,
    arch: String,
    os: String,
    helper_bin: Option<String>,
}

fn handle_plugin_init(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: PluginInitParams = parse_optional_params(params)?;

    let updated_prefs = state.with_mut_global_prefs(|prefs| {
        if let Some(incoming) = parsed.prefs {
            *prefs = incoming;
        }
        prefs.migrate_v3_ap_fallback();
        prefs.clone()
    });

    let updated_disabled = {
        let mut guard = state
            .disabled_formats
            .lock()
            .expect("disabled_formats mutex is never poisoned");
        if !parsed.disabled_formats.is_empty() {
            *guard = parsed.disabled_formats;
        }
        let arch = if parsed.arch.is_empty() {
            std::env::consts::ARCH
        } else {
            parsed.arch.as_str()
        };
        let os = if parsed.os.is_empty() {
            std::env::consts::OS
        } else {
            parsed.os.as_str()
        };
        GlobalPrefs::migrate_v1_disabled_formats(arch, os, &mut guard);
        guard.clone()
    };

    let helper_token = parsed.helper_bin.as_deref().unwrap_or("lms-spotty");
    let custom_convert_conf = TranscodeTableBuilder::build_custom_convert_conf(helper_token);
    let credentials = state.accounts.all_credentials()?;
    let accounts: Vec<Value> = credentials
        .into_iter()
        .map(|(username, id)| json!({ "username": username, "id": id.as_str() }))
        .collect();
    let has_player_credentials = state.accounts.has_player_credentials();

    Ok(json!({
        "version": SPOTTY_VERSION,
        "prefs": updated_prefs,
        "disabled_formats": updated_disabled,
        "custom_convert_conf": custom_convert_conf,
        "accounts": accounts,
        "has_player_credentials": has_player_credentials,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OpmlFeedParams {
    menu: Option<String>,
    query: Option<String>,
    account: Option<String>,
}

async fn handle_opml_feed(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: OpmlFeedParams = parse_optional_params(params)?;
    let client = state.build_client(parsed.account.as_deref())?;
    let global_prefs = state.with_global_prefs(Clone::clone);
    let player_prefs = PlayerPrefs::default();

    let mut accounts = Vec::new();
    for (username, id) in state.accounts.all_credentials()? {
        accounts.push(RouterAccount::new(id.as_str(), username));
    }
    let searches = state
        .recent_searches
        .lock()
        .expect("recent_searches mutex is never poisoned")
        .clone();

    let router = OpmlRouter::new(client, global_prefs, player_prefs)
        .with_accounts(accounts)
        .with_recent_searches(searches);

    let route = if let Some(query) = parsed.query {
        OpmlRoute::Search {
            query,
            kind: None,
            is_recent: false,
        }
    } else if let Some(menu) = parsed.menu.as_deref() {
        OpmlRoute::from_menu_tag(menu).ok_or_else(|| {
            RpcDispatchError::InvalidParams(format!("Unknown OPML menu tag: {menu}"))
        })?
    } else {
        OpmlRoute::Root
    };

    let feed = router.handle_feed(route).await?;
    Ok(serde_json::to_value(feed)?)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RecentSearchesParams {
    action: Option<String>,
    query: Option<String>,
    delete_index: Option<usize>,
    delete_label: Option<String>,
    clear_label: Option<String>,
}

fn handle_recent_searches(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: RecentSearchesParams = parse_optional_params(params)?;
    let mut store = state
        .recent_searches
        .lock()
        .expect("recent_searches mutex is never poisoned");

    let delete_label = parsed.delete_label.as_deref().unwrap_or("Delete");
    let clear_label = parsed.clear_label.as_deref().unwrap_or("Clear all");

    match parsed.action.as_deref().unwrap_or("list") {
        "add" => {
            let query = parsed.query.as_deref().ok_or_else(|| {
                RpcDispatchError::InvalidParams(String::from("Missing query for add"))
            })?;
            store.add(query);
            let items: Vec<String> = store.mru_items().into_iter().map(str::to_owned).collect();
            Ok(json!({ "items": items, "count": store.len() }))
        }
        "delete_index" => {
            let idx = parsed.delete_index.ok_or_else(|| {
                RpcDispatchError::InvalidParams(String::from("Missing delete_index"))
            })?;
            let resp = store.handle_cli_query(
                RecentSearchCliQuery::DeleteIndex(idx),
                delete_label,
                clear_label,
            )?;
            Ok(serde_json::to_value(resp)?)
        }
        "clear" => {
            let resp = store.handle_cli_query(
                RecentSearchCliQuery::DeleteAll,
                delete_label,
                clear_label,
            )?;
            Ok(serde_json::to_value(resp)?)
        }
        _ => {
            let resp = store.handle_cli_query(
                RecentSearchCliQuery::ListMenu {
                    delete_menu_idx: parsed.delete_index,
                },
                delete_label,
                clear_label,
            )?;
            Ok(serde_json::to_value(resp)?)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProtocolMetadataParams {
    uri: Option<String>,
    uris: Vec<String>,
    cache_only: bool,
    account: Option<String>,
}

async fn handle_protocol_metadata(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    use std::str::FromStr as _;

    let parsed: ProtocolMetadataParams = parse_optional_params(params)?;
    let client = state.build_client(parsed.account.as_deref())?;

    if !parsed.uris.is_empty() {
        let refs: Vec<&str> = parsed.uris.iter().map(String::as_str).collect();
        let tracks = client.tracks_and_episodes(&refs).await?;
        return Ok(serde_json::to_value(tracks)?);
    }

    let raw_uri = parsed
        .uri
        .as_deref()
        .map(str::trim)
        .filter(|val| !val.is_empty())
        .ok_or_else(|| {
            RpcDispatchError::InvalidParams(String::from("Missing uri or uris parameter"))
        })?;
    let spotify_uri = SpotifyUri::from_str(raw_uri)?;

    if parsed.cache_only {
        if matches!(
            spotify_uri.kind(),
            SpotifyEntityKind::Track | SpotifyEntityKind::Episode
        ) {
            let lookup = client
                .track_cached(raw_uri, TrackCacheLookup::CacheOnly)
                .await?;
            return Ok(serde_json::to_value(lookup)?);
        }
        let cached = state.cache.metadata_value(&spotify_uri.to_colon_uri())?;
        return Ok(json!({ "cached": cached }));
    }

    match spotify_uri.kind() {
        SpotifyEntityKind::Track | SpotifyEntityKind::Episode => {
            let track = client
                .track_cached(raw_uri, TrackCacheLookup::FetchIfMissing)
                .await?;
            Ok(serde_json::to_value(track)?)
        }
        SpotifyEntityKind::Album => Ok(serde_json::to_value(client.album(raw_uri).await?)?),
        SpotifyEntityKind::Artist => Ok(serde_json::to_value(client.artist(raw_uri).await?)?),
        _ => {
            let cached = state.cache.metadata_value(&spotify_uri.to_colon_uri())?;
            Ok(json!({ "cached": cached }))
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExplodePlaylistParams {
    uri: String,
    max_items: Option<usize>,
    added_at_desc: bool,
    account: Option<String>,
}

async fn handle_explode_playlist(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: ExplodePlaylistParams = serde_json::from_value(params)?;
    if parsed.uri.trim().is_empty() {
        return Err(RpcDispatchError::InvalidParams(String::from(
            "Missing playlist uri",
        )));
    }
    let client = state.build_client(parsed.account.as_deref())?;
    let mut options = PlaylistTracksOptions::new();
    if let Some(max) = parsed.max_items {
        options = options.with_max_items(max);
    }
    if parsed.added_at_desc {
        options = options.with_sort(PlaylistTrackSort::AddedAtDescending);
    }
    let tracks = client.playlist_tracks(&parsed.uri, &options).await?;
    Ok(serde_json::to_value(tracks)?)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TrackGainParams {
    player_id: Option<String>,
    server_mode: Option<String>,
    remote_replay_gain: Option<f32>,
}

fn handle_track_gain(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: TrackGainParams = parse_optional_params(params)?;
    let policy = parsed
        .player_id
        .as_deref()
        .and_then(|pid| {
            state
                .player_prefs
                .lock()
                .expect("player_prefs mutex is never poisoned")
                .get(pid)
                .map(|prefs| prefs.replaygain)
        })
        .unwrap_or(ReplayGainPolicy::Disabled);

    let mode = match parsed
        .server_mode
        .as_deref()
        .unwrap_or("smart")
        .to_ascii_lowercase()
        .as_str()
    {
        "disabled" | "none" | "0" => ServerReplayGainMode::Disabled,
        "track" | "1" => ServerReplayGainMode::Track,
        "album" | "2" => ServerReplayGainMode::Album,
        _ => ServerReplayGainMode::Smart,
    };

    let gain = resolve_lms_track_gain(policy, mode, parsed.remote_replay_gain);
    Ok(json!({ "gain": gain }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ImporterNeedsUpdateParams {
    include_playlists: Option<bool>,
    now_epoch_secs: Option<u64>,
}

async fn handle_importer_needs_update(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: ImporterNeedsUpdateParams = parse_optional_params(params)?;
    let policy = if parsed.include_playlists.unwrap_or(true) {
        PlaylistSyncPolicy::IncludePlaylists
    } else {
        PlaylistSyncPolicy::IgnorePlaylists
    };
    let now = parsed.now_epoch_secs.unwrap_or_else(current_epoch_secs);

    let dont_import = state.with_global_prefs(|prefs| prefs.dont_import_accounts.clone());
    let mut clients = Vec::new();
    for (username, id) in state.accounts.all_credentials()? {
        if dont_import.contains(id.as_str()) {
            continue;
        }
        let client = state.build_client(Some(&username))?;
        clients.push((id.as_str().to_owned(), Arc::new(client)));
    }

    let needs_update = online_library_needs_update(&clients, &state.cache, policy, now).await?;
    Ok(json!({ "needs_update": needs_update }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DstmSeedDto {
    url: Option<String>,
    artist: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DstmParams {
    seeds: Vec<DstmSeedDto>,
    account: Option<String>,
}

async fn handle_integrations_dstm(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: DstmParams = parse_optional_params(params)?;
    let client = state.build_client(parsed.account.as_deref())?;
    let mut seeds = Vec::with_capacity(parsed.seeds.len());
    for dto in parsed.seeds {
        seeds.push(SeedTrackInput::new(dto.url, dto.artist, dto.title));
    }
    let tracks = DstmRecommender::generate_mix(&client, &seeds).await?;
    Ok(json!({ "tracks": tracks }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct HomeExtrasParams {
    tag: Option<String>,
}

fn handle_integrations_home_extras(params: Value) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: HomeExtrasParams = parse_optional_params(params)?;
    let resolved_menu = parsed.tag.as_deref().and_then(resolve_home_extra_menu);
    let extras: Vec<Value> = HOME_EXTRAS
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "tag": item.tag,
                "menu_param": item.menu_param,
                "title_token": item.title_token,
                "subtitle_token": item.subtitle_token,
                "needs_player": item.needs_player,
            })
        })
        .collect();
    Ok(json!({
        "extras": extras,
        "resolved_menu": resolved_menu,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LastMixParams {
    artist: String,
    title: String,
    best_only: bool,
    account: Option<String>,
}

async fn handle_integrations_last_mix(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: LastMixParams = parse_optional_params(params)?;
    let client_id = state.with_global_prefs(|prefs| Some(ClientId::from(prefs.icon_code.as_str())));
    let enabled = LastMixService::is_enabled(&state.accounts, client_id.as_ref());
    if !enabled || (parsed.artist.trim().is_empty() && parsed.title.trim().is_empty()) {
        return Ok(json!({
            "enabled": enabled,
            "candidates": [],
        }));
    }

    let client = state.build_client(parsed.account.as_deref())?;
    if parsed.best_only {
        let best = LastMixService::best_match(&client, &parsed.artist, &parsed.title).await?;
        let mapped = best.map(|cand| {
            json!({
                "title": cand.title,
                "artist": cand.artist,
                "url": cand.url,
            })
        });
        return Ok(json!({
            "enabled": enabled,
            "best": mapped,
        }));
    }

    let candidates =
        LastMixService::lookup_candidates(&client, &parsed.artist, &parsed.title).await?;
    let mapped: Vec<Value> = candidates
        .into_iter()
        .map(|cand| {
            json!({
                "title": cand.title,
                "artist": cand.artist,
                "url": cand.url,
            })
        })
        .collect();
    Ok(json!({
        "enabled": enabled,
        "candidates": mapped,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsBasicParams {
    params: BTreeMap<String, String>,
    display_names: BTreeMap<String, String>,
    products: BTreeMap<String, String>,
    now_epoch_secs: Option<u64>,
}

fn handle_settings_basic(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: SettingsBasicParams = parse_optional_params(params)?;
    let form = BasicSettingsForm::from_params(&parsed.params);
    let now = parsed.now_epoch_secs.unwrap_or_else(current_epoch_secs);
    let folder_upload_dir = state.cache_dir.join("spotty").join("playlistFolders");
    let diagnostics = HelperDiagnostics::new();

    let mut prefs_guard = state
        .global_prefs
        .lock()
        .expect("global_prefs mutex is never poisoned");
    let mut searches_guard = state
        .recent_searches
        .lock()
        .expect("recent_searches mutex is never poisoned");

    let submission = BasicSettingsController::handle_submit(
        &form,
        &mut prefs_guard,
        &state.accounts,
        &state.rate_limit_gate,
        &mut searches_guard,
        &folder_upload_dir,
        now,
        &parsed.display_names,
        &parsed.products,
        &diagnostics,
    )?;

    let outcome_json = match &submission.outcome {
        SettingsPostOutcome::RenderBasic(vm) => json!({
            "kind": "render_basic",
            "view_model": vm,
        }),
        SettingsPostOutcome::RedirectAuth { ajax_update } => json!({
            "kind": "redirect_auth",
            "ajax_update": ajax_update,
            "location": submission.outcome.redirect_location(),
        }),
        SettingsPostOutcome::RedirectPlayerAuth { ajax_update } => json!({
            "kind": "redirect_player_auth",
            "ajax_update": ajax_update,
            "location": submission.outcome.redirect_location(),
        }),
        _ => json!({
            "kind": "unknown",
        }),
    };

    let side_effects: Vec<String> = submission
        .side_effects
        .iter()
        .map(|se| format!("{se:?}"))
        .collect();
    Ok(json!({
        "outcome": outcome_json,
        "side_effects": side_effects,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AuthCheckParams {
    promote: bool,
}

fn handle_settings_auth_check(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: AuthCheckParams = parse_optional_params(params)?;
    let status = AuthLandingController::check_authenticate_status(&state.accounts);
    let promoted = if parsed.promote {
        AuthLandingController::cleanup_and_promote(&state.accounts)?
            .map(|id| id.as_str().to_owned())
    } else {
        None
    };
    Ok(json!({
        "status": status,
        "promoted": promoted,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OauthPrepareParams {
    lms_redirect_uri: String,
    user_agent: String,
    client_id: Option<String>,
}

async fn handle_settings_oauth_prepare(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: OauthPrepareParams = serde_json::from_value(params)?;
    let icon_code = match parsed.client_id {
        Some(raw) => IconCode::new(raw)?,
        None => state.with_global_prefs(|prefs| prefs.icon_code.clone()),
    };
    let controller = PkceCallbackController::new(
        Arc::clone(&state.cache),
        Arc::clone(&state.accounts),
        Arc::clone(&state.tokens),
    )?;
    let authorize_url = controller
        .prepare_oauth_redirect(&icon_code, &parsed.lms_redirect_uri, &parsed.user_agent)
        .await?;
    Ok(json!({ "authorize_url": authorize_url }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OauthCallbackParams {
    code: Option<String>,
    client_id: Option<String>,
}

async fn handle_settings_oauth_callback(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: OauthCallbackParams = parse_optional_params(params)?;
    let icon_code = match parsed.client_id {
        Some(raw) => IconCode::new(raw)?,
        None => state.with_global_prefs(|prefs| prefs.icon_code.clone()),
    };
    let controller = PkceCallbackController::new(
        Arc::clone(&state.cache),
        Arc::clone(&state.accounts),
        Arc::clone(&state.tokens),
    )?;
    let vm = controller
        .handle_oauth_callback(&icon_code, parsed.code.as_deref())
        .await?;
    Ok(serde_json::to_value(vm)?)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsPlayerParams {
    player_id: String,
    action: Option<String>,
    params: HashMap<String, String>,
    account: Option<String>,
}

async fn handle_settings_player(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: SettingsPlayerParams = parse_optional_params(params)?;
    let player_id = if parsed.player_id.trim().is_empty() {
        String::from("default")
    } else {
        parsed.player_id
    };
    let client = state.build_client(parsed.account.as_deref())?;
    let controller = PlayerSettingsController::new(&HelperCapabilities::new());

    if parsed.action.as_deref() == Some("save") {
        let mut prefs = state
            .player_prefs
            .lock()
            .expect("player_prefs mutex is never poisoned")
            .get(&player_id)
            .cloned()
            .unwrap_or_default();
        let vm = controller
            .save_prefs(&client, &mut prefs, &parsed.params)
            .await?;
        state
            .player_prefs
            .lock()
            .expect("player_prefs mutex is never poisoned")
            .insert(player_id, prefs);
        return Ok(serde_json::to_value(vm)?);
    }

    let prefs = state
        .player_prefs
        .lock()
        .expect("player_prefs mutex is never poisoned")
        .get(&player_id)
        .cloned()
        .unwrap_or_default();
    let vm = controller.load_view(&client, &prefs).await?;
    Ok(serde_json::to_value(vm)?)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PlayerAuthCheckParams {
    action: Option<String>,
    device_name: Option<String>,
    client_id: Option<String>,
}

fn handle_settings_player_auth_check(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: PlayerAuthCheckParams = parse_optional_params(params)?;
    match parsed.action.as_deref().unwrap_or("check") {
        "shutdown" => {
            state.player_auth_supervisor.shutdown();
            Ok(json!({ "running": false }))
        }
        "page" => {
            let device_name = parsed.device_name.as_deref().unwrap_or("Spotty");
            let client_id = match parsed.client_id {
                Some(raw) => raw,
                None => state.with_global_prefs(|prefs| prefs.icon_code.as_str().to_owned()),
            };
            let ap_fallback = state.with_global_prefs(|prefs| prefs.ap_fallback);
            let outcome = state.player_auth_supervisor.handle_page(
                &state.accounts,
                device_name,
                &client_id,
                ap_fallback,
            )?;
            Ok(json!({
                "outcome": format!("{outcome:?}"),
                "running": state.player_auth_supervisor.is_running(),
            }))
        }
        "start" => {
            let device_name = parsed.device_name.as_deref().unwrap_or("Spotty");
            let client_id = match parsed.client_id {
                Some(raw) => raw,
                None => state.with_global_prefs(|prefs| prefs.icon_code.as_str().to_owned()),
            };
            let ap_fallback = ApFallbackPolicy::default();
            let started = state.player_auth_supervisor.start(
                state.accounts.spotty_dir(),
                device_name,
                &client_id,
                ap_fallback,
            )?;
            Ok(json!({
                "started": started,
                "running": state.player_auth_supervisor.is_running(),
            }))
        }
        _ => {
            let status = check_player_credentials(&state.accounts);
            Ok(serde_json::to_value(status)?)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PlaylistFoldersParams {
    action: Option<String>,
    params: HashMap<String, String>,
}

fn handle_settings_playlist_folders(
    params: Value,
    state: &DaemonState,
) -> std::result::Result<Value, RpcDispatchError> {
    let parsed: PlaylistFoldersParams = parse_optional_params(params)?;
    let controller = PlaylistFoldersSettingsController::with_default_storage(
        state.accounts.spotty_dir(),
        Arc::clone(&state.cache),
    );
    let deleted = match parsed.action.as_deref().unwrap_or("load") {
        "delete" => controller.delete_from_params(&parsed.params)?,
        "purge" => controller.purge_upload_cache()?,
        _ => 0,
    };
    let vm = controller.load_view()?;
    Ok(json!({
        "deleted": deleted,
        "view_model": vm,
    }))
}

fn parse_optional_params<T: serde::de::DeserializeOwned + Default>(
    params: Value,
) -> std::result::Result<T, RpcDispatchError> {
    if params.is_null() {
        return Ok(T::default());
    }
    Ok(serde_json::from_value(params)?)
}

fn current_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs())
}
