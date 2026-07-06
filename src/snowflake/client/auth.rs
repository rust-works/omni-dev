//! v1 login flows. All methods finish through the shared [`complete_login`],
//! which POSTs `session/v1/login-request` and parses the token response; they
//! differ only in the `AUTHENTICATOR` + credential fields they contribute.
//!
//! External-browser SSO — the documented handshake:
//! 1. Bind a localhost callback listener (its port is the redirect target).
//! 2. `POST session/authenticator-request` with `AUTHENTICATOR=EXTERNALBROWSER`
//!    and `BROWSER_MODE_REDIRECT_PORT` → returns an `ssoUrl` and a `proofKey`.
//! 3. Open `ssoUrl`; the IdP redirects the browser to
//!    `http://localhost:<port>/?token=…`.
//! 4. `POST session/v1/login-request` with the captured `TOKEN` + `PROOF_KEY`
//!    → returns the session token, master token, and their validities.

use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::form_urlencoded;

use super::config::{BrowserConfig, BrowserLaunch, KeyPairConfig, SnowflakeClientConfig};
use super::error::{Error, Result};
use super::jwt;
use super::session::LoginTokens;
use super::transport::{request_id, Transport};
use crate::utils::secret::Secret;

/// Identifies this client to Snowflake.
const CLIENT_APP_ID: &str = "omni-dev";
/// This client's version.
const CLIENT_APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long to wait for the browser SSO callback before giving up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs the external-browser SSO login and returns the session tokens.
pub(crate) async fn external_browser_login(
    transport: &Transport,
    config: &SnowflakeClientConfig,
    browser: &BrowserConfig,
) -> Result<LoginTokens> {
    // 1. Bind the callback listener first so the redirect port is known.
    let listener = TcpListener::bind(SocketAddr::new(
        browser.callback_addr,
        browser.callback_port,
    ))
    .await
    .map_err(Error::Io)?;
    let port = listener.local_addr().map_err(Error::Io)?.port();

    // 2. Ask Snowflake for the SSO URL and proof key.
    let rid = request_id();
    let body = json!({ "data": {
        "ACCOUNT_NAME": config.account,
        "LOGIN_NAME": config.user,
        "AUTHENTICATOR": "EXTERNALBROWSER",
        "BROWSER_MODE_REDIRECT_PORT": port.to_string(),
        "CLIENT_APP_ID": CLIENT_APP_ID,
        "CLIENT_APP_VERSION": CLIENT_APP_VERSION,
    }});
    let data = transport
        .post(
            "session/authenticator-request",
            &[("requestId", rid.as_str())],
            &body,
            None,
        )
        .await?;
    let sso_url = data
        .get("ssoUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Auth("authenticator-request returned no ssoUrl".into()))?
        .to_string();
    let proof_key = data
        .get("proofKey")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Auth("authenticator-request returned no proofKey".into()))?
        .to_string();

    // 3. Open the SSO URL (in the configured browser/profile).
    open_browser(&browser.launch, &sso_url)?;

    // 4. Capture the token from the localhost redirect.
    let token = wait_for_callback(listener).await?;

    // 5. Complete the login with the captured SSO token + proof key.
    let data = Map::from_iter([
        ("AUTHENTICATOR".to_string(), json!("EXTERNALBROWSER")),
        ("TOKEN".to_string(), json!(token)),
        ("PROOF_KEY".to_string(), json!(proof_key)),
    ]);
    complete_login(transport, config, data).await
}

