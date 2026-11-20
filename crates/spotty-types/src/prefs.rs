//! Global plugin preferences, per-player preferences, and Perl migration
//! helpers.

use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;

use crate::manifest::IconCode;

/// Spotify audio stream bitrate passed to the `librespot` helper.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Bitrate {
    /// 96 kbps Ogg Vorbis stream.
    Kbps96,
    /// 160 kbps Ogg Vorbis stream.
    Kbps160,
    /// 320 kbps Ogg Vorbis stream (default).
    #[default]
    Kbps320,
}

impl Bitrate {
    /// Returns the numeric bitrate in kilobits per second.
    #[must_use]
    pub fn kbps(&self) -> u16 {
        match self {
            Self::Kbps96 => 96,
            Self::Kbps160 => 160,
            Self::Kbps320 => 320,
        }
    }
}

/// Per-player explicit content filtering mode (`Settings/Player.pm`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ExplicitFilterMode {
    /// Allow both clean and explicit tracks (`0`).
    #[default]
    AllowAll,
    /// Hide explicit tracks in browse menus (`1`).
    HideExplicit,
    /// Hide explicit tracks and block playback (`2`).
    StrictMaster,
}

/// Collection sort order preference (`Plugin.pm` / `Settings.pm`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    /// Sort items alphabetically by title or artist name.
    Alphabetical,
    /// Preserve natural/chronological order (`added_at` or API order).
    NaturalOrder,
}

/// Spotify Access Point port fallback policy (`Plugin.pm` migration v3).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ApFallbackPolicy {
    /// Use the default `librespot` access point port selection.
    #[default]
    DefaultPort,
    /// Pass `--ap-port=12321` to force the fallback access point.
    ForcePort12321,
}

/// Track and album title suffix cleanup mode (`cleanupTags`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum TagCleanupPolicy {
    /// Strip `(Remastered ...)`, `[Deluxe Edition]`, and similar suffixes.
    #[default]
    StripSuffixes,
    /// Preserve raw Spotify metadata titles unmodified.
    PreserveRaw,
}

/// Artist view album visibility scope (`myAlbumsOnly`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AlbumScope {
    /// Show only albums saved in the user's library under an artist.
    MyAlbumsOnly,
    /// Show the artist's full Spotify catalog (default).
    #[default]
    FullArtistCatalog,
}

/// Top-level account switcher menu visibility (`accountSwitcherMenu`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum AccountSwitcherVisibility {
    /// Hide the account switcher menu for simplified switching (default).
    #[default]
    Hidden,
    /// Show the account switcher menu inside Spotty browse menus.
    VisibleInMenu,
}

/// Podcast episode listing order (`reversePodcastOrder`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum PodcastEpisodeOrder {
    /// List episodes oldest-first (default).
    #[default]
    OldestFirst,
    /// Reverse order to show newest episodes first.
    ReverseNewestFirst,
}

/// Per-player Spotty client `ReplayGain` normalization policy (`replaygain`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ReplayGainPolicy {
    /// Disable Spotty helper volume normalization (default).
    #[default]
    Disabled,
    /// Enable Spotty `--enable-volume-normalisation` on this player.
    ClientEnabled,
}

/// Global plugin preferences matching `Plugins::Spotty::Plugin` defaults.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalPrefs {
    /// Two-letter ISO 3166-1 alpha-2 country code (`"US"`).
    pub country: String,
    /// Title tag suffix cleanup behavior (`cleanupTags => 1`).
    pub cleanup_tags: TagCleanupPolicy,
    /// Configured Spotify audio streaming bitrate (`bitrate => 320`).
    pub bitrate: Bitrate,
    /// Spotify client application icon code (`iconCode`).
    pub icon_code: IconCode,
    /// Track playback counter since the last audio cache purge.
    pub tracks_since_purge: u64,
    /// Default Home menu section IDs hidden across Spotty (`ignoreHomeItems`).
    pub ignore_home_items: HashSet<String>,
    /// Account switcher menu visibility (`accountSwitcherMenu => 0`).
    pub account_switcher: AccountSwitcherVisibility,
    /// Whether artist menus restrict to saved albums (`myAlbumsOnly`).
    pub album_scope: AlbumScope,
    /// Saved tracks sort order (`sortSongsAlphabetically => 1`).
    pub sort_songs: SortOrder,
    /// Saved albums sort order (`sortAlbumsAlphabetically => 1`).
    pub sort_albums: SortOrder,
    /// Followed artists sort order (`sortArtistsAlphabetically => 1`).
    pub sort_artists: SortOrder,
    /// Playlist track sort order (`sortPlaylisttracksByAddition => 0`).
    pub sort_playlist_tracks: SortOrder,
    /// Access point fallback port policy (`forceFallbackAP`).
    pub ap_fallback: ApFallbackPolicy,
    /// Account IDs excluded from `OnlineLibrary` scanning.
    pub dont_import_accounts: HashSet<String>,
    /// Optional explicit helper binary path (`helper => ""`).
    pub helper: String,
}

