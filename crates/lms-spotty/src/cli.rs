//! Dual-persona CLI parser and execution entrypoint supporting legacy
//! `spotty` helper flags and modern `lms-spotty` subcommands.

use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use spotty_api::{
    Pipeline, RateLimitGate, RateLimitPolicy, SpotifyClient, SpotifyTransport, TokenManager,
};
use spotty_audio::{
    format_helper_check_output, materialize_oauth_credentials, parse_legacy_spotty_args,
    HelperCapabilities, LegacySpottyCliAction, StreamRequest, TrackStreamer, ZeroconfAuthenticator,
    ZEROCONF_AUTH_TIMEOUT_SECS,
};
use spotty_cache::{AccountStore, DualCache, AUTHENTICATE_DIR};
use spotty_importer::{ImporterRunner, ScanScope, SqliteLibrarySink};
use spotty_types::{Bitrate, IconCode, Result, SpotifyUri, SpottyError};

use crate::daemon::{DaemonConfig, SpottyDaemon, DEFAULT_BIND_ADDR};
use crate::rpc_protocol::{default_endpoint_path, SPOTTY_VERSION};

/// Top-level `lms-spotty` CLI command parser.
#[non_exhaustive]
#[derive(Debug, Parser)]
#[command(
    name = "lms-spotty",
    version = SPOTTY_VERSION,
    about = "Unified Spotty-RS JSON-RPC daemon, library scanner, and drop-in audio helper"
)]
pub struct CliCommand {
    /// Subcommand to execute when not invoked in legacy `spotty` helper mode.
    #[command(subcommand)]
    pub subcommand: CliSubcommand,
}

/// Supported `lms-spotty` subcommands.
#[non_exhaustive]
#[derive(Debug, Subcommand)]
pub enum CliSubcommand {
    /// Runs the persistent newline-delimited JSON-RPC 2.0 daemon.
    Daemon(DaemonCommandArgs),
    /// Scans Spotify library accounts into the LMS SQLite `library.db`.
    Scan(ScanCommandArgs),
    /// Streams a Spotify track URI as raw PCM S16LE to stdout.
    Stream(StreamCommandArgs),
    /// Prints `spotty` helper version and JSON capabilities (`--check`).
    Check,
}

/// Arguments for `lms-spotty daemon`.
#[non_exhaustive]
#[derive(Debug, Args)]
pub struct DaemonCommandArgs {
    /// Base cache directory containing `spotty/` state and `cache.redb`.
    #[arg(long = "cache-dir")]
    pub cache_dir: PathBuf,
    /// Optional path for the atomic JSON endpoint discovery file.
    #[arg(long = "endpoint-file")]
    pub endpoint_file: Option<PathBuf>,
    /// TCP bind address (`127.0.0.1:0` by default).
    #[arg(long = "bind", default_value = DEFAULT_BIND_ADDR)]
    pub bind: String,
}

/// Library scan scope for `lms-spotty scan`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum CliScanScope {
    /// Scan albums, artists, and playlists.
    #[default]
    Full,
    /// Scan albums and artists only, skipping playlists.
    LibraryOnly,
    /// Scan playlists only.
    PlaylistsOnly,
}

impl From<CliScanScope> for ScanScope {
    fn from(scope: CliScanScope) -> Self {
        match scope {
            CliScanScope::Full => Self::FullLibrary,
            CliScanScope::LibraryOnly => Self::LibraryWithoutPlaylists,
            CliScanScope::PlaylistsOnly => Self::PlaylistsOnly,
        }
    }
}

/// Arguments for `lms-spotty scan`.
#[non_exhaustive]
#[derive(Debug, Args)]
pub struct ScanCommandArgs {
    /// Base cache directory containing `spotty/` accounts and `cache.redb`.
    #[arg(long = "cache-dir")]
    pub cache_dir: PathBuf,
    /// Path to the LMS `library.db` SQLite database.
    #[arg(long = "lms-db")]
    pub lms_db: PathBuf,
    /// Scan scope (`full`, `library-only`, or `playlists-only`).
    #[arg(long = "scope", value_enum, default_value_t = CliScanScope::Full)]
    pub scope: CliScanScope,
}

