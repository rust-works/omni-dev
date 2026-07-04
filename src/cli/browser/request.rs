//! `omni-dev browser bridge request` — a thin client that sends a request through a
//! running bridge, injecting the required auth headers itself.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use clap::{Parser, ValueEnum};
use futures::StreamExt as _;

use crate::browser::auth;
use crate::browser::bridge::DEFAULT_CONTROL_PORT;
use crate::browser::client::BridgeClient;
use crate::browser::protocol::{ControlRequest, ResponseEnvelope, StreamLine};

/// Fetch credentials mode forwarded to the browser `fetch()`.
///
/// Mirrors the Fetch API's `credentials` option. `Include` (the default)
/// preserves pre-credentials behavior; `Omit` is required to read a
/// wildcard-CORS (`Access-Control-Allow-Origin: *`) cross-origin response, which
/// the browser refuses to expose to a credentialed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Credentials {
    /// Send cookies/auth on every request (default; correct for same-origin APIs).
    Include,
    /// Send no credentials; required for wildcard-CORS cross-origin assets.
    Omit,
    /// Send credentials only on same-origin requests (browser `fetch` default).
    SameOrigin,
}

impl Credentials {
    /// The wire value the browser snippet passes to `fetch()`'s `credentials`,
    /// owned so it can be mapped straight into the optional request field.
    fn to_fetch_value(self) -> String {
        match self {
            Self::Include => "include",
            Self::Omit => "omit",
            Self::SameOrigin => "same-origin",
        }
        .to_string()
    }
}

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
    /// UTF-8 text only; use `--body-file` for binary payloads.
    #[arg(long)]
    pub body: Option<String>,

    /// Read the request body from a file as raw bytes, base64-encoded on the
    /// wire (the browser decodes it before `fetch()`). Use for binary payloads
    /// such as images, protobuf, or gzip. Mutually exclusive with `--body`.
    #[arg(long, value_name = "PATH", conflicts_with = "body")]
    pub body_file: Option<PathBuf>,

    /// Fetch credentials mode. Defaults to `include` (cookies/auth sent). Use
    /// `omit` to read a wildcard-CORS cross-origin response (e.g. a public CDN
    /// asset), which a credentialed request cannot read.
    #[arg(long, value_enum)]
    pub credentials: Option<Credentials>,

    /// Control-plane port of the running bridge.
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub control_port: u16,

    /// Read the session token from this `0600` file instead of the environment.
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,

    /// Stream the response: print body chunks to stdout as they arrive instead
    /// of buffering the whole response into one envelope. Use for SSE / chunked /
    /// long-lived endpoints.
    #[arg(long)]
    pub stream: bool,

    /// Route to a specific connected tab: a connection id (from
    /// `/__bridge/status`) or an `Origin` that uniquely matches one tab.
    /// Required when more than one tab is connected.
    #[arg(long, value_name = "ID|ORIGIN")]
    pub target: Option<String>,

    /// Permit a cross-origin outbound URL for this request only (e.g.
    /// `https://static.xx.fbcdn.net`). Takes precedence over the bridge's
    /// `serve --allow-origin` for this request's outbound-URL check, and does
    /// not affect the tab's WebSocket connection. Omit for same-origin
    /// (relative) requests.
    #[arg(long, value_name = "URL")]
    pub allow_origin: Option<String>,
}

