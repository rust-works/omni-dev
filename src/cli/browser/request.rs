//! `omni-dev browser request` — a thin client that sends a request through a
//! running bridge, injecting the required auth headers itself.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::browser::auth;
use crate::browser::protocol::{ControlRequest, ResponseEnvelope};

/// Default control-plane port (matches `bridge`'s default).
const DEFAULT_CONTROL_PORT: u16 = 9998;

/// Sends a request through a running bridge and prints the response.
///
/// Reads the session token from `OMNI_BRIDGE_TOKEN` or `--token-file`, adds the
/// `Authorization` and `X-Omni-Bridge` headers, and POSTs to
/// `/__bridge/request` on the running bridge.
#[derive(Parser)]
pub struct RequestCommand {
    /// Request URL, relative to the browser's page origin (e.g. `/api/foo`).
    #[arg(long)]
    pub url: String,

    /// HTTP method.
    #[arg(long, default_value = "GET")]
    pub method: String,

    /// Request header as `Name: Value`. May be repeated.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub headers: Vec<String>,

    /// Request body. Prefix with `@` to read from a file (e.g. `@payload.json`).
    #[arg(long)]
    pub body: Option<String>,

    /// Control-plane port of the running bridge.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub control_port: u16,

    /// Read the session token from this `0600` file instead of the environment.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
}

impl RequestCommand {
    /// Executes the request command.
    pub async fn execute(self) -> Result<()> {
        let token = auth::resolve_existing_token(self.token_file.as_deref())?;
        let headers = parse_headers(&self.headers)?;
        let body = resolve_body(self.body.as_deref())?;

        let payload = ControlRequest {
            url: self.url,
            method: self.method,
            headers,
            body,
        };

        let endpoint = format!("http://127.0.0.1:{}/__bridge/request", self.control_port);
        let client = reqwest::Client::new();
        let resp = client
            .post(&endpoint)
            .bearer_auth(&token)
            .header(auth::BRIDGE_HEADER, auth::BRIDGE_HEADER_VALUE)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("Failed to reach bridge at {endpoint} (is it running?)"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("Failed to read bridge response")?;
        if !status.is_success() {
            bail!("bridge returned {status}: {text}");
        }

        // Pretty-print the structured envelope when possible; fall back to raw.
        match serde_json::from_str::<ResponseEnvelope>(&text) {
            Ok(env) => println!("{}", serde_json::to_string_pretty(&env)?),
            Err(_) => println!("{text}"),
        }
        Ok(())
    }
}

/// Parses `Name: Value` header strings into a map.
fn parse_headers(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for h in raw {
        let (name, value) = h
            .split_once(':')
            .with_context(|| format!("Invalid header (expected 'Name: Value'): {h}"))?;
        let name = name.trim();
        let value = value.trim();
        if !auth::header_is_safe(name, value) {
            bail!("Invalid header name or value: {h}");
        }
        map.insert(name.to_string(), value.to_string());
    }
    Ok(map)
}

/// Resolves a `--body` argument, reading from a file when prefixed with `@`.
fn resolve_body(body: Option<&str>) -> Result<Option<String>> {
    match body {
        None => Ok(None),
        Some(spec) => match spec.strip_prefix('@') {
            Some(path) => {
                let contents = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read body file {path}"))?;
                Ok(Some(contents))
            }
            None => Ok(Some(spec.to_string())),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_headers_splits_name_and_value() {
        let h = parse_headers(&["Accept: application/json".to_string()]).unwrap();
        assert_eq!(
            h.get("Accept").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn parse_headers_rejects_malformed() {
        assert!(parse_headers(&["no-colon".to_string()]).is_err());
    }

    #[test]
    fn parse_headers_rejects_crlf() {
        assert!(parse_headers(&["X: a\r\nEvil: y".to_string()]).is_err());
    }

    #[test]
    fn resolve_body_inline_and_file() {
        assert_eq!(resolve_body(None).unwrap(), None);
        assert_eq!(resolve_body(Some("hi")).unwrap(), Some("hi".to_string()));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(&path, "{\"a\":1}").unwrap();
        let spec = format!("@{}", path.display());
        assert_eq!(
            resolve_body(Some(&spec)).unwrap(),
            Some("{\"a\":1}".to_string())
        );
    }
}
