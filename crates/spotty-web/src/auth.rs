//! OAuth authentication landing page view-model and credential poller.

use serde::Serialize;
use spotty_cache::{AccountId, AccountStore, AUTHENTICATE_DIR};
use spotty_types::Result;

/// Default LMS redirect endpoint path for initiating PKCE OAuth.
pub const DEFAULT_AUTH_REDIRECT_PATH: &str = "/plugins/Spotty/settings/redirect";

/// JSON status payload returned by `plugins/Spotty/settings/hasCredentials`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AuthCheckJson {
    /// `true` when `<spotty_dir>/__AUTHENTICATE__/credentials.json` is valid.
    #[serde(rename = "hasCredentials")]
    pub has_credentials: bool,
}

impl AuthCheckJson {
    /// Creates a new [`AuthCheckJson`] payload.
    #[must_use]
    pub fn new(has_credentials: bool) -> Self {
        Self { has_credentials }
    }
}

/// Template view-model for `authentication.html`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthPageViewModel {
    /// Local LMS endpoint path that initiates the OAuth redirect flow.
    #[serde(rename = "authUrl")]
    pub auth_url: String,
    /// Relay callback URL registered with Spotify
    /// (`https://api.lms-community.org/auth/callback`).
    #[serde(rename = "callbackUrl")]
    pub callback_url: String,
    /// `true` when the LMS host OS string identifies a Docker container.
    #[serde(rename = "isDocker")]
    pub is_docker: bool,
}

impl AuthPageViewModel {
    /// Creates a new [`AuthPageViewModel`].
    #[must_use]
    pub fn new(
        auth_url: impl Into<String>,
        callback_url: impl Into<String>,
        is_docker: bool,
    ) -> Self {
        Self {
            auth_url: auth_url.into(),
            callback_url: callback_url.into(),
            is_docker,
        }
    }
}

/// Controller for `Plugins::Spotty::Settings::Auth` landing polling and
/// `__AUTHENTICATE__` folder promotion.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AuthLandingController;

impl AuthLandingController {
    /// Creates a new [`AuthLandingController`].
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Checks whether `<spotty_dir>/__AUTHENTICATE__/credentials.json` exists
    /// and contains a valid credential record.
    #[must_use]
    pub fn check_authenticate_status(accounts: &AccountStore) -> AuthCheckJson {
        let Ok(auth_id) = AccountId::parse(AUTHENTICATE_DIR) else {
            return AuthCheckJson::new(false);
        };
        let has_credentials = match accounts.credentials(&auth_id) {
            Ok(Some(record)) => !record.username.is_empty(),
            Ok(None) | Err(_) => false,
        };
        AuthCheckJson::new(has_credentials)
    }

    /// Promotes `<spotty_dir>/__AUTHENTICATE__` to its `<8-hex>` account
    /// folder and deletes any leftover `__AUTHENTICATE__` staging directory
    /// only when promotion actually succeeded.
    ///
    /// # Errors
    ///
    /// Returns [`spotty_types::SpottyError::Io`] or
    /// [`spotty_types::SpottyError::Cache`] if folder promotion or cleanup fails.
    pub fn cleanup_and_promote(accounts: &AccountStore) -> Result<Option<AccountId>> {
        let promoted = accounts.promote_authenticate_folder()?;
        if promoted.is_some() {
            let auth_id = AccountId::parse(AUTHENTICATE_DIR)?;
            accounts.delete_account(&auth_id)?;
        }
        Ok(promoted)
    }

    /// Builds the [`AuthPageViewModel`] for `authentication.html`.
    #[must_use]
    pub fn render_auth_page(
        auth_url: impl Into<String>,
        callback_url: impl Into<String>,
        os_name: &str,
    ) -> AuthPageViewModel {
        let is_docker = os_name.to_ascii_lowercase().contains("docker");
        AuthPageViewModel::new(auth_url, callback_url, is_docker)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn write_creds(path: &Path, body: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir should be created");
        }
        fs::write(path, body).expect("credentials file should be written");
    }

    #[test]
    fn check_authenticate_status_and_json_key_match_template() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());

        let initial = AuthLandingController::check_authenticate_status(&store);
        assert!(!initial.has_credentials);
        let serialized = serde_json::to_string(&initial).expect("json serialization succeeds");
        assert_eq!(serialized, r#"{"hasCredentials":false}"#);

        let auth_creds = store
            .spotty_dir()
            .join(AUTHENTICATE_DIR)
            .join("credentials.json");
        write_creds(&auth_creds, br#"{"username":"alice","auth_type":1}"#);

        let ready = AuthLandingController::check_authenticate_status(&store);
        assert!(ready.has_credentials);
        let ready_json = serde_json::to_string(&ready).expect("json serialization succeeds");
        assert_eq!(ready_json, r#"{"hasCredentials":true}"#);
    }

    #[test]
    fn cleanup_and_promote_moves_authenticate_to_eight_hex_dir() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());
        let expected_id = AccountId::from_username("alice");

        let auth_creds = store
            .spotty_dir()
            .join(AUTHENTICATE_DIR)
            .join("credentials.json");
        write_creds(
            &auth_creds,
            br#"{"username":"alice","auth_type":1,"auth_data":"tok"}"#,
        );

        let promoted = AuthLandingController::cleanup_and_promote(&store)
            .expect("cleanup_and_promote should succeed")
            .expect("valid __AUTHENTICATE__ should promote");
        assert_eq!(promoted, expected_id);
        assert!(
            store
                .account_dir(&expected_id)
                .join("credentials.json")
                .is_file(),
            "promoted <8-hex>/credentials.json must exist"
        );
        assert!(
            !store.spotty_dir().join(AUTHENTICATE_DIR).exists(),
            "__AUTHENTICATE__ directory must be removed"
        );
    }

    #[test]
    fn cleanup_and_promote_preserves_authenticate_when_no_credentials_exist() {
        let dir = TempDir::new().expect("temp dir should be created");
        let store = AccountStore::new(dir.path());
        let auth_dir = store.spotty_dir().join(AUTHENTICATE_DIR);
        fs::create_dir_all(&auth_dir).expect("auth staging dir should be created");
        fs::write(auth_dir.join("staging.tmp"), b"in-flight")
            .expect("staging marker should be written");

        let promoted = AuthLandingController::cleanup_and_promote(&store)
            .expect("cleanup_and_promote should succeed");
        assert!(promoted.is_none());
        assert!(
            auth_dir.exists(),
            "__AUTHENTICATE__ directory must not be deleted when promotion returns None"
        );
    }

    #[test]
    fn render_auth_page_detects_docker_and_serializes_keys() {
        let vm = AuthLandingController::render_auth_page(
            DEFAULT_AUTH_REDIRECT_PATH,
            "https://api.lms-community.org/auth/callback",
            "Linux Docker Container",
        );
        let json: serde_json::Value = serde_json::to_value(&vm).expect("json value");
        assert_eq!(json["authUrl"], DEFAULT_AUTH_REDIRECT_PATH);
        assert_eq!(
            json["callbackUrl"],
            "https://api.lms-community.org/auth/callback"
        );
        assert_eq!(json["isDocker"], true);
    }
}