/// Arguments for `lms-spotty stream`.
#[non_exhaustive]
#[derive(Debug, Args)]
pub struct StreamCommandArgs {
    /// Base cache directory containing `spotty/` credentials.
    #[arg(long = "cache-dir")]
    pub cache_dir: PathBuf,
    /// Spotify track or episode URI (`spotify:track:<id>`).
    #[arg(long = "uri")]
    pub uri: String,
    /// Optional playback start offset in seconds.
    #[arg(long = "start-position")]
    pub start_position: Option<u64>,
    /// Target bitrate in kbps (`96`, `160`, or `320`).
    #[arg(long = "bitrate", default_value_t = 320)]
    pub bitrate: u16,
}

/// Executes either legacy `spotty` helper mode or modern `lms-spotty` CLI
/// subcommands.
///
/// # Errors
///
/// Returns [`SpottyError`] if argument parsing, daemon startup, scanning,
/// authentication, or audio streaming fails.
pub async fn run_cli<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let raw_args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.into().to_string_lossy().into_owned())
        .collect();

    if contains_legacy_action_flag(&raw_args) {
        if let Some(action) = parse_legacy_spotty_args(&raw_args)? {
            return execute_legacy_action(action).await;
        }
    }

    let parsed = match CliCommand::try_parse_from(&raw_args) {
        Ok(cmd) => cmd,
        Err(err) => {
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                err.print()
                    .map_err(|io_err| SpottyError::Io(io_err.to_string()))?;
                return Ok(());
            }
            return Err(SpottyError::Audio(err.to_string()));
        }
    };
    match parsed.subcommand {
        CliSubcommand::Daemon(daemon_args) => execute_daemon(daemon_args).await,
        CliSubcommand::Scan(scan_args) => execute_scan(scan_args).await,
        CliSubcommand::Stream(stream_args) => execute_stream(stream_args).await,
        CliSubcommand::Check => {
            println!(
                "{}",
                format_helper_check_output(SPOTTY_VERSION, &HelperCapabilities::new())
            );
            Ok(())
        }
    }
}

/// Returns `true` when raw CLI arguments include a legacy `spotty` helper
/// action flag (`--check`, `--single-track`, `--get-token`, or
/// `--authenticate`).
#[must_use]
pub fn contains_legacy_action_flag(args: &[String]) -> bool {
    for arg in args.iter().skip(1) {
        if arg == "--check"
            || arg == "--single-track"
            || arg.starts_with("--single-track=")
            || arg == "--get-token"
            || arg == "--authenticate"
        {
            return true;
        }
    }
    false
}

async fn execute_legacy_action(action: LegacySpottyCliAction) -> Result<()> {
    match action {
        LegacySpottyCliAction::CheckCapabilities { device_name: _ } => {
            println!(
                "{}",
                format_helper_check_output(SPOTTY_VERSION, &HelperCapabilities::new())
            );
            Ok(())
        }
        LegacySpottyCliAction::SingleTrackStream(request) => {
            let streamer = TrackStreamer::with_librespot();
            let mut stdout = tokio::io::stdout();
            let _bytes = streamer.stream_pcm_s16le(&request, &mut stdout).await?;
            Ok(())
        }
        LegacySpottyCliAction::MaterializeToken {
            cache_dir,
            client_id,
            access_token,
        } => {
            let username = if client_id.trim().is_empty() {
                AUTHENTICATE_DIR
            } else {
                client_id.as_str()
            };
            let _path = materialize_oauth_credentials(&cache_dir, &access_token, username)?;
            Ok(())
        }
        LegacySpottyCliAction::ZeroconfAuthenticate {
            cache_dir,
            device_name,
            ap_fallback,
        } => {
            let authenticator = ZeroconfAuthenticator::with_librespot();
            let client_id = IconCode::default();
            let _record = authenticator
                .authenticate_with_timeout(
                    &cache_dir,
                    &device_name,
                    client_id.as_str(),
                    ap_fallback,
                    Duration::from_secs(ZEROCONF_AUTH_TIMEOUT_SECS),
                )
                .await?;
            Ok(())
        }
        _ => Err(SpottyError::Audio(String::from(
            "unsupported legacy spotty CLI action",
        ))),
    }
}

