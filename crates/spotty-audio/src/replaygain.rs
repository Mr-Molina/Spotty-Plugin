//! `ReplayGain` and `librespot` volume normalisation policy resolution.
//!
//! Mirrors `Plugins::Spotty::ProtocolHandler::trackGain`
//! (`ProtocolHandler.pm:38-49`) to prevent double `ReplayGain` attenuation when
//! `librespot` client-side volume normalisation is enabled.

use spotty_types::ReplayGainPolicy;

/// Client-side `librespot` volume normalisation toggle
/// (`--enable-volume-normalisation`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NormalisationMode {
    /// Disable `librespot` built-in volume normalisation (default).
    #[default]
    Disabled,
    /// Enable `librespot` built-in volume normalisation.
    Enabled,
}

impl From<ReplayGainPolicy> for NormalisationMode {
    fn from(policy: ReplayGainPolicy) -> Self {
        match policy {
            ReplayGainPolicy::ClientEnabled => Self::Enabled,
            _ => Self::Disabled,
        }
    }
}

/// Lyrion Music Server per-player `replayGainMode` preference.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ServerReplayGainMode {
    /// Server-side `ReplayGain` disabled (`0`).
    #[default]
    Disabled,
    /// Track-level `ReplayGain` (`1`).
    Track,
    /// Album-level `ReplayGain` (`2`).
    Album,
    /// Smart track/album `ReplayGain` (`3`).
    Smart,
}

/// Resolves the effective LMS `trackGain` value for a remote Spotify stream.
///
/// Matches `ProtocolHandler.pm:38-49`:
/// - Returns `None` whenever `spotty_client_replaygain` is
///   [`ReplayGainPolicy::ClientEnabled`], preventing double `ReplayGain`
///   attenuation when `librespot` performs normalisation itself.
/// - Otherwise returns `remote_replay_gain` when `server_mode` is not
///   [`ServerReplayGainMode::Disabled`], or `None` when server `ReplayGain` is
///   disabled.
#[must_use]
pub fn resolve_lms_track_gain(
    spotty_client_replaygain: ReplayGainPolicy,
    server_mode: ServerReplayGainMode,
    remote_replay_gain: Option<f32>,
) -> Option<f32> {
    if spotty_client_replaygain == ReplayGainPolicy::ClientEnabled {
        return None;
    }

    match server_mode {
        ServerReplayGainMode::Disabled => None,
        ServerReplayGainMode::Track | ServerReplayGainMode::Album | ServerReplayGainMode::Smart => {
            remote_replay_gain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_replaygain_enabled_suppresses_lms_track_gain() {
        for mode in [
            ServerReplayGainMode::Disabled,
            ServerReplayGainMode::Track,
            ServerReplayGainMode::Album,
            ServerReplayGainMode::Smart,
        ] {
            let gain = resolve_lms_track_gain(ReplayGainPolicy::ClientEnabled, mode, Some(-6.5));
            assert_eq!(
                gain, None,
                "client ReplayGain must suppress LMS gain for mode {mode:?}"
            );
        }
    }

    #[test]
    fn client_replaygain_disabled_respects_server_mode_and_remote_gain() {
        assert_eq!(
            resolve_lms_track_gain(
                ReplayGainPolicy::Disabled,
                ServerReplayGainMode::Disabled,
                Some(-4.2),
            ),
            None,
            "Disabled server mode must return None even with remote gain"
        );

        for mode in [
            ServerReplayGainMode::Track,
            ServerReplayGainMode::Album,
            ServerReplayGainMode::Smart,
        ] {
            assert_eq!(
                resolve_lms_track_gain(ReplayGainPolicy::Disabled, mode, Some(-4.2)),
                Some(-4.2),
                "server mode {mode:?} must forward remote gain when client RG is disabled"
            );
            assert_eq!(
                resolve_lms_track_gain(ReplayGainPolicy::Disabled, mode, None),
                None,
                "server mode {mode:?} must return None when remote gain is absent"
            );
        }
    }

    #[test]
    fn normalisation_mode_converts_from_replay_gain_policy() {
        assert_eq!(
            NormalisationMode::from(ReplayGainPolicy::Disabled),
            NormalisationMode::Disabled
        );
        assert_eq!(
            NormalisationMode::from(ReplayGainPolicy::ClientEnabled),
            NormalisationMode::Enabled
        );
    }
}