impl Default for GlobalPrefs {
    fn default() -> Self {
        let mut ignore_home_items = HashSet::new();
        ignore_home_items.insert(String::from("recently-updated-playlists[0]"));
        ignore_home_items.insert(String::from("recently-updated-playlists"));
        ignore_home_items.insert(String::from("recently-played"));

        Self {
            country: String::from("US"),
            cleanup_tags: TagCleanupPolicy::StripSuffixes,
            bitrate: Bitrate::Kbps320,
            icon_code: IconCode::default(),
            tracks_since_purge: 0,
            ignore_home_items,
            account_switcher: AccountSwitcherVisibility::Hidden,
            album_scope: AlbumScope::FullArtistCatalog,
            sort_songs: SortOrder::Alphabetical,
            sort_albums: SortOrder::Alphabetical,
            sort_artists: SortOrder::Alphabetical,
            sort_playlist_tracks: SortOrder::NaturalOrder,
            ap_fallback: ApFallbackPolicy::DefaultPort,
            dont_import_accounts: HashSet::new(),
            helper: String::new(),
        }
    }
}

impl GlobalPrefs {
    /// Applies `Plugin.pm` v1 migration adding `"spt-flc-*-*"` to `disabled`
    /// on non-x86, non-Windows, non-macOS hosts when no `"spt*"` entry exists.
    pub fn migrate_v1_disabled_formats(arch: &str, os: &str, disabled: &mut Vec<String>) {
        if is_windows_or_mac(os) || is_x86_arch(arch) {
            return;
        }
        if disabled.iter().any(|entry| entry.starts_with("spt")) {
            return;
        }
        disabled.push(String::from("spt-flc-*-*"));
    }

    /// Applies `Plugin.pm` v3 migration enabling `--ap-port=12321` fallback.
    pub fn migrate_v3_ap_fallback(&mut self) {
        self.ap_fallback = ApFallbackPolicy::ForcePort12321;
    }
}

/// Per-player Spotty preferences matching `Plugins::Spotty::Settings::Player`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlayerPrefs {
    /// Player `ReplayGain` policy (`replaygain`).
    pub replaygain: ReplayGainPolicy,
    /// Player explicit content filter mode (`filterExplicitContent`).
    pub explicit_filter: ExplicitFilterMode,
    /// Podcast episode ordering preference (`reversePodcastOrder`).
    pub podcast_order: PodcastEpisodeOrder,
    /// Home menu section IDs hidden for this player (`ignoreHomeItems`).
    pub ignore_home_items: HashSet<String>,
    /// Optional Spotify account ID bound to this player.
    pub account_id: Option<String>,
}

fn is_windows_or_mac(os: &str) -> bool {
    let lower = os.trim().to_ascii_lowercase();
    lower.contains("win") || lower.contains("mac") || lower.contains("darwin") || lower == "osx"
}

