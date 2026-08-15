//! Error types for Drive operations.

use std::fmt;

use thiserror::Error;

/// Which OAuth2 grant produced Google's `invalid_grant` response.
///
/// Google's response body is identical for both causes, so the
/// distinguishing message is chosen from *which call* got the error, not
/// from anything in the response body itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantContext {
    /// The initial `authorization_code` → token exchange.
    CodeExchange,
    /// A `refresh_token` → access-token renewal.
    Refresh,
}

impl fmt::Display for GrantContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CodeExchange => write!(
                f,
                "the authorization code was invalid, already used, expired (codes are \
                 single-use and valid only a few minutes), or the PKCE code_verifier did not \
                 match the code_challenge sent at the start of login. Run \
                 `omni-dev drive auth login` again."
            ),
            Self::Refresh => write!(
                f,
                "this almost always means either (1) your Drive OAuth client is in \"Testing\" \
                 publishing status, where refresh tokens expire after 7 days — publish it to \
                 \"In production\" in Google Cloud Console to avoid this, or (2) access was \
                 revoked. Run `omni-dev drive auth login` again to re-authenticate."
            ),
        }
    }
}

/// Errors that can occur during Drive operations.
#[derive(Error, Debug)]
pub enum DriveError {
    /// Drive credentials are not configured.
    #[error("Drive credentials not configured. Run `omni-dev drive auth login`")]
    CredentialsNotFound,

    /// A Drive API request failed.
    #[error("Drive API request failed: HTTP {status}: {body}")]
    ApiRequestFailed {
        /// HTTP status code.
        status: u16,
        /// Response body text (or an extracted `error.message`/`reason`
        /// summary when the body is Drive's JSON error envelope).
        body: String,
    },

    /// The OAuth callback's `state` did not match the value generated at the
    /// start of login.
    #[error(
        "OAuth state mismatch: the browser callback did not present the expected state \
         value; aborting login for safety"
    )]
    StateMismatch,

    /// Google's `?error=` redirect (e.g. the user clicked "Cancel").
    #[error("Google denied the authorization request: {0}")]
    AuthorizationDenied(String),

    /// No browser callback arrived within the timeout.
    #[error(
        "Timed out after {0}s waiting for the browser sign-in callback; re-run \
         `omni-dev drive auth login`"
    )]
    CallbackTimeout(u64),

    /// The browser's callback request could not be parsed.
    #[error(
        "The browser's sign-in callback was malformed or missing the `code`/`state` parameters"
    )]
    MalformedCallback,

    /// Google rejected a code exchange or refresh with `invalid_grant`.
    #[error("Google rejected the request (invalid_grant): {0}")]
    InvalidGrant(GrantContext),

    /// Google's token response was missing a required field.
    #[error("Drive OAuth token response was malformed: missing `{0}`")]
    MalformedTokenResponse(&'static str),

    /// Google's token response carried no `drive.readonly` scope at all —
    /// e.g. the Drive permission was left unticked on the consent screen.
    #[error(
        "Google did not grant the drive.readonly scope (received: {0}).\n  On the consent \
         screen, tick the Drive permission — restricted scopes are not granted by default. \
         Re-run `omni-dev drive auth login`."
    )]
    NoScopeGranted(String),

    /// The configured browser launch command was invalid.
    #[error("Invalid browser launch command: {0}")]
    InvalidBrowserCommand(String),
}

impl DriveError {
    /// Builds an [`AuthorizationDenied`](Self::AuthorizationDenied) from
    /// Google's `error`/`error_description` redirect parameters.
    ///
    /// Kept as a constructor (not a conditional format string) so the
    /// "does an optional description exist" branching lives in one place
    /// instead of inside the `#[error(...)]` macro, matching this
    /// codebase's convention of plain field interpolation in
    /// `#[error(...)]` attributes.
    #[must_use]
    pub(crate) fn authorization_denied(reason: &str, description: Option<&str>) -> Self {
        let detail = match description {
            Some(d) if !d.is_empty() => format!("{reason} ({d})"),
            _ => reason.to_string(),
        };
        Self::AuthorizationDenied(detail)
    }

