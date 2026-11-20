//! Unified error types and UI localization error token mapping.

use std::io;

use thiserror::Error;

/// Unified error type returned across all `Spotty-RS` crates.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SpottyError {
    /// Spotify Web API returned HTTP 429 Too Many Requests.
    #[error("Spotify API rate limited on {endpoint}; retry after {retry_after_secs}s")]
    RateLimited {
        /// Seconds to wait before retrying, from `Retry-After`.
        retry_after_secs: u64,
        /// API endpoint path that triggered the rate limit.
        endpoint: String,
    },

    /// No Spotify OAuth access token is available for the requested account.
    #[error("no Spotify access token available")]
    NoAccessToken,

    /// The stored OAuth refresh token was rejected with `invalid_grant`.
    #[error("Spotify OAuth grant is invalid or revoked")]
    InvalidGrant,

    /// Input string could not be parsed as a valid Spotify URI or URL.
    #[error("invalid Spotify URI: {0}")]
    InvalidUri(String),

    /// HTTP transport or remote API error.
    #[error("HTTP transport error: {0}")]
    Http(String),

    /// Persistent `redb` or account store cache failure.
    #[error("cache error: {0}")]
    Cache(String),

    /// `SQLite` database read or write failure.
    #[error("database error: {0}")]
    Database(String),

    /// Audio streaming, Connect, or transcoding pipeline failure.
    #[error("audio pipeline error: {0}")]
    Audio(String),

    /// JSON payload serialization or deserialization failure.
    #[error("JSON error: {0}")]
    Json(String),

    /// Filesystem or process I/O failure.
    #[error("I/O error: {0}")]
    Io(String),
}

impl SpottyError {
    /// Maps an error variant to its LMS `strings.txt` localization key.
    #[must_use]
    pub fn ui_error_token(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "PLUGIN_SPOTTY_ERROR_429",
            Self::NoAccessToken => "PLUGIN_SPOTTY_MISSING_TOKEN",
            Self::InvalidGrant => "PLUGIN_SPOTTY_AUTH_FAILED",
            Self::InvalidUri(_) => "PLUGIN_SPOTTY_ERROR_INVALID_URI",
            Self::Http(_) => "PLUGIN_SPOTTY_ERROR_HTTP",
            Self::Cache(_) => "PLUGIN_SPOTTY_ERROR_CACHE",
            Self::Database(_) => "PLUGIN_SPOTTY_ERROR_DATABASE",
            Self::Audio(_) => "PLUGIN_SPOTTY_HELPER_ERROR",
            Self::Json(_) => "PLUGIN_SPOTTY_ERROR_JSON",
            Self::Io(_) => "PLUGIN_SPOTTY_ERROR_IO",
        }
    }
}

impl From<io::Error> for SpottyError {
    fn from(err: io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for SpottyError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err.to_string())
    }
}

/// Convenience `Result` alias parameterized by [`SpottyError`].
pub type Result<T> = std::result::Result<T, SpottyError>;

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_display_includes_endpoint_and_retry_after() {
        let err = SpottyError::RateLimited {
            retry_after_secs: 42,
            endpoint: String::from("/v1/me/tracks"),
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("42s") && rendered.contains("/v1/me/tracks"),
            "unexpected RateLimited display: {rendered}"
        );
    }

    #[test]
    fn ui_error_tokens_match_expected_keys() {
        let cases = [
            (
                SpottyError::RateLimited {
                    retry_after_secs: 5,
                    endpoint: String::from("/v1/search"),
                },
                "PLUGIN_SPOTTY_ERROR_429",
            ),
            (SpottyError::NoAccessToken, "PLUGIN_SPOTTY_MISSING_TOKEN"),
            (SpottyError::InvalidGrant, "PLUGIN_SPOTTY_AUTH_FAILED"),
            (
                SpottyError::InvalidUri(String::from("bad")),
                "PLUGIN_SPOTTY_ERROR_INVALID_URI",
            ),
            (
                SpottyError::Http(String::from("502")),
                "PLUGIN_SPOTTY_ERROR_HTTP",
            ),
            (
                SpottyError::Cache(String::from("corrupt")),
                "PLUGIN_SPOTTY_ERROR_CACHE",
            ),
            (
                SpottyError::Database(String::from("locked")),
                "PLUGIN_SPOTTY_ERROR_DATABASE",
            ),
            (
                SpottyError::Audio(String::from("pcm")),
                "PLUGIN_SPOTTY_HELPER_ERROR",
            ),
            (
                SpottyError::Json(String::from("eof")),
                "PLUGIN_SPOTTY_ERROR_JSON",
            ),
            (
                SpottyError::Io(String::from("enoent")),
                "PLUGIN_SPOTTY_ERROR_IO",
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.ui_error_token(), expected, "token mismatch for {err:?}");
        }
    }

    #[test]
    fn from_io_and_json_errors_preserve_message() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "missing file");
        let converted_io = SpottyError::from(io_err);
        assert_eq!(
            converted_io.ui_error_token(),
            "PLUGIN_SPOTTY_ERROR_IO",
            "unexpected token for converted io error: {converted_io:?}"
        );

        let json_err =
            serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON should fail");
        let converted_json = SpottyError::from(json_err);
        assert_eq!(
            converted_json.ui_error_token(),
            "PLUGIN_SPOTTY_ERROR_JSON",
            "unexpected token for converted json error: {converted_json:?}"
        );
    }
}
