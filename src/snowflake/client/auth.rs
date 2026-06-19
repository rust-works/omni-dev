//! External-browser SSO login (the v1 login flow).
//!
//! The documented handshake:
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

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::form_urlencoded;

use super::config::{BrowserConfig, BrowserLaunch, SnowflakeClientConfig};
use super::error::{Error, Result};
use super::session::LoginTokens;
use super::transport::{request_id, Transport};

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

    // 5. Complete the login.
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
    let body = json!({ "data": {
        "ACCOUNT_NAME": config.account,
        "LOGIN_NAME": config.user,
        "AUTHENTICATOR": "EXTERNALBROWSER",
        "TOKEN": token,
        "PROOF_KEY": proof_key,
        "CLIENT_APP_ID": CLIENT_APP_ID,
        "CLIENT_APP_VERSION": CLIENT_APP_VERSION,
        // We never advertise Arrow capability, so the server returns result sets
        // as JSON (`parse_rows` keeps an Arrow guard as a backstop).
        "SESSION_PARAMETERS": { "CLIENT_SESSION_KEEP_ALIVE": true },
    }});
    let data = transport
        .post("session/v1/login-request", &query, &body, None)
        .await?;

    let session_token = data
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Auth("login-request returned no session token".into()))?
        .to_string();
    let master_token = data
        .get("masterToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_validity_secs = data
        .get("validityInSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    let master_validity_secs = data
        .get("masterValidityInSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(14400);

    Ok(LoginTokens {
        session_token,
        master_token,
        session_validity_secs,
        master_validity_secs,
    })
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
}
