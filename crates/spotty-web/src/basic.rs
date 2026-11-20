//! Basic Spotty plugin settings controller and diagnostics view-model.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use spotty_api::RateLimitGate;
use spotty_cache::{AccountId, AccountStore};
use spotty_opml::RecentSearchesStore;
use spotty_playlist_folders::{FolderUploadManager, PurgeMode};
use spotty_types::{GlobalPrefs, IconCode, Result, TagCleanupPolicy};

use crate::auth::AuthLandingController;

const DEFAULT_HELPER_ERROR_LABEL: &str = "PLUGIN_SPOTTY_HELPER_ERROR";

/// Player entry displayed on `basic.html`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlayerSummary {
    /// Human-readable LMS player name.
    pub name: String,
    /// LMS player MAC/client identifier.
    pub id: String,
}

impl PlayerSummary {
    /// Creates a new [`PlayerSummary`].
    #[must_use]
    pub fn new(name: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id: id.into(),
        }
    }
}

/// Host helper and hardware diagnostics for [`BasicSettingsController`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperDiagnostics {
    /// Resolved path to the `spotty` helper binary or adapter, if any.
    pub helper_path: Option<String>,
    /// Semantic version string reported by the helper (without leading `'v'`).
    pub helper_version: Option<String>,
    /// Pre-formatted HTML/localized error message when no helper is available.
    pub helper_missing: Option<String>,
    /// Whether the helper supports `--ap-port` configuration (`!no-ap-port`).
    pub can_ap_port: bool,
    /// Whether the host runs on a low-power Raspberry Pi (`isLowCaloriesPi`).
    pub is_low_calories_pi: bool,
    /// Connected LMS players rendered on the basic settings screen.
    pub players: Vec<PlayerSummary>,
    /// Localized fallback label used when `helper_version` is absent.
    pub helper_error_label: String,
}

impl Default for HelperDiagnostics {
    fn default() -> Self {
        Self {
            helper_path: Some(String::from("spotty-rs")),
            helper_version: Some(String::from("4.62.3")),
            helper_missing: None,
            can_ap_port: true,
            is_low_calories_pi: false,
            players: Vec::new(),
            helper_error_label: String::from(DEFAULT_HELPER_ERROR_LABEL),
        }
    }
}

impl HelperDiagnostics {
    /// Creates a default [`HelperDiagnostics`] instance.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Template view-model matching `HTML/EN/plugins/Spotty/settings/basic.html`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BasicSettingsViewModel {
    /// Diagnostic message shown when the helper binary is missing.
    #[serde(rename = "helperMissing")]
    pub helper_missing: Option<String>,
    /// Sorted list of `{ "<username>": "<8-hex-id>" }` account maps.
    #[serde(rename = "credentials")]
    pub credentials: Vec<BTreeMap<String, String>>,
    /// Map of `<username>` to display name.
    #[serde(rename = "displayNames")]
    pub display_names: BTreeMap<String, String>,
    /// Map of `<username>` to Spotify subscription tier (`premium`, `free`, etc.).
    #[serde(rename = "products")]
    pub products: BTreeMap<String, String>,
    /// Non-empty rate-limit error banner when HTTP 429 cooldown is active.
    #[serde(rename = "error429")]
    pub error429: Option<String>,
    /// `true` on low-power ARM boards (`isLowCaloriesPi`).
    #[serde(rename = "isLowCaloriesPi")]
    pub is_low_calories_pi: bool,
    /// LMS players sorted case-insensitively by player name.
    #[serde(rename = "players")]
    pub players: Vec<PlayerSummary>,
    /// Path to the active helper binary, if resolved.
    #[serde(rename = "helperPath")]
    pub helper_path: Option<String>,
    /// Formatted helper version (`"v<ver>"`) or fallback error string.
    #[serde(rename = "helperVersion")]
    pub helper_version: String,
    /// `true` when `--ap-port` fallback configuration is supported.
    #[serde(rename = "canApPort")]
    pub can_ap_port: bool,
    /// `true` when [`GlobalPrefs::icon_code`] equals the default icon hash.
    #[serde(rename = "hasDefaultIcon")]
    pub has_default_icon: bool,
    /// `true` when `<spotty_dir>/credentials.json` exists for Zeroconf auth.
    #[serde(rename = "hasPlayerCredentials")]
    pub has_player_credentials: bool,
    /// Map of `<8-hex-id>` to `true` for accounts excluded from library import.
    #[serde(rename = "dontImportAccounts")]
    pub dont_import_accounts: BTreeMap<String, bool>,
    /// Settings validation warning with `iconCode` rewritten to `Client ID`.
    #[serde(rename = "warning")]
    pub warning: Option<String>,
}

