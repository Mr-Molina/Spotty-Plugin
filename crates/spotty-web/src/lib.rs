//! Web UI settings handlers, OAuth PKCE relay, and Zeroconf supervision.

pub mod auth;
pub mod basic;
pub mod pkce_callback;
pub mod player;
pub mod player_auth;
pub mod playlist_folders_ui;

pub use crate::auth::{
    AuthCheckJson, AuthLandingController, AuthPageViewModel, DEFAULT_AUTH_REDIRECT_PATH,
};
pub use crate::basic::{
    BasicSettingsController, BasicSettingsForm, BasicSettingsSubmission, BasicSettingsViewModel,
    HelperDiagnostics, PlayerSummary, SettingsPostOutcome, SettingsSideEffect,
};
pub use crate::pkce_callback::{
    CallbackViewModel, PkceCallbackController, CALLBACK_SKIN_OVERRIDE, DEFAULT_PLUGIN_ID,
    LMS_COMMUNITY_CALLBACK_URL, LMS_COMMUNITY_PREPARE_URL, LMS_PLUGIN_ID_HEADER,
    PKCE_VERIFIER_CACHE_KEY, SPOTIFY_AUTHORIZE_URL,
};
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
