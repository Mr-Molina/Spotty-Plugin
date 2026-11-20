//! Web UI settings handlers, OAuth PKCE relay, and Zeroconf supervision.

pub mod player;
pub mod player_auth;
pub mod playlist_folders_ui;

pub use crate::player::{
    apply_home_item_toggles, build_home_menu_items, HomeMenuItemOption, PlayerSettingsController,
    PlayerSettingsViewModel,
};
pub use crate::player_auth::{
    check_player_credentials, PlayerAuthCheckJson, PlayerAuthPageOutcome, PlayerAuthSupervisor,
};
pub use crate::playlist_folders_ui::{
    CachedFolderFileView, PlaylistFoldersSettingsController, PlaylistFoldersViewModel,
};