/// Control-flow outcome returned by [`BasicSettingsController::handle_submit`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsPostOutcome {
    /// Render `basic.html` using the populated view-model.
    RenderBasic(Box<BasicSettingsViewModel>),
    /// HTTP 302 redirect to `authentication.html?ajaxUpdate=...`.
    RedirectAuth {
        /// Whether the LMS settings request originated from an AJAX update.
        ajax_update: bool,
    },
    /// HTTP 302 redirect to `player-auth.html?ajaxUpdate=...`.
    RedirectPlayerAuth {
        /// Whether the LMS settings request originated from an AJAX update.
        ajax_update: bool,
    },
}

impl SettingsPostOutcome {
    /// Returns the HTTP `Location` header value when the outcome is a redirect.
    #[must_use]
    pub fn redirect_location(&self) -> Option<String> {
        match self {
            Self::RenderBasic(_) => None,
            Self::RedirectAuth { ajax_update } => Some(format!(
                "authentication.html?ajaxUpdate={}",
                if *ajax_update { "1" } else { "" }
            )),
            Self::RedirectPlayerAuth { ajax_update } => Some(format!(
                "player-auth.html?ajaxUpdate={}",
                if *ajax_update { "1" } else { "" }
            )),
        }
    }
}

/// Side effect emitted when handling a `basic.html` form submission.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsSideEffect {
    /// `cleanupTags` changed, requiring an LMS `rescan onlinelibrary`.
    TriggerOnlineLibraryRescan,
    /// `iconCode` changed, wiping all accounts and clearing `RateLimitGate`.
    PurgedAccountsOnClientIdChange,
    /// `clearPlaylistFolderCache` cleared uploaded playlist folder files.
    ClearedPlaylistFolders,
    /// `clearSearchHistory` emptied [`RecentSearchesStore`].
    ClearedRecentSearches,
}

/// Result returned by [`BasicSettingsController::handle_submit`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicSettingsSubmission {
    /// Render or redirect outcome for the HTTP handler.
    pub outcome: SettingsPostOutcome,
    /// Side effects triggered during this request.
    pub side_effects: Vec<SettingsSideEffect>,
}

/// Parsed form submission parameters from `basic.html`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BasicSettingsForm {
    /// `saveSettings` submit flag.
    pub save_settings: bool,
    /// Submitted `pref_iconCode` value (`None` or empty falls back to default).
    pub icon_code: Option<String>,
    /// Submitted `pref_cleanupTags` toggle.
    pub cleanup_tags: Option<TagCleanupPolicy>,
    /// `clearPlaylistFolderCache` checkbox.
    pub clear_playlist_folder_cache: bool,
    /// `clearSearchHistory` checkbox.
    pub clear_search_history: bool,
    /// Per-account `pref_dontimport_<id>` toggles.
    pub dont_import_updates: BTreeMap<String, bool>,
    /// Account IDs targeted by `delete_<id>` submit buttons.
    pub delete_accounts: Vec<String>,
    /// `removePlayerAccount` submit button.
    pub remove_player_account: bool,
    /// `addAccount` submit button.
    pub add_account: bool,
    /// `addPlayerAccount` submit button.
    pub add_player_account: bool,
    /// `forceBasicSettings` bypass flag suppressing auth redirects.
    pub force_basic_settings: bool,
    /// `ajaxUpdate` flag propagated to redirect URLs.
    pub ajax_update: bool,
    /// Optional validation warning generated by the LMS settings framework.
    pub warning: Option<String>,
}