    /// The Drive-specific `reason` embedded in an
    /// [`ApiRequestFailed`](Self::ApiRequestFailed)'s body, if
    /// `DriveClient::response_to_error` found one.
    ///
    /// That constructor already discards the raw JSON error envelope in
    /// favour of a human-readable `"{message} (reason: {reason})"` string
    /// (`client.rs`'s `extract_drive_error_message`), so this parses that
    /// fixed suffix convention rather than re-parsing JSON that isn't kept
    /// around by the time this variant exists.
    ///
    /// Unused for now: Gmail's twin (`GmailError::reason`) is used by
    /// `gmail sync`'s reconciliation engine, which Drive has no equivalent
    /// of (explicitly out of scope) — reserved for a future error-reason
    /// sensitive consumer. Narrow allow here rather than a crate-wide one
    /// (removed in #1524).
    #[allow(dead_code)]
    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Self::ApiRequestFailed { body, .. } => body
                .rsplit_once("(reason: ")
                .and_then(|(_, rest)| rest.strip_suffix(')')),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn credentials_not_found_display() {
        let err = DriveError::CredentialsNotFound;
        assert!(err.to_string().contains("not configured"));
        assert!(err.to_string().contains("drive auth login"));
    }

    #[test]
    fn api_request_failed_display() {
        let err = DriveError::ApiRequestFailed {
            status: 403,
            body: "insufficientPermissions".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("403"));
        assert!(msg.contains("insufficientPermissions"));
    }

    #[test]
    fn state_mismatch_display_mentions_state() {
        assert!(DriveError::StateMismatch.to_string().contains("state"));
    }

    #[test]
    fn reason_extracts_the_formatted_suffix() {
        let err = DriveError::ApiRequestFailed {
            status: 404,
            body: "Not Found (reason: notFound)".to_string(),
        };
        assert_eq!(err.reason(), Some("notFound"));
    }

    #[test]
    fn reason_is_none_when_body_has_no_reason_suffix() {
        let err = DriveError::ApiRequestFailed {
            status: 500,
            body: "Internal Server Error".to_string(),
        };
        assert_eq!(err.reason(), None);
    }

    #[test]
    fn reason_is_none_for_non_api_request_failed_variants() {
        assert_eq!(DriveError::StateMismatch.reason(), None);
    }

    #[test]
    fn authorization_denied_includes_description_when_present() {
        let err = DriveError::authorization_denied("access_denied", Some("user declined"));
        let msg = err.to_string();
        assert!(msg.contains("access_denied"));
        assert!(msg.contains("user declined"));
        assert!(msg.contains('('));
    }

    #[test]
    fn authorization_denied_omits_parens_when_absent() {
        let err = DriveError::authorization_denied("access_denied", None);
        let msg = err.to_string();
        assert!(msg.contains("access_denied"));
        assert!(!msg.contains('('));
    }

    #[test]
    fn authorization_denied_omits_parens_when_description_empty() {
        let err = DriveError::authorization_denied("access_denied", Some(""));
        let msg = err.to_string();
        assert!(!msg.contains('('));
    }

    #[test]
    fn callback_timeout_display_includes_seconds() {
        let err = DriveError::CallbackTimeout(120);
        assert!(err.to_string().contains("120"));
    }

    #[test]
    fn malformed_callback_display() {
        assert!(DriveError::MalformedCallback
            .to_string()
            .to_lowercase()
            .contains("malformed"));
    }

    #[test]
    fn invalid_grant_code_exchange_display_mentions_pkce_and_expired_code() {
        let err = DriveError::InvalidGrant(GrantContext::CodeExchange);
        let msg = err.to_string();
        assert!(msg.contains("PKCE"));
        assert!(msg.contains("expired"));
        assert!(msg.contains("auth login"));
    }

    #[test]
    fn invalid_grant_refresh_display_mentions_7_days_and_testing_mode() {
        let err = DriveError::InvalidGrant(GrantContext::Refresh);
        let msg = err.to_string();
        assert!(msg.contains("7 days"));
        assert!(msg.contains("Testing"));
        assert!(msg.contains("auth login"));
    }

    #[test]
    fn grant_context_variants_produce_distinct_messages() {
        let code = GrantContext::CodeExchange.to_string();
        let refresh = GrantContext::Refresh.to_string();
        assert_ne!(code, refresh);
    }

    #[test]
    fn malformed_token_response_names_the_missing_field() {
        let err = DriveError::MalformedTokenResponse("refresh_token");
        assert!(err.to_string().contains("refresh_token"));
    }

    #[test]
    fn invalid_browser_command_display_includes_detail() {
        let err = DriveError::InvalidBrowserCommand("empty command".to_string());
        assert!(err.to_string().contains("empty command"));
    }

    #[test]
    fn no_scope_granted_display_names_the_received_scopes() {
        let err = DriveError::NoScopeGranted("openid, email, profile".to_string());
        let msg = err.to_string();
        assert!(msg.contains("openid, email, profile"));
        assert!(msg.contains("consent screen"));
        assert!(msg.contains("auth login"));
    }
}