fn is_x86_arch(arch: &str) -> bool {
    let lower = arch.trim().to_ascii_lowercase();
    lower.contains("x86")
        || lower.contains("i386")
        || lower.contains("i486")
        || lower.contains("i586")
        || lower.contains("i686")
        || lower.contains("amd64")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::manifest::DEFAULT_ICON_HASH;

    #[test]
    fn bitrate_kbps_returns_expected_values() {
        assert_eq!(Bitrate::Kbps96.kbps(), 96);
        assert_eq!(Bitrate::Kbps160.kbps(), 160);
        assert_eq!(Bitrate::Kbps320.kbps(), 320);
        assert_eq!(Bitrate::default(), Bitrate::Kbps320);
    }

    #[test]
    fn global_prefs_default_matches_plugin_pm() {
        let prefs = GlobalPrefs::default();
        assert_eq!(prefs.country, "US");
        assert_eq!(prefs.cleanup_tags, TagCleanupPolicy::StripSuffixes);
        assert_eq!(prefs.bitrate, Bitrate::Kbps320);
        assert_eq!(prefs.icon_code.as_str(), DEFAULT_ICON_HASH);
        assert_eq!(prefs.tracks_since_purge, 0);
        assert_eq!(prefs.ignore_home_items.len(), 3);
        assert!(prefs.ignore_home_items.contains("recently-played"));
        assert!(prefs
            .ignore_home_items
            .contains("recently-updated-playlists"));
        assert!(prefs
            .ignore_home_items
            .contains("recently-updated-playlists[0]"));
        assert_eq!(prefs.account_switcher, AccountSwitcherVisibility::Hidden);
        assert_eq!(prefs.album_scope, AlbumScope::FullArtistCatalog);
        assert_eq!(prefs.sort_songs, SortOrder::Alphabetical);
        assert_eq!(prefs.sort_albums, SortOrder::Alphabetical);
        assert_eq!(prefs.sort_artists, SortOrder::Alphabetical);
        assert_eq!(prefs.sort_playlist_tracks, SortOrder::NaturalOrder);
        assert_eq!(prefs.ap_fallback, ApFallbackPolicy::DefaultPort);
        assert!(prefs.dont_import_accounts.is_empty());
        assert!(prefs.helper.is_empty());
    }

    #[test]
    fn player_prefs_default_matches_settings_player_pm() {
        let player = PlayerPrefs::default();
        assert_eq!(player.replaygain, ReplayGainPolicy::Disabled);
        assert_eq!(player.explicit_filter, ExplicitFilterMode::AllowAll);
        assert_eq!(player.podcast_order, PodcastEpisodeOrder::OldestFirst);
        assert!(player.ignore_home_items.is_empty());
        assert_eq!(player.account_id, None);
    }

    #[test]
    fn migrate_v1_disabled_formats_gates_on_non_x86_non_win_mac() {
        let mut arm_linux = Vec::new();
        GlobalPrefs::migrate_v1_disabled_formats("aarch64", "linux", &mut arm_linux);
        assert_eq!(arm_linux, vec![String::from("spt-flc-*-*")]);

        // Idempotent if an spt* entry is already present.
        GlobalPrefs::migrate_v1_disabled_formats("armv7l", "linux", &mut arm_linux);
        assert_eq!(arm_linux, vec![String::from("spt-flc-*-*")]);

        let mut custom_spt = vec![String::from("spt-mp3-*-*")];
        GlobalPrefs::migrate_v1_disabled_formats("armv7l", "linux", &mut custom_spt);
        assert_eq!(custom_spt, vec![String::from("spt-mp3-*-*")]);

        for (arch, os) in [
            ("x86_64", "linux"),
            ("i686", "linux"),
            ("aarch64", "darwin"),
            ("aarch64", "windows"),
            ("arm64", "macOS"),
        ] {
            let mut entries = Vec::new();
            GlobalPrefs::migrate_v1_disabled_formats(arch, os, &mut entries);
            assert!(
                entries.is_empty(),
                "expected no spt-flc disable for arch={arch} os={os}: {entries:?}"
            );
        }
    }

    #[test]
    fn migrate_v3_sets_force_port_12321() {
        let mut prefs = GlobalPrefs::default();
        assert_eq!(prefs.ap_fallback, ApFallbackPolicy::DefaultPort);
        prefs.migrate_v3_ap_fallback();
        assert_eq!(prefs.ap_fallback, ApFallbackPolicy::ForcePort12321);
    }

    proptest! {
        #[test]
        fn migrate_v1_is_idempotent_on_arm_linux(
            arch in prop_oneof![Just("aarch64"), Just("armv7l"), Just("armhf"), Just("riscv64")],
            os in prop_oneof![Just("linux"), Just("Linux"), Just("freebsd")],
        ) {
            let mut disabled = Vec::new();
            GlobalPrefs::migrate_v1_disabled_formats(arch, os, &mut disabled);
            GlobalPrefs::migrate_v1_disabled_formats(arch, os, &mut disabled);
            prop_assert_eq!(disabled, vec![String::from("spt-flc-*-*")]);
        }
    }
}