impl BasicSettingsForm {
    /// Parses a raw LMS query/form parameter map into a [`BasicSettingsForm`].
    #[must_use]
    pub fn from_params(params: &BTreeMap<String, String>) -> Self {
        let mut form = Self::default();

        for (key, value) in params {
            if let Some(account_id) = key.strip_prefix("delete_") {
                if !account_id.is_empty() && !value.trim().is_empty() {
                    form.delete_accounts.push(account_id.to_owned());
                }
                continue;
            }
            if let Some(account_id) = key.strip_prefix("pref_dontimport_") {
                if !account_id.is_empty() {
                    form.dont_import_updates
                        .insert(account_id.to_owned(), is_truthy(value));
                }
                continue;
            }
            match key.as_str() {
                "saveSettings" => form.save_settings = is_truthy(value),
                "pref_iconCode" => form.icon_code = Some(value.clone()),
                "pref_cleanupTags" => {
                    form.cleanup_tags = Some(if is_truthy(value) {
                        TagCleanupPolicy::StripSuffixes
                    } else {
                        TagCleanupPolicy::PreserveRaw
                    });
                }
                "clearPlaylistFolderCache" => {
                    form.clear_playlist_folder_cache = is_truthy(value);
                }
                "clearSearchHistory" => form.clear_search_history = is_truthy(value),
                "removePlayerAccount" => form.remove_player_account = is_truthy(value),
                "addAccount" => form.add_account = is_truthy(value),
                "addPlayerAccount" => form.add_player_account = is_truthy(value),
                "forceBasicSettings" => form.force_basic_settings = is_truthy(value),
                "ajaxUpdate" => form.ajax_update = is_truthy(value),
                "warning" if !value.is_empty() => {
                    form.warning = Some(value.clone());
                }
                _ => {}
            }
        }

        form
    }
}

/// Controller implementing `Plugins::Spotty::Settings` (`basic.html`).
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BasicSettingsController;

