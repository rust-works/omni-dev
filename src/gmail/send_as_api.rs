//! Gmail send-as settings wrapper.
//!
//! `users.settings.sendAs.list` returns the account's primary address and
//! every send-as alias in one unpaginated call (1 quota unit, allowed by
//! `gmail.readonly`). `gmail draft create --reply-all` uses it to leave the
//! account's own addresses out of a reply (#1954), and `gmail draft
//! create`/`update --from` to check that a `From` is one Gmail will send as
//! ([`resolve_from`], #1956). Read-only: nothing here creates, edits or
//! verifies an alias.

use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

use crate::gmail::client::GmailClient;
use crate::gmail::compose::Mailbox;

/// The `verificationStatus` of an alias still awaiting verification, which
/// Gmail won't send as.
const PENDING: &str = "pending";

/// One address the account can send as.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SendAs {
    /// The address that appears in `From`.
    #[serde(rename = "sendAsEmail")]
    pub send_as_email: String,
    /// The name Gmail puts beside the address in `From`. Empty or absent
    /// means Gmail uses the primary address's name (for the primary
    /// address itself, the Google account's name).
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    /// Whether this is the account's primary address.
    #[serde(rename = "isPrimary", default)]
    pub is_primary: bool,
    /// `accepted` or `pending` for a custom alias; absent for the primary
    /// address, which needs no verification.
    #[serde(rename = "verificationStatus", default)]
    pub verification_status: Option<String>,
}

impl SendAs {
    /// Whether Gmail will send as this address: anything but an alias
    /// still awaiting verification.
    fn is_usable(&self) -> bool {
        self.verification_status.as_deref() != Some(PENDING)
    }

    /// The non-blank `displayName`, if any.
    fn name(&self) -> Option<&str> {
        self.display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
    }
}

/// Checks `from` against the account's send-as addresses and returns the
/// mailbox to write as the draft's `From`, or `None` to leave `From` to
/// Gmail.
///
/// The address matches `sendAsEmail` case-insensitively and is written in
/// the stored spelling. An alias still awaiting verification is refused,
/// as is an address that isn't one of `send_as`: Gmail would send from the
/// primary address instead, or not at all.
///
/// Without a name in `from`, the name is the alias's non-blank
/// `displayName`, else the primary address's, which is the name Gmail
/// itself sends a nameless alias under. When that leaves the primary address
/// with no name, the result is `None`: Gmail fills in the primary address
/// and the Google account's name, which a nameless `From` would lose.
pub fn resolve_from(send_as: &[SendAs], from: &Mailbox) -> Result<Option<Mailbox>> {
    let Some(alias) = send_as
        .iter()
        .find(|alias| alias.send_as_email.eq_ignore_ascii_case(&from.email))
    else {
        let usable: Vec<&str> = send_as
            .iter()
            .filter(|alias| alias.is_usable())
            .map(|alias| alias.send_as_email.as_str())
            .collect();
        bail!(
            "{} is not one of this account's send-as addresses (usable: {}); add it under \
             Gmail's Settings > Accounts > Send mail as, or use one of those",
            from.email,
            if usable.is_empty() {
                "none listed".to_string()
            } else {
                usable.join(", ")
            }
        );
    };
    ensure!(
        alias.is_usable(),
        "send-as address {} is awaiting verification; confirm it from the verification email \
         Gmail sent, then retry",
        alias.send_as_email
    );
    if let Some(name) = from.name.as_deref() {
        return Mailbox::from_parts(Some(name), &alias.send_as_email).map(Some);
    }
    let primary_name = send_as
        .iter()
        .find(|alias| alias.is_primary)
        .and_then(SendAs::name);
    match alias.name().or(primary_name) {
        Some(name) => Mailbox::from_parts(Some(name), &alias.send_as_email)
            .with_context(|| {
                format!(
                    "Gmail's display name for {} can't be used in a header; pass the name \
                     yourself, as --from 'Name <{}>'",
                    alias.send_as_email, alias.send_as_email
                )
            })
            .map(Some),
        None if alias.is_primary => Ok(None),
        None => Mailbox::from_parts(None, &alias.send_as_email).map(Some),
    }
}

/// The `users.settings.sendAs.list` response body.
#[derive(Debug, Deserialize)]
struct SendAsList {
    #[serde(rename = "sendAs", default)]
    send_as: Vec<SendAs>,
}

/// Send-as settings API façade.
#[derive(Debug)]
pub struct SendAsApi<'a> {
    client: &'a GmailClient,
}

impl<'a> SendAsApi<'a> {
    /// Wraps an existing [`GmailClient`] for send-as settings.
    #[must_use]
    pub fn new(client: &'a GmailClient) -> Self {
        Self { client }
    }

    /// Lists the primary address and every send-as alias.
    pub async fn list(&self) -> Result<Vec<SendAs>> {
        let url =
            GmailClient::api_url(self.client.base_url(), "/gmail/v1/users/me/settings/sendAs")?;
        let list: SendAsList = self
            .client
            .get_parsed(
                url.as_str(),
                "Failed to parse users.settings.sendAs.list response",
            )
            .await?;
        Ok(list.send_as)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::gmail::auth::{GmailCredentials, GmailScope};
    use crate::utils::secret::Secret;

    const SEND_AS_PATH: &str = "/gmail/v1/users/me/settings/sendAs";

    fn test_credentials() -> GmailCredentials {
        GmailCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: GmailScope::ReadOnly,
        }
    }

    async fn client_with_bootstrapped_token(server: &wiremock::MockServer) -> GmailClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut client = GmailClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::gmail::client::test_support::replace_session(
            &mut client,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        client
    }

