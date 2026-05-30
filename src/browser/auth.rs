//! Authentication and request-guard primitives for the browser bridge.
//!
//! The trust boundary, stated once: **a request is trusted only if it presents
//! the session token AND is not a cross-origin browser request; everything else
//! is denied.** A localhost bind is necessary but not sufficient — it stops
//! off-host access but not other local users/processes, nor web pages the
//! operator visits. See [ADR-0036](../../docs/adrs/adr-0036.md).
//!
//! The functions here are deliberately small and operate on borrowed primitives
//! rather than framework types so the security checks can be unit-tested in
//! isolation from axum / tungstenite.

use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use rand::RngCore;

/// Environment variable an operator may use to pin the session token instead of
/// letting the bridge generate one. Never read from argv (`ps`/`/proc` expose
/// it).
pub const TOKEN_ENV: &str = "OMNI_BRIDGE_TOKEN";

/// Custom header every control-plane request must carry. A custom header forces
/// a CORS preflight that the server refuses, blocking simple-request CSRF.
pub const BRIDGE_HEADER: &str = "x-omni-bridge";

/// Required value of [`BRIDGE_HEADER`].
pub const BRIDGE_HEADER_VALUE: &str = "1";

/// Number of random bytes behind a generated token (URL-safe base64, no pad).
const TOKEN_BYTES: usize = 32;

/// Generates a fresh random session token (256 bits, URL-safe base64).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Resolves the session token, in priority order:
///
/// 1. `--token-file` — read its trimmed contents. On Unix the file must be
///    `0600` (owner-only) or resolution fails closed.
/// 2. `OMNI_BRIDGE_TOKEN` — read from the environment.
/// 3. Otherwise a fresh token is generated.
///
/// The token is **never** accepted from argv.
pub fn resolve_token(token_file: Option<&Path>) -> Result<String> {
    if let Some(path) = token_file {
        return read_token_file(path);
    }
    if let Ok(value) = std::env::var(TOKEN_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    Ok(generate_token())
}

/// Resolves an *existing* session token for a client.
///
/// Used by the `request` subcommand: `--token-file` then `OMNI_BRIDGE_TOKEN`.
/// Unlike [`resolve_token`], it never generates one — a client must use the
/// token the running bridge printed.
pub fn resolve_existing_token(token_file: Option<&Path>) -> Result<String> {
    if let Some(path) = token_file {
        return read_token_file(path);
    }
    match std::env::var(TOKEN_ENV) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => bail!(
            "No session token found. Set {TOKEN_ENV} or pass --token-file with the token the \
             running bridge printed."
        ),
    }
}

fn read_token_file(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("Failed to stat token file {}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "Token file {} must be 0600 (owner-only); found {:o}",
                path.display(),
                mode
            );
        }
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read token file {}", path.display()))?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        bail!("Token file {} is empty", path.display());
    }
    Ok(trimmed.to_string())
}

/// Constant-time string comparison, to avoid leaking the token via timing.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Checks an `Authorization` header value against the expected token.
///
/// Accepts only `Bearer <token>` with a constant-time token comparison.
#[must_use]
pub fn bearer_matches(authorization: Option<&str>, token: &str) -> bool {
    let Some(value) = authorization else {
        return false;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(presented.trim(), token)
}

/// Whether the request carries the mandatory `X-Omni-Bridge: 1` header.
#[must_use]
pub fn has_bridge_header(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == BRIDGE_HEADER_VALUE)
}

/// Whether `host` is an allowed loopback authority for `control_port`.
///
/// Blocks DNS rebinding: only the exact `localhost:<port>` / `127.0.0.1:<port>`
/// (and the IPv6 loopback) authorities are accepted.
#[must_use]
pub fn host_allowed(host: &str, control_port: u16) -> bool {
    let allowed = [
        format!("localhost:{control_port}"),
        format!("127.0.0.1:{control_port}"),
        format!("[::1]:{control_port}"),
    ];
    allowed.iter().any(|a| a == host)
}

