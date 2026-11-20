//! LMS host callback contracts, player client identifiers, and commands.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Result;

/// Normalized LMS player identifier (typically a lowercase MAC address).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClientId(String);

impl ClientId {
    /// Creates a [`ClientId`], trimming whitespace and normalizing ASCII case.
    #[must_use]
    pub fn new(raw: impl AsRef<str>) -> Self {
        Self(raw.as_ref().trim().to_ascii_lowercase())
    }

    /// Returns the normalized client identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Extracts a 12-character lowercase hex MAC string if valid.
    ///
    /// Accepts colon-separated, dash-separated, or bare 12-hex MAC formats.
    #[must_use]
    pub fn to_mac12_hex(&self) -> Option<String> {
        let mut hex = String::with_capacity(12);
        for ch in self.0.chars() {
            if ch == ':' || ch == '-' {
                continue;
            }
            if !ch.is_ascii_hexdigit() {
                return None;
            }
            hex.push(ch.to_ascii_lowercase());
        }
        if hex.len() == 12 {
            Some(hex)
        } else {
            None
        }
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ClientId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ClientId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ClientId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Playback and playlist mutations dispatched to an LMS player client.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistCommand {
    /// Clears the target player's current playlist queue.
    Clear,
    /// Replaces or loads a list of Spotify track URIs into the player queue.
    PlayTracks(Vec<String>),
    /// Jumps playback to the zero-based playlist item index.
    JumpIndex(usize),
    /// Seeks the active track to the given offset in seconds.
    SeekSeconds(u64),
}

/// Host callbacks implemented by the LMS bridge or an in-memory test double.
pub trait LmsHostCallbacks: Send + Sync {
    /// Notifies LMS that track metadata for `client` has been updated.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::SpottyError`] if the RPC transport or host
    /// notification fails.
    fn notify_new_metadata(&self, client: &ClientId) -> Result<()>;

    /// Requests LMS to trigger an `OnlineLibrary` rescan pass.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::SpottyError`] if the rescan request cannot be
    /// delivered to the LMS host.
    fn trigger_online_library_rescan(&self) -> Result<()>;

    /// Executes a [`PlaylistCommand`] against the specified LMS `client`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::SpottyError`] if the host rejects or fails to
    /// dispatch the command.
    fn execute_playlist_command(&self, client: &ClientId, cmd: PlaylistCommand) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn client_id_normalizes_mac_to_lowercase() {
        let id = ClientId::new("  AA:BB:CC:DD:EE:FF ");
        assert_eq!(
            id.as_str(),
            "aa:bb:cc:dd:ee:ff",
            "expected lowercase trimmed ClientId, got {}",
            id.as_str()
        );
        assert_eq!(id.to_string(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn client_id_extracts_mac12_hex_for_valid_formats() {
        let colon = ClientId::from("AA:BB:CC:11:22:33");
        assert_eq!(colon.to_mac12_hex().as_deref(), Some("aabbcc112233"));

        let dash = ClientId::from("aa-bb-cc-11-22-33");
        assert_eq!(dash.to_mac12_hex().as_deref(), Some("aabbcc112233"));

        let bare = ClientId::from("AABBCC112233");
        assert_eq!(bare.to_mac12_hex().as_deref(), Some("aabbcc112233"));

        let invalid_len = ClientId::from("aa:bb:cc");
        assert_eq!(invalid_len.to_mac12_hex(), None);

        let non_hex = ClientId::from("player-living-room");
        assert_eq!(non_hex.to_mac12_hex(), None);
    }

    #[derive(Default)]
    struct MockHost {
        events: Mutex<Vec<String>>,
    }

    impl LmsHostCallbacks for MockHost {
        fn notify_new_metadata(&self, client: &ClientId) -> Result<()> {
            let mut guard = self.events.lock().expect("mock mutex poisoned");
            guard.push(format!("meta:{}", client.as_str()));
            Ok(())
        }

        fn trigger_online_library_rescan(&self) -> Result<()> {
            let mut guard = self.events.lock().expect("mock mutex poisoned");
            guard.push(String::from("rescan"));
            Ok(())
        }

        fn execute_playlist_command(&self, client: &ClientId, cmd: PlaylistCommand) -> Result<()> {
            let mut guard = self.events.lock().expect("mock mutex poisoned");
            guard.push(format!("cmd:{}:{cmd:?}", client.as_str()));
            Ok(())
        }
    }

    #[test]
    fn mock_host_callbacks_record_all_commands() {
        let host = MockHost::default();
        let client = ClientId::new("00:04:20:AA:BB:CC");

        host.notify_new_metadata(&client)
            .expect("notify_new_metadata should succeed");
        host.trigger_online_library_rescan()
            .expect("trigger_online_library_rescan should succeed");
        host.execute_playlist_command(&client, PlaylistCommand::Clear)
            .expect("clear command should succeed");
        host.execute_playlist_command(
            &client,
            PlaylistCommand::PlayTracks(vec![String::from("spotify:track:123")]),
        )
        .expect("play tracks command should succeed");
        host.execute_playlist_command(&client, PlaylistCommand::JumpIndex(2))
            .expect("jump command should succeed");
        host.execute_playlist_command(&client, PlaylistCommand::SeekSeconds(90))
            .expect("seek command should succeed");

        let guard = host.events.lock().expect("mock mutex poisoned");
        assert_eq!(guard.len(), 6, "expected 6 recorded events: {guard:?}");
    }
}