    #[tokio::test]
    async fn list_parses_the_primary_address_and_aliases() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "sendAs": [
                        {
                            "sendAsEmail": "me@example.com",
                            "displayName": "Me",
                            "isPrimary": true,
                            "isDefault": true,
                        },
                        {
                            "sendAsEmail": "alias@example.org",
                            "verificationStatus": "accepted",
                            "treatAsAlias": true,
                            "smtpMsa": { "host": "smtp.example.org" },
                        },
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let list = SendAsApi::new(&client).list().await.unwrap();
        assert_eq!(
            list,
            [
                SendAs {
                    send_as_email: "me@example.com".to_string(),
                    display_name: Some("Me".to_string()),
                    is_primary: true,
                    verification_status: None,
                },
                SendAs {
                    send_as_email: "alias@example.org".to_string(),
                    display_name: None,
                    is_primary: false,
                    verification_status: Some("accepted".to_string()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn list_treats_a_missing_array_as_empty() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        assert!(SendAsApi::new(&client).list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        let client = client_with_bootstrapped_token(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(SEND_AS_PATH))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let err = SendAsApi::new(&client).list().await.unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
    }

    // ── resolve_from ─────────────────────────────────────────────────

    fn alias(email: &str, name: Option<&str>, status: Option<&str>) -> SendAs {
        SendAs {
            send_as_email: email.to_string(),
            display_name: name.map(str::to_string),
            is_primary: false,
            verification_status: status.map(str::to_string),
        }
    }

    fn primary(name: Option<&str>) -> SendAs {
        SendAs {
            is_primary: true,
            ..alias("me@example.com", name, None)
        }
    }

    fn account(primary_name: Option<&str>) -> Vec<SendAs> {
        vec![
            primary(primary_name),
            alias("Sales@Example.org", Some("Sales Team"), Some("accepted")),
            alias("bare@example.org", Some("  "), Some("accepted")),
            alias("new@example.org", None, Some("pending")),
            alias(
                "managed@example.org",
                None,
                Some("verificationStatusUnspecified"),
            ),
        ]
    }

    fn resolve_in(send_as: &[SendAs], input: &str) -> Result<Option<Mailbox>> {
        resolve_from(send_as, &Mailbox::parse(input).unwrap())
    }

    fn resolved(primary_name: Option<&str>, input: &str) -> Option<Mailbox> {
        resolve_in(&account(primary_name), input).unwrap()
    }

    fn mailbox(input: &str) -> Mailbox {
        Mailbox::parse(input).unwrap()
    }

    #[test]
    fn resolve_from_matches_case_insensitively_and_writes_the_stored_spelling() {
        assert_eq!(
            resolved(None, "sales@example.ORG"),
            Some(mailbox("Sales Team <Sales@Example.org>"))
        );
    }

    #[test]
    fn resolve_from_keeps_an_explicit_name() {
        assert_eq!(
            resolved(Some("Me"), "Zoë <sales@example.org>"),
            Some(mailbox("Zoë <Sales@Example.org>"))
        );
        assert_eq!(
            resolved(None, "Me Myself <me@example.com>"),
            Some(mailbox("Me Myself <me@example.com>"))
        );
    }

    #[test]
    fn resolve_from_gives_a_nameless_alias_the_primary_name_as_gmail_does() {
        assert_eq!(
            resolved(Some("Me Primary"), "bare@example.org"),
            Some(mailbox("Me Primary <bare@example.org>"))
        );
        assert_eq!(
            resolved(None, "bare@example.org"),
            Some(mailbox("bare@example.org"))
        );
    }

    #[test]
    fn resolve_from_names_the_primary_address_or_leaves_it_to_gmail() {
        assert_eq!(
            resolved(Some("Me Primary"), "me@example.com"),
            Some(mailbox("Me Primary <me@example.com>"))
        );
        // A nameless `From` would drop the account's name, which Gmail
        // fills in when there is no `From` at all.
        assert_eq!(resolved(None, "me@example.com"), None);
        assert_eq!(resolved(Some(" "), "ME@example.com"), None);
    }

    #[test]
    fn resolve_from_refuses_an_unknown_address_and_lists_the_usable_ones() {
        let err = resolve_in(&account(None), "stranger@example.net")
            .unwrap_err()
            .to_string();
        assert!(err.contains("stranger@example.net"), "{err}");
        assert!(
            err.contains(
                "usable: me@example.com, Sales@Example.org, bare@example.org, managed@example.org)"
            ),
            "{err}"
        );
        let err = resolve_in(&[], "a@example.com").unwrap_err().to_string();
        assert!(err.contains("none listed"), "{err}");
    }

    #[test]
    fn resolve_from_refuses_only_an_alias_awaiting_verification() {
        let err = resolve_in(&account(None), "new@example.org")
            .unwrap_err()
            .to_string();
        assert!(err.contains("awaiting verification"), "{err}");
        assert_eq!(
            resolved(None, "managed@example.org"),
            Some(mailbox("managed@example.org"))
        );
    }

    #[test]
    fn resolve_from_blames_a_bad_display_name_from_the_api() {
        let send_as = [alias(
            "x@example.org",
            Some("Evil\r\nBcc: e@example.com"),
            None,
        )];
        let err = format!("{:#}", resolve_in(&send_as, "x@example.org").unwrap_err());
        assert!(
            err.contains("Gmail's display name for x@example.org"),
            "{err}"
        );
        assert!(err.contains("control characters"), "{err}");
        // An explicit name sidesteps it.
        assert_eq!(
            resolve_in(&send_as, "X <x@example.org>").unwrap(),
            Some(mailbox("X <x@example.org>"))
        );
    }
}