/// Whether a request looks browser-originated and must therefore be denied.
///
/// A legitimate CLI client sends neither an `Origin` header nor
/// `Sec-Fetch-Site: cross-site`/`same-site`. Any such header marks a request a
/// web page made, which the control plane refuses.
#[must_use]
pub fn is_browser_originated(origin: Option<&str>, sec_fetch_site: Option<&str>) -> bool {
    if origin.is_some() {
        return true;
    }
    matches!(
        sec_fetch_site.map(str::trim),
        Some("cross-site" | "same-site" | "same-origin")
    )
}

/// Rejects header names/values that could smuggle a second header or request
/// line via CR/LF (or other control characters).
#[must_use]
pub fn header_is_safe(name: &str, value: &str) -> bool {
    let bad = |s: &str| s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0);
    !name.is_empty() && !bad(name) && !bad(value)
}

/// Normalises a decoded request path, rejecting traversal and control
/// characters. Run **before** the `/__bridge/` routing/auth split so a
/// percent-encoded segment cannot bypass the prefix check.
///
/// Returns the percent-decoded path, or `None` if it is unsafe.
#[must_use]
pub fn normalize_request_path(raw: &str) -> Option<String> {
    let decoded = percent_decode(raw)?;
    if decoded.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return None;
    }
    // Reject any `..` path segment (traversal).
    if decoded
        .split('/')
        .any(|seg| seg == ".." || seg == "..%2f" || seg == "..%2F")
    {
        return None;
    }
    Some(decoded)
}

/// Minimal percent-decoder for request paths. Returns `None` on malformed
/// escapes or non-UTF-8 results.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).copied().and_then(hex_val)?;
                let lo = bytes.get(i + 2).copied().and_then(hex_val)?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Reason an outbound URL was rejected by [`validate_outbound_url`].
#[derive(Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The URL is absolute/cross-origin and no `--allow-origin` permits it.
    CrossOriginDenied,
    /// The URL could not be parsed or is otherwise malformed.
    Malformed,
}

/// Enforces the default-closed outbound scope.
///
/// Relative URLs (page-origin) are always allowed. Absolute or
/// protocol-relative URLs are rejected unless their origin exactly matches
/// `allow_origin`.
pub fn validate_outbound_url(url: &str, allow_origin: Option<&str>) -> Result<(), ScopeError> {
    // Protocol-relative (`//host/...`) is cross-origin.
    let is_relative = url.starts_with('/') && !url.starts_with("//");
    if is_relative {
        if url.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(ScopeError::Malformed);
        }
        return Ok(());
    }

    let allow = allow_origin.ok_or(ScopeError::CrossOriginDenied)?;
    let target = url::Url::parse(url).map_err(|_| ScopeError::Malformed)?;
    let allowed = url::Url::parse(allow).map_err(|_| ScopeError::Malformed)?;
    if origins_match(&target, &allowed) {
        Ok(())
    } else {
        Err(ScopeError::CrossOriginDenied)
    }
}

fn origins_match(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Extracts and verifies the bridge token from WebSocket subprotocols.
///
/// Returns the matching subprotocol to echo back in the handshake response, or
/// `None` if no presented protocol matches the expected token.
#[must_use]
pub fn ws_subprotocol_token<'a, I>(subprotocols: I, token: &str) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    subprotocols
        .into_iter()
        .map(str::trim)
        .find(|p| constant_time_eq(p, token))
}

