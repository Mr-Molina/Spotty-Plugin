//! Legacy `spotty` helper CLI flag parser and `--check` capability report
//! formatter.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use serde::Deserialize;
use serde::Serialize;
use spotty_types::ApFallbackPolicy;
use spotty_types::Bitrate;
use spotty_types::Result;
use spotty_types::SpotifyUri;
use spotty_types::SpottyError;
use spotty_types::DEFAULT_ICON_HASH;

use crate::replaygain::NormalisationMode;
use crate::streamer::StreamRequest;
use crate::transcode::AudioCachePolicy;

const DEFAULT_DEVICE_NAME: &str = "Squeezebox";
const FALLBACK_AP_PORT: u16 = 12_321;

/// Helper capability flags serialized as JSON on line 2+ of `spotty --check`
/// output (`Helper.pm:101-122`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperCapabilities {
    /// Whether Spotify autoplay / radio continuation is supported.
    pub autoplay: bool,
    /// Whether podcast episode streaming is supported.
    pub podcasts: bool,
    /// Whether `--enable-volume-normalisation` is supported.
    #[serde(rename = "volume-normalisation")]
    pub volume_normalisation: bool,
    /// Whether the `--ap-port` fallback flag is unsupported/disabled.
    #[serde(rename = "no-ap-port")]
    pub no_ap_port: bool,
    /// Whether OAuth token materialization (`--get-token`) is supported.
    pub oauth: bool,
    /// Whether Zeroconf discovery port customization is supported.
    #[serde(rename = "zeroconf-port")]
    pub zeroconf_port: bool,
}

impl HelperCapabilities {
    /// Creates a [`HelperCapabilities`] table with the default in-process
    /// `Spotty-RS` feature flags enabled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for HelperCapabilities {
    fn default() -> Self {
        Self {
            autoplay: true,
            podcasts: true,
            volume_normalisation: true,
            no_ap_port: false,
            oauth: true,
            zeroconf_port: true,
        }
    }
}

/// Formats `spotty -n "Spotty" --check` output compatible with
/// `Plugins::Spotty::Helper::helperCheck` (`Helper.pm:101-114`).
///
/// Line 1 is `ok spotty v{version}` and line 2+ is pretty-printed JSON of
/// `caps`.
#[must_use]
pub fn format_helper_check_output(version: &str, caps: &HelperCapabilities) -> String {
    let trimmed = version.trim();
    let clean_version = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let json = serde_json::to_string_pretty(caps).unwrap_or_else(|_| String::from("{}"));
    format!("ok spotty v{clean_version}\n{json}")
}

/// Parsed action derived from legacy `spotty` helper CLI flags.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacySpottyCliAction {
    /// `--check` helper version and JSON capability probe (`Helper.pm`).
    CheckCapabilities {
        /// Announced device name (`-n` / `--name`).
        device_name: String,
    },
    /// `--single-track` raw PCM track stream request (`custom-convert.conf`).
    SingleTrackStream(StreamRequest),
    /// `--get-token --access-token=...` OAuth credential materialization
    /// (`Callback.pm`).
    MaterializeToken {
        /// Target cache directory (`-c` / `--cache`).
        cache_dir: PathBuf,
        /// Spotify application client ID (`--client-id`).
        client_id: String,
        /// OAuth access token (`--access-token`).
        access_token: String,
    },
    /// `--authenticate` Zeroconf discovery login (`PlayerAuth.pm`).
    ZeroconfAuthenticate {
        /// Target cache directory (`-c` / `--cache`).
        cache_dir: PathBuf,
        /// Announced Zeroconf device name (`-n` / `--name`).
        device_name: String,
        /// Access point port fallback policy (`--ap-port=12321`).
        ap_fallback: ApFallbackPolicy,
    },
    /// `--authenticate --zeroconf-port=<port>` Zeroconf discovery login with
    /// explicit HTTP listener port.
    ZeroconfAuthenticateOnPort {
        /// Target cache directory (`-c` / `--cache`).
        cache_dir: PathBuf,
        /// Announced Zeroconf device name (`-n` / `--name`).
        device_name: String,
        /// Access point port fallback policy (`--ap-port=...`).
        ap_fallback: ApFallbackPolicy,
        /// Explicit Zeroconf HTTP listener port (`--zeroconf-port`).
        zeroconf_port: u16,
    },
}