impl RequestCommand {
    /// Executes the request command.
    pub async fn execute(self) -> Result<()> {
        let token = super::resolve_client_token(self.token_file.as_deref())?;
        let headers = parse_headers(&self.headers)?;
        let (body, encoding) = resolve_body(self.body.as_deref(), self.body_file.as_deref())?;

        let payload = ControlRequest {
            url: self.url,
            method: self.method,
            headers,
            body,
            stream: self.stream,
            target: self.target,
            allow_origin: self.allow_origin,
            credentials: self.credentials.map(Credentials::to_fetch_value),
            encoding,
        };

        let client = BridgeClient::new(self.control_port, token);
        let endpoint = client.endpoint();
        let resp = client
            .request_builder(&payload)
            .send()
            .await
            .with_context(|| format!("Failed to reach bridge at {endpoint} (is it running?)"))?;

        let status = resp.status();
        if self.stream {
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                bail!("bridge returned {status}: {text}");
            }
            return stream_ndjson(resp).await;
        }

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

/// Consumes an NDJSON streamed response: decodes each `{seq,chunk}` line's
/// base64 to raw bytes on stdout as it arrives, reports the head status and any
/// error on stderr, and stops on the terminating `{done}` line.
async fn stream_ndjson(resp: reqwest::Response) -> Result<()> {
    let mut body = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let stdout = std::io::stdout();
    while let Some(piece) = body.next().await {
        let piece = piece.context("Failed to read bridge stream")?;
        buf.extend_from_slice(&piece);
        // Process every complete newline-terminated line in the buffer.
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = &line[..line.len() - 1]; // drop the trailing newline
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<StreamLine>(line) {
                Ok(StreamLine::Head { status, .. }) => {
                    eprintln!("status: {status}");
                }
                Ok(StreamLine::Chunk { chunk, .. }) => {
                    let bytes = BASE64
                        .decode(chunk.as_bytes())
                        .context("bridge sent an invalid base64 chunk")?;
                    let mut handle = stdout.lock();
                    handle.write_all(&bytes).context("Failed to write chunk")?;
                    handle.flush().ok();
                }
                Ok(StreamLine::Done { .. }) => return Ok(()),
                Ok(StreamLine::Error { error }) => bail!("browser stream error: {error}"),
                Err(e) => bail!("unparseable stream line from bridge: {e}"),
            }
        }
    }
    Ok(())
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

/// Resolves the request body into a `(body, encoding)` pair.
///
/// `--body` carries UTF-8 text (verbatim, or read from a file when prefixed with
/// `@`) and never sets an encoding. `--body-file` reads raw bytes and
/// base64-encodes them, tagging `encoding = "base64"` so the browser snippet
/// decodes the body back to bytes before `fetch()`. The two are mutually
/// exclusive (enforced by clap `conflicts_with`; re-checked here defensively).
fn resolve_body(
    body: Option<&str>,
    body_file: Option<&Path>,
) -> Result<(Option<String>, Option<String>)> {
    match (body, body_file) {
        (Some(_), Some(_)) => bail!("--body and --body-file are mutually exclusive"),
        (None, Some(path)) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("Failed to read body file {}", path.display()))?;
            Ok((Some(BASE64.encode(bytes)), Some("base64".to_string())))
        }
        (Some(spec), None) => match spec.strip_prefix('@') {
            Some(path) => {
                let contents = std::fs::read_to_string(path)
                    .with_context(|| format!("Failed to read body file {path}"))?;
                Ok((Some(contents), None))
            }
            None => Ok((Some(spec.to_string()), None)),
        },
        (None, None) => Ok((None, None)),
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
    fn credentials_defaults_to_none() {
        let cmd = RequestCommand::try_parse_from(["request", "--url", "/x"]).unwrap();
        assert!(cmd.credentials.is_none());
    }

    #[test]
    fn credentials_parses_each_mode() {
        for (arg, expected) in [
            ("include", Credentials::Include),
            ("omit", Credentials::Omit),
            ("same-origin", Credentials::SameOrigin),
        ] {
            let cmd =
                RequestCommand::try_parse_from(["request", "--url", "/x", "--credentials", arg])
                    .unwrap();
            assert_eq!(cmd.credentials, Some(expected));
        }
    }

    #[test]
    fn credentials_rejects_invalid_value() {
        assert!(RequestCommand::try_parse_from([
            "request",
            "--url",
            "/x",
            "--credentials",
            "bogus"
        ])
        .is_err());
    }

    #[test]
    fn credentials_maps_to_fetch_wire_value() {
        assert_eq!(Credentials::Include.to_fetch_value(), "include");
        assert_eq!(Credentials::Omit.to_fetch_value(), "omit");
        assert_eq!(Credentials::SameOrigin.to_fetch_value(), "same-origin");
    }

    #[test]
    fn resolve_body_inline_and_file() {
        // No body → no encoding.
        assert_eq!(resolve_body(None, None).unwrap(), (None, None));
        // Inline text → verbatim, no encoding.
        assert_eq!(
            resolve_body(Some("hi"), None).unwrap(),
            (Some("hi".to_string()), None)
        );

        // `@file` → text contents, no encoding.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.json");
        std::fs::write(&path, "{\"a\":1}").unwrap();
        let spec = format!("@{}", path.display());
        assert_eq!(
            resolve_body(Some(&spec), None).unwrap(),
            (Some("{\"a\":1}".to_string()), None)
        );
    }

    #[test]
    fn resolve_body_file_base64_round_trips_binary() {
        // Non-UTF-8 bytes that `read_to_string` would reject.
        let raw: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x80];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.bin");
        std::fs::write(&path, raw).unwrap();

        let (body, encoding) = resolve_body(None, Some(path.as_path())).unwrap();
        assert_eq!(encoding.as_deref(), Some("base64"));
        // The base64 payload decodes back to the exact original bytes.
        let decoded = BASE64.decode(body.unwrap().as_bytes()).unwrap();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn resolve_body_file_always_base64_even_for_text() {
        // `--body-file` is the raw-bytes path: it always base64-encodes, even a
        // perfectly valid UTF-8 file (byte-exact on the wire either way).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "hello").unwrap();

        let (body, encoding) = resolve_body(None, Some(path.as_path())).unwrap();
        assert_eq!(encoding.as_deref(), Some("base64"));
        assert_eq!(BASE64.decode(body.unwrap().as_bytes()).unwrap(), b"hello");
    }

    #[test]
    fn body_and_body_file_are_mutually_exclusive() {
        // clap rejects the two flags together (`conflicts_with`).
        assert!(RequestCommand::try_parse_from([
            "request",
            "--url",
            "/x",
            "--body",
            "hi",
            "--body-file",
            "payload.bin",
        ])
        .is_err());
    }
}