/// Whether a WebSocket upgrade's `Origin` is permitted.
///
/// With no `--allow-origin` configured, any origin is accepted (the token in
/// the subprotocol is the gate). With one configured, the origin must match.
#[must_use]
pub fn ws_origin_allowed(origin: Option<&str>, allow_origin: Option<&str>) -> bool {
    match allow_origin {
        None => true,
        Some(allowed) => origin.is_some_and(|o| {
            url::Url::parse(o)
                .ok()
                .zip(url::Url::parse(allowed).ok())
                .is_some_and(|(o, a)| origins_match(&o, &a))
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_urlsafe() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(a.len() >= 40);
    }

    #[test]
    fn constant_time_eq_matches_str_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn bearer_accepts_only_correct_token() {
        assert!(bearer_matches(Some("Bearer tok"), "tok"));
        assert!(!bearer_matches(Some("Bearer wrong"), "tok"));
        assert!(!bearer_matches(Some("tok"), "tok"));
        assert!(!bearer_matches(None, "tok"));
    }

    #[test]
    fn bridge_header_must_be_one() {
        assert!(has_bridge_header(Some("1")));
        assert!(has_bridge_header(Some(" 1 ")));
        assert!(!has_bridge_header(Some("0")));
        assert!(!has_bridge_header(None));
    }

    #[test]
    fn host_allowlist_blocks_rebinding() {
        assert!(host_allowed("localhost:9998", 9998));
        assert!(host_allowed("127.0.0.1:9998", 9998));
        assert!(host_allowed("[::1]:9998", 9998));
        assert!(!host_allowed("evil.example.com:9998", 9998));
        assert!(!host_allowed("localhost:9999", 9998));
        assert!(!host_allowed("localhost", 9998));
    }

    #[test]
    fn browser_origin_is_rejected() {
        assert!(is_browser_originated(Some("https://evil.test"), None));
        assert!(is_browser_originated(None, Some("cross-site")));
        assert!(is_browser_originated(None, Some("same-site")));
        assert!(!is_browser_originated(None, None));
        assert!(!is_browser_originated(None, Some("none")));
    }

    #[test]
    fn header_crlf_is_rejected() {
        assert!(header_is_safe("Accept", "application/json"));
        assert!(!header_is_safe("X\r\nEvil", "v"));
        assert!(!header_is_safe("X", "a\r\nSet-Cookie: y"));
        assert!(!header_is_safe("", "v"));
    }

    #[test]
    fn path_normalization_rejects_traversal() {
        assert_eq!(
            normalize_request_path("/loki/api/v1/labels").as_deref(),
            Some("/loki/api/v1/labels")
        );
        assert_eq!(
            normalize_request_path("/a/%2e%2e/b"),
            Some("/a/../b".to_string()).filter(|_| false).or(None)
        );
        assert!(normalize_request_path("/a/../b").is_none());
        assert!(normalize_request_path("/a/%2e%2e/b").is_none());
        assert!(normalize_request_path("/a/%00/b").is_none());
        assert!(normalize_request_path("/bad%2").is_none());
    }

    #[test]
    fn outbound_scope_is_default_closed() {
        assert_eq!(validate_outbound_url("/api/foo", None), Ok(()));
        assert_eq!(
            validate_outbound_url("https://evil.test/x", None),
            Err(ScopeError::CrossOriginDenied)
        );
        assert_eq!(
            validate_outbound_url("//evil.test/x", None),
            Err(ScopeError::CrossOriginDenied)
        );
    }

    #[test]
    fn outbound_scope_honors_allow_origin() {
        assert_eq!(
            validate_outbound_url("https://ok.test/x", Some("https://ok.test")),
            Ok(())
        );
        assert_eq!(
            validate_outbound_url("https://evil.test/x", Some("https://ok.test")),
            Err(ScopeError::CrossOriginDenied)
        );
        // Relative always allowed regardless of allow-origin.
        assert_eq!(validate_outbound_url("/x", Some("https://ok.test")), Ok(()));
    }

    #[test]
    fn ws_subprotocol_token_selects_match() {
        assert_eq!(ws_subprotocol_token(["a", "tok", "b"], "tok"), Some("tok"));
        assert_eq!(ws_subprotocol_token(["a", "b"], "tok"), None);
        assert_eq!(ws_subprotocol_token([], "tok"), None);
    }

    #[test]
    fn ws_origin_allowed_logic() {
        assert!(ws_origin_allowed(Some("https://anything.test"), None));
        assert!(ws_origin_allowed(None, None));
        assert!(ws_origin_allowed(
            Some("https://ok.test"),
            Some("https://ok.test")
        ));
        assert!(!ws_origin_allowed(
            Some("https://evil.test"),
            Some("https://ok.test")
        ));
        assert!(!ws_origin_allowed(None, Some("https://ok.test")));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_requires_0600() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tok");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "secret-token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_token(Some(&path)).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(resolve_token(Some(&path)).unwrap(), "secret-token");
    }
}