impl LegacySpottyCliAction {
    /// Returns the explicit `--zeroconf-port` if this action is
    /// [`LegacySpottyCliAction::ZeroconfAuthenticateOnPort`].
    #[must_use]
    pub fn zeroconf_port(&self) -> Option<u16> {
        match self {
            Self::ZeroconfAuthenticateOnPort { zeroconf_port, .. } => Some(*zeroconf_port),
            _ => None,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct LegacySpottyArgs {
    #[arg(short = 'n', long = "name")]
    name: Option<String>,

    #[arg(short = 'c', long = "cache")]
    cache: Option<PathBuf>,

    #[arg(long = "check")]
    check: bool,

    #[arg(long = "single-track")]
    single_track: Option<String>,

    #[arg(long = "start-position")]
    start_position: Option<f64>,

    #[arg(long = "bitrate")]
    bitrate: Option<u16>,

    #[arg(long = "disable-discovery")]
    disable_discovery: bool,

    #[arg(long = "disable-audio-cache")]
    disable_audio_cache: bool,

    #[arg(long = "enable-volume-normalisation")]
    enable_volume_normalisation: bool,

    #[arg(long = "ap-port")]
    ap_port: Option<u16>,

    #[arg(long = "zeroconf-port")]
    zeroconf_port: Option<u16>,

    #[arg(long = "get-token")]
    get_token: bool,

    #[arg(long = "client-id")]
    client_id: Option<String>,

    #[arg(long = "scope")]
    scope: Option<String>,

    #[arg(long = "access-token")]
    access_token: Option<String>,

    #[arg(long = "authenticate")]
    authenticate: bool,

    #[arg(long = "verbose")]
    verbose: bool,
}

/// Parses legacy `spotty` helper command-line arguments used by
/// `custom-convert.conf`, `Helper.pm`, `Callback.pm`, and `PlayerAuth.pm`.
///
/// Accepts either flag-only slices (`["-n", "Spotty", "--check"]`) or full
/// `argv` slices beginning with the executable name (`["spotty", ...]`).
/// Returns `Ok(None)` when no action flag (`--check`, `--single-track`,
/// `--get-token`, or `--authenticate`) is present.
///
/// # Errors
///
/// Returns [`SpottyError::Audio`] for unknown/conflicting flags, unsupported
/// `--bitrate` or `--ap-port` values, or missing required `-c`/`--cache` paths;
/// returns [`SpottyError::InvalidUri`] if `--single-track` is not a valid
/// Spotify URI; returns [`SpottyError::NoAccessToken`] if `--get-token` omits
/// `--access-token`.
pub fn parse_legacy_spotty_args<I, S>(args: I) -> Result<Option<LegacySpottyCliAction>>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let Some(first) = raw.first() else {
        return Ok(None);
    };

    let cli_slice = if is_binary_argv0(first) {
        &raw[1..]
    } else {
        &raw[..]
    };
    if cli_slice.is_empty() {
        return Ok(None);
    }

    let parsed = LegacySpottyArgs::try_parse_from(cli_slice)
        .map_err(|err| SpottyError::Audio(format!("invalid legacy spotty CLI arguments: {err}")))?;

    if parsed.verbose {
        tracing::debug!("legacy spotty CLI invoked with --verbose");
    }
    if let Some(scope) = parsed.scope.as_deref() {
        if scope.trim().is_empty() {
            return Err(SpottyError::Audio(String::from("--scope cannot be empty")));
        }
        tracing::debug!(scope = %scope, "legacy spotty CLI --scope provided");
    }

    let device_name = parsed
        .name
        .unwrap_or_else(|| String::from(DEFAULT_DEVICE_NAME));
    let (ap_fallback, ap_port) = parse_ap_fallback(parsed.ap_port)?;

    let action_count = usize::from(parsed.check)
        + usize::from(parsed.single_track.is_some())
        + usize::from(parsed.get_token)
        + usize::from(parsed.authenticate);
    if action_count == 0 {
        return Ok(None);
    }
    if action_count > 1 {
        return Err(SpottyError::Audio(String::from(
            "conflicting legacy spotty action flags provided",
        )));
    }

    if parsed.check {
        return Ok(Some(LegacySpottyCliAction::CheckCapabilities {
            device_name,
        }));
    }

    let cache_dir = parsed.cache.ok_or_else(|| {
        SpottyError::Audio(String::from(
            "missing required -c/--cache directory argument",
        ))
    })?;

    if let Some(track_raw) = parsed.single_track {
        let uri: SpotifyUri = track_raw.parse()?;
        let bitrate = parse_bitrate(parsed.bitrate)?;
        let (start_secs, start_ms) = parse_start_position(parsed.start_position)?;
        let mut req = StreamRequest::new(cache_dir, uri);
        req.start_offset_secs = start_secs;
        req.start_offset_ms = start_ms;
        req.bitrate = bitrate;
        req.audio_cache = if parsed.disable_audio_cache {
            AudioCachePolicy::Disabled
        } else {
            AudioCachePolicy::Enabled
        };
        req.normalisation = if parsed.enable_volume_normalisation {
            NormalisationMode::Enabled
        } else {
            NormalisationMode::Disabled
        };
        req.ap_fallback = ap_fallback;
        req.ap_port = ap_port;
        req.device_name = device_name;
        req.verbose = parsed.verbose;
        req.disable_discovery = parsed.disable_discovery;
        return Ok(Some(LegacySpottyCliAction::SingleTrackStream(req)));
    }

    if parsed.get_token {
        let access_token = parsed
            .access_token
            .filter(|tok| !tok.trim().is_empty())
            .ok_or(SpottyError::NoAccessToken)?;
        let client_id = match parsed.client_id {
            Some(id) if id.trim().is_empty() => {
                return Err(SpottyError::Audio(String::from(
                    "--client-id cannot be empty",
                )));
            }
            Some(id) => id,
            None => String::from(DEFAULT_ICON_HASH),
        };
        return Ok(Some(LegacySpottyCliAction::MaterializeToken {
            cache_dir,
            client_id,
            access_token,
        }));
    }

    if let Some(zeroconf_port) = parsed.zeroconf_port {
        return Ok(Some(LegacySpottyCliAction::ZeroconfAuthenticateOnPort {
            cache_dir,
            device_name,
            ap_fallback,
            zeroconf_port,
        }));
    }

    Ok(Some(LegacySpottyCliAction::ZeroconfAuthenticate {
        cache_dir,
        device_name,
        ap_fallback,
    }))
}

fn is_binary_argv0(arg: &OsStr) -> bool {
    let Some(s) = arg.to_str() else {
        return false;
    };
    if s.starts_with('-') {
        return false;
    }
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    let stem = Path::new(s)
        .file_stem()
        .and_then(|st| st.to_str())
        .unwrap_or(s);
    stem.eq_ignore_ascii_case("spotty") || stem.eq_ignore_ascii_case("lms-spotty")
}

fn parse_start_position(raw: Option<f64>) -> Result<(Option<u64>, Option<u32>)> {
    let Some(secs) = raw else {
        return Ok((None, None));
    };
    if !secs.is_finite() || secs < 0.0 {
        return Err(SpottyError::Audio(format!(
            "invalid --start-position {secs}; expected non-negative finite seconds"
        )));
    }
    let ms_f64 = (secs * 1_000.0).round();
    if ms_f64 > f64::from(u32::MAX) {
        return Err(SpottyError::Audio(format!(
            "seek offset {secs}s exceeds 32-bit millisecond limit"
        )));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole_secs = secs.trunc() as u64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = ms_f64 as u32;
    Ok((Some(whole_secs), Some(ms)))
}

fn parse_bitrate(raw: Option<u16>) -> Result<Bitrate> {
    match raw {
        None | Some(320) => Ok(Bitrate::Kbps320),
        Some(160) => Ok(Bitrate::Kbps160),
        Some(96) => Ok(Bitrate::Kbps96),
        Some(other) => Err(SpottyError::Audio(format!(
            "unsupported Spotify bitrate {other}; expected 96, 160, or 320"
        ))),
    }
}

fn parse_ap_fallback(ap_port: Option<u16>) -> Result<(ApFallbackPolicy, Option<u16>)> {
    match ap_port {
        None => Ok((ApFallbackPolicy::DefaultPort, None)),
        Some(0) => Err(SpottyError::Audio(String::from(
            "invalid --ap-port 0; expected 1..=65535",
        ))),
        Some(FALLBACK_AP_PORT) => Ok((ApFallbackPolicy::ForcePort12321, Some(FALLBACK_AP_PORT))),
        Some(port) => Ok((ApFallbackPolicy::DefaultPort, Some(port))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_check_output_matches_helper_pm_header_and_hyphenated_json_schema() {
        let caps = HelperCapabilities::default();
        let output = format_helper_check_output("4.62.3", &caps);
        let mut lines = output.lines();
        let first_line = lines.next().expect("line 1 must exist");
        assert_eq!(
            first_line, "ok spotty v4.62.3",
            "line 1 must match Helper.pm /^ok spotty v([\\d\\.]+)/i"
        );

        let body: String = lines.collect::<Vec<_>>().join("\n");
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("line 2+ must parse as valid JSON");
        assert_eq!(parsed["autoplay"], serde_json::json!(true));
        assert_eq!(parsed["podcasts"], serde_json::json!(true));
        assert_eq!(parsed["volume-normalisation"], serde_json::json!(true));
        assert_eq!(parsed["no-ap-port"], serde_json::json!(false));
        assert_eq!(parsed["oauth"], serde_json::json!(true));
        assert_eq!(parsed["zeroconf-port"], serde_json::json!(true));

        let prefixed = format_helper_check_output("v4.62.3", &caps);
        assert_eq!(
            prefixed.lines().next(),
            Some("ok spotty v4.62.3"),
            "leading 'v' must not produce 'vv'"
        );
    }

    #[test]
    fn parses_helper_pm_check_command_with_and_without_binary_name() {
        let without_bin = parse_legacy_spotty_args(["-n", "Spotty", "--check"])
            .expect("--check without binary name should parse");
        assert_eq!(
            without_bin,
            Some(LegacySpottyCliAction::CheckCapabilities {
                device_name: String::from("Spotty"),
            })
        );

        let with_bin = parse_legacy_spotty_args(["/usr/bin/spotty", "-n", "Spotty", "--check"])
            .expect("--check with binary name should parse");
        assert_eq!(with_bin, without_bin);
    }

    #[test]
    fn parses_custom_convert_single_track_stream_command() {
        let args = [
            "spotty",
            "-n",
            "Squeezebox",
            "-c",
            "/var/cache/spotty/deadbeef",
            "--single-track",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "--bitrate",
            "320",
            "--disable-discovery",
            "--disable-audio-cache",
            "--enable-volume-normalisation",
            "--ap-port=12321",
            "--start-position",
            "42",
            "--verbose",
        ];

        let parsed = parse_legacy_spotty_args(args)
            .expect("custom-convert.conf command line should parse")
            .expect("action should be Some");

        let LegacySpottyCliAction::SingleTrackStream(req) = parsed else {
            panic!("expected SingleTrackStream, got {parsed:?}");
        };
        assert_eq!(req.cache_dir, PathBuf::from("/var/cache/spotty/deadbeef"));
        assert_eq!(
            req.uri.to_colon_uri(),
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC"
        );
        assert_eq!(req.start_offset_secs, Some(42));
        assert_eq!(req.bitrate, Bitrate::Kbps320);
        assert_eq!(req.audio_cache, AudioCachePolicy::Disabled);
        assert_eq!(req.normalisation, NormalisationMode::Enabled);
        assert_eq!(req.ap_fallback, ApFallbackPolicy::ForcePort12321);
        assert_eq!(req.device_name, "Squeezebox");
    }

    #[test]
    fn parses_single_track_without_optional_disable_flags() {
        let args = [
            "-c",
            "/cache",
            "--single-track",
            "spotify://track:4uLU6hMCjMI75M1A2tKUQC",
            "--bitrate",
            "160",
        ];
        let parsed = parse_legacy_spotty_args(args)
            .expect("minimal single-track command should parse")
            .expect("action should be Some");

        let LegacySpottyCliAction::SingleTrackStream(req) = parsed else {
            panic!("expected SingleTrackStream, got {parsed:?}");
        };
        assert_eq!(req.bitrate, Bitrate::Kbps160);
        assert_eq!(req.audio_cache, AudioCachePolicy::Enabled);
        assert_eq!(req.normalisation, NormalisationMode::Disabled);
        assert_eq!(req.ap_fallback, ApFallbackPolicy::DefaultPort);
        assert_eq!(req.start_offset_secs, None);
    }

    #[test]
    fn parses_callback_pm_get_token_command() {
        let args = [
            "spotty",
            "-n",
            "Squeezebox",
            "-c",
            "/var/cache/spotty/__AUTHENTICATE__",
            "--client-id",
            "93aac68fb06348598c1e67734dfaceee",
            "--disable-discovery",
            "--get-token",
            "--scope",
            "playlist-read+streaming",
            "--access-token=tok_abc_123",
        ];

        let parsed = parse_legacy_spotty_args(args)
            .expect("Callback.pm command should parse")
            .expect("action should be Some");

        assert_eq!(
            parsed,
            LegacySpottyCliAction::MaterializeToken {
                cache_dir: PathBuf::from("/var/cache/spotty/__AUTHENTICATE__"),
                client_id: String::from("93aac68fb06348598c1e67734dfaceee"),
                access_token: String::from("tok_abc_123"),
            }
        );
    }

    #[test]
    fn parses_player_auth_pm_authenticate_command() {
        let args = [
            "-c",
            "/var/cache/spotty",
            "-n",
            "Spotty (My Server)",
            "--authenticate",
            "--ap-port=12321",
            "--verbose",
        ];

        let parsed = parse_legacy_spotty_args(args)
            .expect("PlayerAuth.pm command should parse")
            .expect("action should be Some");

        assert_eq!(
            parsed,
            LegacySpottyCliAction::ZeroconfAuthenticate {
                cache_dir: PathBuf::from("/var/cache/spotty"),
                device_name: String::from("Spotty (My Server)"),
                ap_fallback: ApFallbackPolicy::ForcePort12321,
            }
        );
    }

    #[test]
    fn returns_none_when_no_action_flag_is_present_and_rejects_invalid_inputs() {
        assert_eq!(
            parse_legacy_spotty_args::<[&str; 0], &str>([]).expect("empty args should return None"),
            None
        );
        assert_eq!(
            parse_legacy_spotty_args(["spotty", "--verbose"])
                .expect("flags without action should return None"),
            None
        );

        let missing_cache = parse_legacy_spotty_args(["--authenticate"])
            .expect_err("missing -c cache dir must fail");
        assert!(matches!(missing_cache, SpottyError::Audio(_)));

        let missing_token = parse_legacy_spotty_args(["-c", "/tmp", "--get-token"])
            .expect_err("--get-token without --access-token must fail");
        assert_eq!(missing_token, SpottyError::NoAccessToken);

        let bad_bitrate = parse_legacy_spotty_args([
            "-c",
            "/tmp",
            "--single-track",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "--bitrate",
            "128",
        ])
        .expect_err("unsupported bitrate must fail");
        assert!(matches!(bad_bitrate, SpottyError::Audio(_)));

        let conflict = parse_legacy_spotty_args(["-c", "/tmp", "--check", "--authenticate"])
            .expect_err("conflicting actions must fail");
        assert!(matches!(conflict, SpottyError::Audio(_)));
    }

    #[test]
    fn parses_zeroconf_port_float_start_position_general_ap_port_and_flags() {
        let auth = parse_legacy_spotty_args([
            "spotty",
            "-c",
            "/var/cache/spotty",
            "-n",
            "Spotty",
            "--authenticate",
            "--zeroconf-port",
            "44100",
        ])
        .expect("zeroconf-port should parse")
        .expect("action");
        assert_eq!(auth.zeroconf_port(), Some(44_100));
        assert!(matches!(
            auth,
            LegacySpottyCliAction::ZeroconfAuthenticateOnPort {
                zeroconf_port: 44_100,
                ..
            }
        ));

        let stream = parse_legacy_spotty_args([
            "-c",
            "/cache",
            "--single-track",
            "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
            "--start-position",
            "12.5",
            "--ap-port",
            "4070",
            "--disable-discovery",
            "--verbose",
        ])
        .expect("float start-position and general ap-port should parse")
        .expect("action");
        let LegacySpottyCliAction::SingleTrackStream(req) = stream else {
            panic!("expected SingleTrackStream");
        };
        assert_eq!(req.start_offset_secs, Some(12));
        assert_eq!(req.start_offset_ms, Some(12_500));
        assert_eq!(req.ap_port, Some(4070));
        assert!(req.verbose);
        assert!(req.disable_discovery);

        let bad_pos = parse_legacy_spotty_args(["unexpected-arg", "--check"])
            .expect_err("unexpected positional arg must not be stripped as argv[0]");
        assert!(matches!(bad_pos, SpottyError::Audio(_)));

        let bad_scope = parse_legacy_spotty_args(["-c", "/tmp", "--check", "--scope", "  "])
            .expect_err("empty --scope must fail");
        assert!(matches!(bad_scope, SpottyError::Audio(_)));
    }
}