impl BasicSettingsController {
    /// Creates a new [`BasicSettingsController`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Processes a `basic.html` GET/POST request, mutating state and returning
    /// either a redirect decision or a populated [`BasicSettingsViewModel`].
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError`] if account deletion, credential
    /// promotion, folder purge, or `iconCode` validation fails.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_submit(
        form: &BasicSettingsForm,
        prefs: &mut GlobalPrefs,
        accounts: &AccountStore,
        rate_limit_gate: &RateLimitGate,
        recent_searches: &mut RecentSearchesStore,
        folder_upload_dir: &Path,
        now_epoch_secs: u64,
        display_names: &BTreeMap<String, String>,
        products: &BTreeMap<String, String>,
        diagnostics: &HelperDiagnostics,
    ) -> Result<BasicSettingsSubmission> {
        let mut parsed_deletes = Vec::with_capacity(form.delete_accounts.len());
        for raw_id in &form.delete_accounts {
            parsed_deletes.push(AccountId::parse(raw_id)?);
        }

        let _promoted = AuthLandingController::cleanup_and_promote(accounts)?;

        for account_id in &parsed_deletes {
            accounts.delete_account(account_id)?;
        }

        let mut side_effects = Vec::new();
        if form.save_settings {
            apply_save_settings(
                form,
                prefs,
                accounts,
                rate_limit_gate,
                recent_searches,
                folder_upload_dir,
                now_epoch_secs,
                &mut side_effects,
            )?;
        }

        if form.remove_player_account {
            accounts.delete_player_credentials()?;
        }

        let all_creds = accounts.all_credentials()?;
        let has_credentials = !all_creds.is_empty();
        let helper_missing =
            diagnostics.helper_path.is_none() || diagnostics.helper_missing.is_some();

        if !form.force_basic_settings {
            if !helper_missing && (form.add_account || !has_credentials) {
                return Ok(BasicSettingsSubmission {
                    outcome: SettingsPostOutcome::RedirectAuth {
                        ajax_update: form.ajax_update,
                    },
                    side_effects,
                });
            }
            if (!helper_missing && form.add_player_account)
                || (has_credentials && !accounts.has_player_credentials())
            {
                return Ok(BasicSettingsSubmission {
                    outcome: SettingsPostOutcome::RedirectPlayerAuth {
                        ajax_update: form.ajax_update,
                    },
                    side_effects,
                });
            }
        }

        let view_model = build_view_model(
            &all_creds,
            prefs,
            accounts,
            rate_limit_gate,
            display_names,
            products,
            diagnostics,
            form.warning.as_deref(),
        );

        Ok(BasicSettingsSubmission {
            outcome: SettingsPostOutcome::RenderBasic(Box::new(view_model)),
            side_effects,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_save_settings(
    form: &BasicSettingsForm,
    prefs: &mut GlobalPrefs,
    accounts: &AccountStore,
    rate_limit_gate: &RateLimitGate,
    recent_searches: &mut RecentSearchesStore,
    folder_upload_dir: &Path,
    now_epoch_secs: u64,
    side_effects: &mut Vec<SettingsSideEffect>,
) -> Result<()> {
    // Validate all inputs before executing any destructive side effects.
    let next_icon = match form.icon_code.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => IconCode::new(raw)?,
        _ => IconCode::default(),
    };
    let mut validated_dont_import = Vec::with_capacity(form.dont_import_updates.len());
    for (raw_id, dont_import) in &form.dont_import_updates {
        let account_id = AccountId::parse(raw_id)?;
        validated_dont_import.push((account_id, *dont_import));
    }

    if form.clear_playlist_folder_cache {
        let _removed = FolderUploadManager::purge_folder_cache(
            folder_upload_dir,
            PurgeMode::DeleteAll,
            now_epoch_secs,
        )?;
        side_effects.push(SettingsSideEffect::ClearedPlaylistFolders);
    }

    if next_icon != prefs.icon_code {
        prefs.icon_code = next_icon;
        let existing = accounts.all_credentials()?;
        for (_username, account_id) in existing {
            accounts.delete_account(&account_id)?;
        }
        rate_limit_gate.clear();
        side_effects.push(SettingsSideEffect::PurgedAccountsOnClientIdChange);
    }

    if form.clear_search_history {
        recent_searches.clear()?;
        side_effects.push(SettingsSideEffect::ClearedRecentSearches);
    }

    for (account_id, dont_import) in validated_dont_import {
        let canonical = account_id.as_str().to_owned();
        if dont_import {
            prefs.dont_import_accounts.insert(canonical);
        } else {
            prefs.dont_import_accounts.remove(&canonical);
        }
    }

    let next_cleanup = form.cleanup_tags.unwrap_or(TagCleanupPolicy::PreserveRaw);
    if next_cleanup != prefs.cleanup_tags {
        prefs.cleanup_tags = next_cleanup;
        side_effects.push(SettingsSideEffect::TriggerOnlineLibraryRescan);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_view_model(
    all_creds: &[(String, AccountId)],
    prefs: &GlobalPrefs,
    accounts: &AccountStore,
    rate_limit_gate: &RateLimitGate,
    display_names_map: &BTreeMap<String, String>,
    products_map: &BTreeMap<String, String>,
    diagnostics: &HelperDiagnostics,
    raw_warning: Option<&str>,
) -> BasicSettingsViewModel {
    let mut sorted_creds = all_creds.to_vec();
    sorted_creds.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));

    let mut credentials = Vec::with_capacity(sorted_creds.len());
    let mut display_names = BTreeMap::new();
    let mut products = BTreeMap::new();

    for (username, account_id) in &sorted_creds {
        let mut tuple = BTreeMap::new();
        tuple.insert(username.clone(), account_id.as_str().to_owned());
        credentials.push(tuple);

        let display_name = display_names_map
            .get(username)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| username.clone());
        display_names.insert(username.clone(), display_name);

        if let Some(product) = products_map.get(username) {
            products.insert(username.clone(), product.clone());
        }
    }

    let mut players = diagnostics.players.clone();
    players.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });

    let helper_version = match diagnostics.helper_version.as_deref() {
        Some(ver) if !ver.trim().is_empty() => format!("v{}", ver.trim()),
        _ => diagnostics.helper_error_label.clone(),
    };

    let error429 = rate_limit_gate
        .is_tripped()
        .map(|secs| format!("Spotify rate limit exceeded (retry in {secs}s)"));

    let mut dont_import_accounts = BTreeMap::new();
    for id in &prefs.dont_import_accounts {
        dont_import_accounts.insert(id.clone(), true);
    }

    let warning = raw_warning.map(rewrite_icon_code_warning);

    BasicSettingsViewModel {
        helper_missing: diagnostics.helper_missing.clone(),
        credentials,
        display_names,
        products,
        error429,
        is_low_calories_pi: diagnostics.is_low_calories_pi,
        players,
        helper_path: diagnostics.helper_path.clone(),
        helper_version,
        can_ap_port: diagnostics.can_ap_port,
        has_default_icon: prefs.icon_code.is_default_icon(),
        has_player_credentials: accounts.has_player_credentials(),
        dont_import_accounts,
        warning,
    }
}

fn rewrite_icon_code_warning(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let needle = "iconcode";
    let rewritten = if let Some(pos) = lower.find(needle) {
        let mut out = String::with_capacity(input.len() + 2);
        out.push_str(&input[..pos]);
        out.push_str("Client ID");
        out.push_str(&input[pos + needle.len()..]);
        out
    } else {
        input.to_owned()
    };
    crate::pkce_callback::escape_html(&rewritten)
}

fn is_truthy(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn seed_account(store: &AccountStore, username: &str) -> AccountId {
        let id = AccountId::from_username(username);
        let path = store.account_dir(&id).join("credentials.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("account dir should be created");
        }
        let body = format!(r#"{{"username":"{username}","auth_type":1}}"#);
        fs::write(&path, body.as_bytes()).expect("credentials.json should be written");
        id
    }

    fn seed_player_creds(store: &AccountStore) {
        let path = store.spotty_dir().join("credentials.json");
        fs::create_dir_all(store.spotty_dir()).expect("spotty dir should be created");
        fs::write(&path, br#"{"username":"player","auth_type":1}"#)
            .expect("player credentials.json should be written");
    }

    #[test]
    fn missing_credentials_redirects_to_authentication_html() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let gate = RateLimitGate::new();
        let mut prefs = GlobalPrefs::default();
        let mut searches = RecentSearchesStore::new();
        let form = BasicSettingsForm {
            ajax_update: true,
            ..BasicSettingsForm::default()
        };

        let result = BasicSettingsController::handle_submit(
            &form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            dir.path(),
            1_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        )
        .expect("handle_submit should succeed");

        assert_eq!(
            result.outcome,
            SettingsPostOutcome::RedirectAuth { ajax_update: true }
        );
        assert_eq!(
            result.outcome.redirect_location().as_deref(),
            Some("authentication.html?ajaxUpdate=1")
        );
    }

    #[test]
    fn missing_player_credentials_or_add_player_redirects_to_player_auth() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let _id = seed_account(&store, "alice");
        let gate = RateLimitGate::new();
        let mut prefs = GlobalPrefs::default();
        let mut searches = RecentSearchesStore::new();
        let form = BasicSettingsForm::default();

        let result = BasicSettingsController::handle_submit(
            &form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            dir.path(),
            1_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        )
        .expect("handle_submit should succeed");

        assert_eq!(
            result.outcome,
            SettingsPostOutcome::RedirectPlayerAuth { ajax_update: false }
        );
        assert_eq!(
            result.outcome.redirect_location().as_deref(),
            Some("player-auth.html?ajaxUpdate=")
        );
    }

    #[test]
    fn changing_icon_code_wipes_accounts_and_clears_rate_limit_gate() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let alice_id = seed_account(&store, "alice");
        seed_player_creds(&store);
        let gate = RateLimitGate::new();
        gate.trip(60);
        assert!(gate.is_tripped().is_some());

        let mut prefs = GlobalPrefs::default();
        let mut searches = RecentSearchesStore::new();
        let mut params = BTreeMap::new();
        params.insert(String::from("saveSettings"), String::from("1"));
        params.insert(
            String::from("pref_iconCode"),
            String::from("0123456789abcdef0123456789abcdef"),
        );
        params.insert(String::from("pref_cleanupTags"), String::from("1"));
        params.insert(String::from("forceBasicSettings"), String::from("1"));
        let form = BasicSettingsForm::from_params(&params);

        let result = BasicSettingsController::handle_submit(
            &form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            dir.path(),
            1_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        )
        .expect("handle_submit should succeed");

        assert!(
            result
                .side_effects
                .contains(&SettingsSideEffect::PurgedAccountsOnClientIdChange),
            "expected PurgedAccountsOnClientIdChange side effect, got {:?}",
            result.side_effects
        );
        assert!(
            gate.is_tripped().is_none(),
            "RateLimitGate must be cleared on iconCode change"
        );
        assert!(
            !store
                .account_dir(&alice_id)
                .join("credentials.json")
                .exists(),
            "existing account credentials must be wiped on iconCode change"
        );
    }

    #[test]
    fn toggling_cleanup_tags_and_clearing_caches_emits_side_effects() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let alice_id = seed_account(&store, "alice");
        seed_player_creds(&store);
        let gate = RateLimitGate::new();

        let upload_dir = dir.path().join("playlistFolders");
        fs::create_dir_all(&upload_dir).expect("upload dir");
        fs::write(upload_dir.join("desktop.file"), b"start-group").expect("upload file");

        let mut prefs = GlobalPrefs::default();
        let mut searches = RecentSearchesStore::from_items(vec![String::from("miles davis")]);

        let mut params = BTreeMap::new();
        params.insert(String::from("saveSettings"), String::from("1"));
        params.insert(String::from("clearPlaylistFolderCache"), String::from("1"));
        params.insert(String::from("clearSearchHistory"), String::from("1"));
        params.insert(
            format!("pref_dontimport_{}", alice_id.as_str()),
            String::from("1"),
        );
        // Omitting `pref_cleanupTags` with `saveSettings=1` toggles
        // `StripSuffixes` -> `PreserveRaw`.
        let form = BasicSettingsForm::from_params(&params);

        let result = BasicSettingsController::handle_submit(
            &form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            &upload_dir,
            2_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        )
        .expect("handle_submit should succeed");

        assert_eq!(prefs.cleanup_tags, TagCleanupPolicy::PreserveRaw);
        assert!(prefs.dont_import_accounts.contains(alice_id.as_str()));
        assert!(searches.is_empty());
        assert!(!upload_dir.join("desktop.file").exists());
        assert!(result
            .side_effects
            .contains(&SettingsSideEffect::TriggerOnlineLibraryRescan));
        assert!(result
            .side_effects
            .contains(&SettingsSideEffect::ClearedPlaylistFolders));
        assert!(result
            .side_effects
            .contains(&SettingsSideEffect::ClearedRecentSearches));
    }

    #[test]
    fn delete_account_and_view_model_json_keys_match_template_toolkit() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let alice_id = seed_account(&store, "alice");
        let bob_id = seed_account(&store, "bob");
        seed_player_creds(&store);
        let gate = RateLimitGate::new();

        let mut prefs = GlobalPrefs::default();
        prefs
            .dont_import_accounts
            .insert(alice_id.as_str().to_owned());
        let mut searches = RecentSearchesStore::new();

        let mut params = BTreeMap::new();
        params.insert(
            format!("delete_{}", bob_id.as_str()),
            String::from("Delete"),
        );
        params.insert(
            String::from("warning"),
            String::from("Invalid iconCode provided"),
        );
        let form = BasicSettingsForm::from_params(&params);

        let mut display_names = BTreeMap::new();
        display_names.insert(String::from("alice"), String::from("Alice Example"));
        let mut products = BTreeMap::new();
        products.insert(String::from("alice"), String::from("premium"));

        let diagnostics = HelperDiagnostics {
            players: vec![
                PlayerSummary::new("Kitchen", "00:04:20:00:00:02"),
                PlayerSummary::new("bedroom", "00:04:20:00:00:01"),
            ],
            ..HelperDiagnostics::default()
        };

        let result = BasicSettingsController::handle_submit(
            &form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            dir.path(),
            1_000,
            &display_names,
            &products,
            &diagnostics,
        )
        .expect("handle_submit should succeed");

        assert!(
            !store.account_dir(&bob_id).join("credentials.json").exists(),
            "bob should have been deleted via delete_<id>"
        );

        let SettingsPostOutcome::RenderBasic(vm) = result.outcome else {
            panic!("expected RenderBasic outcome, got {:?}", result.outcome);
        };
        assert_eq!(vm.warning.as_deref(), Some("Invalid Client ID provided"));
        assert_eq!(vm.helper_version, "v4.62.3");
        assert_eq!(vm.players[0].name, "bedroom");
        assert_eq!(vm.players[1].name, "Kitchen");

        let json_val = serde_json::to_value(&*vm).expect("serialize view model");
        let obj = json_val.as_object().expect("must serialize as JSON object");
        let actual_keys: HashSet<&str> = obj.keys().map(String::as_str).collect();
        let expected_keys: HashSet<&str> = [
            "helperMissing",
            "credentials",
            "displayNames",
            "products",
            "error429",
            "isLowCaloriesPi",
            "players",
            "helperPath",
            "helperVersion",
            "canApPort",
            "hasDefaultIcon",
            "hasPlayerCredentials",
            "dontImportAccounts",
            "warning",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            actual_keys, expected_keys,
            "JSON keys must match basic.html Template Toolkit names byte-for-byte"
        );
    }

    #[test]
    fn validation_failure_prevents_folder_purge_and_escapes_warning_html() {
        let dir = TempDir::new().expect("temp dir");
        let store = AccountStore::new(dir.path());
        let _alice = seed_account(&store, "alice");
        seed_player_creds(&store);
        let upload_dir = dir.path().join("playlistFolders");
        fs::create_dir_all(&upload_dir).expect("upload dir");
        let marker_file = upload_dir.join("keep.file");
        fs::write(&marker_file, b"start-group").expect("upload file");

        let gate = RateLimitGate::new();
        let mut prefs = GlobalPrefs::default();
        let mut searches = RecentSearchesStore::new();

        let mut bad_params = BTreeMap::new();
        bad_params.insert(String::from("saveSettings"), String::from("1"));
        bad_params.insert(String::from("clearPlaylistFolderCache"), String::from("1"));
        bad_params.insert(String::from("pref_dontimport_../bad"), String::from("1"));
        let bad_form = BasicSettingsForm::from_params(&bad_params);
        let err = BasicSettingsController::handle_submit(
            &bad_form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            &upload_dir,
            1_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        );
        assert!(err.is_err(), "invalid AccountId must be rejected");
        assert!(
            marker_file.exists(),
            "playlist folder cache must not be purged when validation fails"
        );

        let mut warn_params = BTreeMap::new();
        warn_params.insert(String::from("forceBasicSettings"), String::from("1"));
        warn_params.insert(
            String::from("warning"),
            String::from("Bad iconCode <img src=x onerror=alert(1)>"),
        );
        let warn_form = BasicSettingsForm::from_params(&warn_params);
        let ok = BasicSettingsController::handle_submit(
            &warn_form,
            &mut prefs,
            &store,
            &gate,
            &mut searches,
            &upload_dir,
            1_000,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &HelperDiagnostics::default(),
        )
        .expect("render with warning");
        let SettingsPostOutcome::RenderBasic(vm) = ok.outcome else {
            panic!("expected RenderBasic");
        };
        let warning = vm.warning.expect("warning present");
        assert!(warning.contains("Client ID"));
        assert!(!warning.contains('<'));
        assert!(warning.contains("&lt;img"));
    }
}