/// The shared tail of every v1 login: augments the method-specific `data`
/// (which supplies `AUTHENTICATOR` and the credential fields) with the common
/// identity/app fields, POSTs `session/v1/login-request` — carrying the config's
/// default warehouse/role/database/schema as query params — and parses the token
/// response into [`LoginTokens`].
pub(super) async fn complete_login(
    transport: &Transport,
    config: &SnowflakeClientConfig,
    mut data: Map<String, Value>,
) -> Result<LoginTokens> {
    data.insert("ACCOUNT_NAME".into(), json!(config.account));
    data.insert("LOGIN_NAME".into(), json!(config.user));
    data.insert("CLIENT_APP_ID".into(), json!(CLIENT_APP_ID));
    data.insert("CLIENT_APP_VERSION".into(), json!(CLIENT_APP_VERSION));
    // We never advertise Arrow capability, so the server returns result sets as
    // JSON (`parse_rows` keeps an Arrow guard as a backstop). Keep the session
    // alive so the master token can be renewed for the daemon's lifetime.
    data.entry("SESSION_PARAMETERS")
        .or_insert_with(|| json!({ "CLIENT_SESSION_KEEP_ALIVE": true }));

    let rid = request_id();
    let mut query: Vec<(&str, &str)> = vec![("requestId", rid.as_str())];
    if let Some(warehouse) = &config.warehouse {
        query.push(("warehouse", warehouse));
    }
    if let Some(database) = &config.database {
        query.push(("databaseName", database));
    }
    if let Some(schema) = &config.schema {
        query.push(("schemaName", schema));
    }
    if let Some(role) = &config.role {
        query.push(("roleName", role));
    }
    let body = json!({ "data": Value::Object(data) });
    let resp = transport
        .post("session/v1/login-request", &query, &body, None)
        .await?;

    let session_token = resp
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Auth("login-request returned no session token".into()))?
        .to_string();
    let master_token = resp
        .get("masterToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_validity_secs = resp
        .get("validityInSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let master_validity_secs = resp
        .get("masterValidityInSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(14400);

    Ok(LoginTokens {
        session_token: session_token.into(),
        master_token: master_token.into(),
        session_validity_secs,
        master_validity_secs,
    })
}

/// Non-interactive login with a programmatic access token (PAT). The PAT is
/// presented as the login `TOKEN` under `AUTHENTICATOR=PROGRAMMATIC_ACCESS_TOKEN`;
/// no browser or callback listener is involved.
pub(crate) async fn pat_login(
    transport: &Transport,
    config: &SnowflakeClientConfig,
    token: &Secret,
) -> Result<LoginTokens> {
    let data = Map::from_iter([
        (
            "AUTHENTICATOR".to_string(),
            json!("PROGRAMMATIC_ACCESS_TOKEN"),
        ),
        ("TOKEN".to_string(), json!(token.expose_secret())),
    ]);
    complete_login(transport, config, data).await
}

/// Non-interactive login with a locally-signed RS256 JWT (key-pair auth). The
/// JWT is built from the account/user and the configured RSA private key and
/// presented as the login `TOKEN` under `AUTHENTICATOR=SNOWFLAKE_JWT`; no
/// browser or callback listener is involved.
pub(crate) async fn keypair_jwt_login(
    transport: &Transport,
    config: &SnowflakeClientConfig,
    keypair: &KeyPairConfig,
) -> Result<LoginTokens> {
    let jwt = jwt::build_jwt(
        &config.account,
        &config.user,
        keypair.private_key_pem.expose_secret(),
    )?;
    let data = Map::from_iter([
        ("AUTHENTICATOR".to_string(), json!("SNOWFLAKE_JWT")),
        ("TOKEN".to_string(), json!(jwt)),
    ]);
    complete_login(transport, config, data).await
}

/// Opens `url` in the configured browser. With [`BrowserLaunch::Command`], a
/// `{url}` placeholder (or a trailing argument) receives the URL — set this to a
/// `Google Chrome --profile-directory=… --new-window {url}` command to target a
/// specific profile in a focused window.
// `{url}` is a literal command placeholder we substitute, not a format string.
#[allow(clippy::literal_string_with_formatting_args)]
fn open_browser(launch: &BrowserLaunch, url: &str) -> Result<()> {
    match launch {
        BrowserLaunch::Manual => {
            tracing::info!("Open this URL in a browser to sign in to Snowflake:\n{url}");
            Ok(())
        }
        BrowserLaunch::Command(args) => {
            let mut parts = args.iter();
            let program = parts
                .next()
                .ok_or_else(|| Error::Auth("empty browser command".into()))?;
            let mut command = Command::new(program);
            let mut placed = false;
            for arg in parts {
                if arg.contains("{url}") {
                    command.arg(arg.replace("{url}", url));
                    placed = true;
                } else {
                    command.arg(arg);
                }
            }
            if !placed {
                command.arg(url);
            }
            spawn_detached(command)
        }
        BrowserLaunch::Auto => {
            let program = if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(target_os = "windows") {
                "explorer"
            } else {
                "xdg-open"
            };
            let mut command = Command::new(program);
            command.arg(url);
            spawn_detached(command)
        }
    }
}

/// Spawns a browser command detached from this process's stdio.
fn spawn_detached(mut command: Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(Error::Io)
}

/// Accepts one localhost connection and extracts the `token` query parameter
/// from the redirected `GET` request line.
async fn wait_for_callback(listener: TcpListener) -> Result<String> {
    let (mut stream, _addr) = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept())
        .await
        .map_err(|_| Error::Auth("timed out waiting for the browser SSO callback".into()))?
        .map_err(Error::Io)?;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.map_err(Error::Io)?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let token = parse_callback_token(&request)
        .ok_or_else(|| Error::Auth("SSO callback contained no token".into()))?;

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
        <html><body>Snowflake sign-in complete. You can close this tab.</body></html>";
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    Ok(token)
}

/// Extracts the `token` parameter from an HTTP request's first line.
fn parse_callback_token(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let path = first_line.split_whitespace().nth(1)?; // "/?token=…"
    let query = path.split_once('?')?.1;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_from_callback_request() {
        let request = "GET /?token=abc123&foo=bar HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_callback_token(request).as_deref(), Some("abc123"));
    }

    #[test]
    fn url_decodes_the_token() {
        let request = "GET /?token=a%2Bb%2Fc%3D HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback_token(request).as_deref(), Some("a+b/c="));
    }

    #[test]
    fn missing_token_is_none() {
        assert_eq!(parse_callback_token("GET /?foo=bar HTTP/1.1\r\n\r\n"), None);
        assert_eq!(parse_callback_token("garbage"), None);
    }

    #[test]
    fn open_browser_manual_logs_and_succeeds() {
        assert!(open_browser(&BrowserLaunch::Manual, "https://example/sso").is_ok());
    }

    #[test]
    fn open_browser_command_substitutes_url_placeholder() {
        // `true` ignores its args and exits 0, exercising arg building + spawn
        // (including the `{url}` substitution) without opening anything.
        let launch = BrowserLaunch::Command(vec!["true".to_string(), "--url={url}".to_string()]);
        assert!(open_browser(&launch, "https://example/sso").is_ok());
    }

    #[test]
    fn open_browser_command_appends_url_when_no_placeholder() {
        let launch = BrowserLaunch::Command(vec!["true".to_string()]);
        assert!(open_browser(&launch, "https://example/sso").is_ok());
    }

    #[test]
    fn open_browser_command_rejects_empty_args() {
        let launch = BrowserLaunch::Command(vec![]);
        assert!(matches!(open_browser(&launch, "u"), Err(Error::Auth(_))));
    }
}