async fn execute_daemon(args: DaemonCommandArgs) -> Result<()> {
    let endpoint_file = args
        .endpoint_file
        .unwrap_or_else(|| default_endpoint_path(&args.cache_dir));
    let config = DaemonConfig::new(&args.cache_dir)
        .with_endpoint_file(endpoint_file)
        .with_bind_addr(args.bind);
    let daemon = SpottyDaemon::bind(config).await?;
    daemon.serve().await
}

async fn execute_scan(args: ScanCommandArgs) -> Result<()> {
    let spotty_dir = args.cache_dir.join("spotty");
    std::fs::create_dir_all(&spotty_dir)?;

    let accounts_store = Arc::new(AccountStore::new(&args.cache_dir));
    let cache = Arc::new(DualCache::open(spotty_dir.join("cache.redb"))?);
    let tokens = Arc::new(TokenManager::new(
        Arc::clone(&cache),
        Arc::clone(&accounts_store),
    )?);
    let gate = Arc::new(RateLimitGate::new());
    let icon_code = IconCode::default();

    let mut account_clients = Vec::new();
    for (username, id) in accounts_store.all_credentials()? {
        let transport = Arc::new(SpotifyTransport::new(
            Arc::clone(&tokens),
            Arc::clone(&cache),
            Arc::clone(&gate),
            icon_code.clone(),
            username.as_str(),
        )?);
        let pipeline = Pipeline::new(Arc::clone(&transport));
        let client = SpotifyClient::new(
            transport,
            pipeline,
            Arc::clone(&cache),
            icon_code.clone(),
            username,
            "US",
            RateLimitPolicy::SleepAndRetryScanner,
        );
        account_clients.push((id.as_str().to_owned(), Arc::new(client)));
    }

    let runner = ImporterRunner::new(account_clients, cache);
    let mut sink = SqliteLibrarySink::open(&args.lms_db)?;
    let now_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_secs());

    let summary = runner
        .run_scan(args.scope.into(), &mut sink, now_epoch_secs)
        .await?;
    let payload = json!({
        "accounts_scanned": summary.accounts_scanned,
        "tracks_stored": summary.tracks_stored,
        "artists_upserted": summary.artists_upserted,
        "playlists_purged": summary.playlists_purged,
        "playlists_upserted": summary.playlists_upserted,
        "tracks_pruned": summary.tracks_pruned,
    });
    println!("{payload}");
    Ok(())
}

async fn execute_stream(args: StreamCommandArgs) -> Result<()> {
    let uri = SpotifyUri::from_str(&args.uri)?;
    let bitrate = match args.bitrate {
        96 => Bitrate::Kbps96,
        160 => Bitrate::Kbps160,
        320 => Bitrate::Kbps320,
        other => {
            return Err(SpottyError::Audio(format!(
                "unsupported bitrate {other} (expected 96, 160, or 320)"
            )));
        }
    };
    let mut request = StreamRequest::new(args.cache_dir, uri);
    request.bitrate = bitrate;
    request.start_offset_secs = args.start_position;
    let streamer = TrackStreamer::with_librespot();
    let mut stdout = tokio::io::stdout();
    let _bytes = streamer.stream_pcm_s16le(&request, &mut stdout).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn clap_command_verifies_without_conflicts() {
        CliCommand::command().debug_assert();
    }

    #[test]
    fn legacy_action_detection_matches_spotty_helper_flags() {
        let check_args = vec![
            String::from("lms-spotty"),
            String::from("-n"),
            String::from("Spotty"),
            String::from("--check"),
        ];
        assert!(contains_legacy_action_flag(&check_args));

        let modern_args = vec![
            String::from("lms-spotty"),
            String::from("daemon"),
            String::from("--cache-dir"),
            String::from("/tmp/cache"),
        ];
        assert!(!contains_legacy_action_flag(&modern_args));
    }
}
