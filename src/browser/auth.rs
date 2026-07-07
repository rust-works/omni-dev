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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine;
use rand::Rng;

use crate::utils::env::{EnvSource, SystemEnv};

/// Environment variable an operator may use to pin the session token instead of
/// letting the bridge generate one. Never read from argv (`ps`/`/proc` expose
/// it).
pub const TOKEN_ENV: &str = "OMNI_BRIDGE_TOKEN";

/// Custom header every control-plane request must carry. A custom header forces
/// a CORS preflight that the server refuses, blocking simple-request CSRF.
pub const BRIDGE_HEADER: &str = "x-omni-bridge";

/// Required value of [`BRIDGE_HEADER`].
pub const BRIDGE_HEADER_VALUE: &str = "1";

/// Optional header selecting which connected tab a control-plane request targets.
///
/// The value is a connection id, or an `Origin` that uniquely matches one tab. It
/// takes precedence over a `target` body field, and is stripped before forwarding.
pub const BRIDGE_TARGET_HEADER: &str = "x-omni-bridge-target";

/// Optional header carrying the originating client's request-log `invocation_id`.
///
/// Threaded so the bridge can correlate the HTTP records it writes while serving
/// a request back to the CLI/MCP invocation that issued it, rather than the
/// bridge's own (#1198). Non-secret, server-side only — never forwarded to the
/// browser.
pub const BRIDGE_ORIGIN_HEADER: &str = "x-omni-bridge-origin";

/// Number of random bytes behind a generated token (URL-safe base64, no pad).
const TOKEN_BYTES: usize = 32;

/// Generates a fresh random session token (256 bits, URL-safe base64).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
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
    resolve_token_with(&SystemEnv, token_file)
}

/// [`resolve_token`] over an injected [`EnvSource`], so the `OMNI_BRIDGE_TOKEN`
/// branch is tested without mutating the process environment (issue #1030).
pub(crate) fn resolve_token_with(
    env: &impl EnvSource,
    token_file: Option<&Path>,
) -> Result<String> {
    if let Some(path) = token_file {
        return read_token_file(path);
    }
    if let Some(value) = env.var(TOKEN_ENV) {
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
    resolve_existing_token_with(&SystemEnv, token_file)
}

/// [`resolve_existing_token`] over an injected [`EnvSource`]; never generates a
/// token. Tests pass a `MapEnv` rather than mutating the process environment.
pub(crate) fn resolve_existing_token_with(
    env: &impl EnvSource,
    token_file: Option<&Path>,
) -> Result<String> {
    if let Some(path) = token_file {
        return read_token_file(path);
    }
    match env.var(TOKEN_ENV) {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
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
/// protocol-relative URLs are rejected unless their origin matches one of the
/// `allowed` origins. An empty `allowed` slice permits relative URLs only.
///
/// The caller resolves `allowed`: the per-request override (a single origin) or
/// the per-origin allowlist entry for the tab a request is routed to (see
/// [`OriginAllowlist::outbound_for`]).
pub fn validate_outbound_url(url: &str, allowed: &[&str]) -> Result<(), ScopeError> {
    // Protocol-relative (`//host/...`) is cross-origin.
    let is_relative = url.starts_with('/') && !url.starts_with("//");
    if is_relative {
        if url.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(ScopeError::Malformed);
        }
        return Ok(());
    }

    // No grant covers an absolute URL — reject before parsing (a malformed
    // absolute URL with no allowance is still simply cross-origin-denied).
    if allowed.is_empty() {
        return Err(ScopeError::CrossOriginDenied);
    }
    let target = url::Url::parse(url).map_err(|_| ScopeError::Malformed)?;
    let permitted = allowed
        .iter()
        .filter_map(|a| url::Url::parse(a).ok())
        .any(|allow| origins_match(&target, &allow));
    if permitted {
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

/// Canonical `scheme://host[:port]` serialisation of a URL's origin, with the
/// scheme's default port dropped (so `https://h` and `https://h:443` collapse to
/// one key). Returns `None` for a URL that does not parse or whose origin is
/// opaque (e.g. `data:`), which are never valid allowlist entries.
fn canonical_origin(s: &str) -> Option<String> {
    let origin = url::Url::parse(s.trim()).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// A per-connecting-origin outbound allowlist for the bridge.
///
/// Maps each **connecting tab origin** (the `Origin` presented at the WebSocket
/// upgrade) to the set of **outbound origins** a request routed to that tab may
/// reach. This is the mechanism behind per-origin scoping: a Grafana tab and a
/// Facebook tab each carry only their own grant, so neither can borrow the
/// other's outbound scope.
///
/// Built from repeatable `--allow-origin` values, each either a bare `ORIGIN`
/// (shorthand: the tab may reach its own origin) or an explicit
/// `CONNECT=OUTBOUND` mapping; repeats accumulate outbound origins under the same
/// connecting key. An empty allowlist is the unconfigured default — the WS gate
/// admits any origin (the session token is the gate) and outbound scope is
/// default-closed (relative URLs only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OriginAllowlist {
    /// Canonical connecting-origin → canonical permitted outbound origins.
    map: BTreeMap<String, BTreeSet<String>>,
}

impl OriginAllowlist {
    /// Parses repeatable `--allow-origin` CLI values into an allowlist.
    ///
    /// Each value is `ORIGIN` (shorthand for `ORIGIN=ORIGIN`) or
    /// `CONNECT=OUTBOUND`. Both sides must be valid, non-opaque origins; a
    /// malformed or empty side is a hard error naming the offending value.
    pub fn parse<S: AsRef<str>>(values: &[S]) -> Result<Self, String> {
        let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for raw in values {
            let raw = raw.as_ref().trim();
            if raw.is_empty() {
                return Err("empty --allow-origin value".to_string());
            }
            let (connect, outbound) = match raw.split_once('=') {
                Some((c, o)) => (c.trim(), o.trim()),
                None => (raw, raw),
            };
            let connect = canonical_origin(connect).ok_or_else(|| {
                format!("invalid --allow-origin connecting origin: {connect:?} (in {raw:?})")
            })?;
            let outbound = canonical_origin(outbound).ok_or_else(|| {
                format!("invalid --allow-origin outbound origin: {outbound:?} (in {raw:?})")
            })?;
            map.entry(connect).or_default().insert(outbound);
        }
        Ok(Self { map })
    }

    /// Whether no `--allow-origin` was configured (the default-open WS gate,
    /// default-closed outbound case).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether a WebSocket upgrade from `origin` is permitted.
    ///
    /// An empty allowlist admits any origin (the token in the subprotocol is the
    /// gate). Otherwise the connecting origin must be a configured key — an
    /// origin-less upgrade is rejected once any entry exists.
    #[must_use]
    pub fn permits_connection(&self, origin: Option<&str>) -> bool {
        if self.map.is_empty() {
            return true;
        }
        origin
            .and_then(canonical_origin)
            .is_some_and(|o| self.map.contains_key(&o))
    }

    /// The outbound origins granted to a request routed to a tab on `origin`.
    ///
    /// Empty when the tab's origin is unknown or carries no grant — leaving only
    /// relative URLs permitted (see [`validate_outbound_url`]).
    #[must_use]
    pub fn outbound_for(&self, origin: Option<&str>) -> Vec<&str> {
        origin
            .and_then(canonical_origin)
            .and_then(|o| self.map.get(&o))
            .map(|set| set.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Renders the configured entries as `CONNECT → OUTBOUND[, OUTBOUND…]` lines
    /// (connecting-origin sorted) for the startup banner. Empty when unconfigured.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        self.map
            .iter()
            .map(|(connect, outbound)| {
                let outbound = outbound.iter().cloned().collect::<Vec<_>>().join(", ");
                format!("{connect} → {outbound}")
            })
            .collect()
    }
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
        assert_eq!(validate_outbound_url("/api/foo", &[]), Ok(()));
        assert_eq!(
            validate_outbound_url("https://evil.test/x", &[]),
            Err(ScopeError::CrossOriginDenied)
        );
        assert_eq!(
            validate_outbound_url("//evil.test/x", &[]),
            Err(ScopeError::CrossOriginDenied)
        );
    }

    #[test]
    fn outbound_scope_honors_allowed_origins() {
        assert_eq!(
            validate_outbound_url("https://ok.test/x", &["https://ok.test"]),
            Ok(())
        );
        assert_eq!(
            validate_outbound_url("https://evil.test/x", &["https://ok.test"]),
            Err(ScopeError::CrossOriginDenied)
        );
        // Any origin in the slice permits the URL.
        assert_eq!(
            validate_outbound_url("https://b.test/x", &["https://a.test", "https://b.test"]),
            Ok(())
        );
        // Relative always allowed regardless of the outbound grant.
        assert_eq!(validate_outbound_url("/x", &["https://ok.test"]), Ok(()));
    }

    #[test]
    fn ws_subprotocol_token_selects_match() {
        assert_eq!(ws_subprotocol_token(["a", "tok", "b"], "tok"), Some("tok"));
        assert_eq!(ws_subprotocol_token(["a", "b"], "tok"), None);
        assert_eq!(ws_subprotocol_token([], "tok"), None);
    }

    #[test]
    fn empty_allowlist_opens_the_ws_gate() {
        let empty = OriginAllowlist::default();
        assert!(empty.is_empty());
        // Token is the gate: any origin (or none) connects.
        assert!(empty.permits_connection(Some("https://anything.test")));
        assert!(empty.permits_connection(None));
        // ...and outbound is default-closed (no origins granted).
        assert!(empty.outbound_for(Some("https://anything.test")).is_empty());
    }

    #[test]
    fn allowlist_gates_connection_by_configured_key() {
        let list = OriginAllowlist::parse(&["https://ok.test"]).unwrap();
        assert!(list.permits_connection(Some("https://ok.test")));
        // Port normalisation: the default https port collapses onto the key.
        assert!(list.permits_connection(Some("https://ok.test:443")));
        assert!(!list.permits_connection(Some("https://evil.test")));
        // An origin-less upgrade is rejected once any entry exists.
        assert!(!list.permits_connection(None));
    }

    #[test]
    fn shorthand_grants_a_tab_its_own_origin() {
        let list = OriginAllowlist::parse(&["https://grafana.internal"]).unwrap();
        assert_eq!(
            list.outbound_for(Some("https://grafana.internal")),
            vec!["https://grafana.internal"]
        );
        // A tab with no matching key carries no outbound grant.
        assert!(list.outbound_for(Some("https://other.test")).is_empty());
        assert!(list.outbound_for(None).is_empty());
    }

    #[test]
    fn mapping_scopes_outbound_per_connecting_origin() {
        // A Grafana tab and a Facebook tab each carry only their own grant.
        let list = OriginAllowlist::parse(&[
            "https://grafana.internal",
            "https://www.facebook.com=https://static.xx.fbcdn.net",
        ])
        .unwrap();
        assert_eq!(
            list.outbound_for(Some("https://grafana.internal")),
            vec!["https://grafana.internal"]
        );
        assert_eq!(
            list.outbound_for(Some("https://www.facebook.com")),
            vec!["https://static.xx.fbcdn.net"]
        );
        // The Grafana tab cannot borrow Facebook's outbound scope.
        assert_eq!(
            validate_outbound_url(
                "https://static.xx.fbcdn.net/x",
                &list.outbound_for(Some("https://grafana.internal"))
            ),
            Err(ScopeError::CrossOriginDenied)
        );
    }

    #[test]
    fn repeated_keys_accumulate_outbound_origins() {
        let list = OriginAllowlist::parse(&[
            "https://app.test=https://a.cdn.test",
            "https://app.test=https://b.cdn.test",
        ])
        .unwrap();
        assert_eq!(
            list.outbound_for(Some("https://app.test")),
            vec!["https://a.cdn.test", "https://b.cdn.test"]
        );
    }

    #[test]
    fn parse_rejects_malformed_and_empty_values() {
        assert!(OriginAllowlist::parse(&["not a url"]).is_err());
        assert!(OriginAllowlist::parse(&["https://ok.test=nonsense"]).is_err());
        assert!(OriginAllowlist::parse(&[""]).is_err());
        assert!(OriginAllowlist::parse(&["=https://ok.test"]).is_err());
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

    // ── token resolution from env / file ─────────────────────────────
    //
    // `OMNI_BRIDGE_TOKEN` is read through an injected `EnvSource`, so these
    // tests pass a pure `MapEnv` and never touch the process environment —
    // no lock, fully parallel (issue #1030).

    use crate::test_support::env::MapEnv;

    #[test]
    fn resolve_token_reads_trimmed_env_var() {
        let env = MapEnv::new().with(TOKEN_ENV, "  env-token  ");
        assert_eq!(resolve_token_with(&env, None).unwrap(), "env-token");
    }

    #[test]
    fn resolve_token_generates_when_env_empty_or_absent() {
        // Absent → freshly generated (long, URL-safe).
        let a = resolve_token_with(&MapEnv::new(), None).unwrap();
        assert!(a.len() >= 40);
        // Blank/whitespace env is ignored and also generates.
        let env = MapEnv::new().with(TOKEN_ENV, "   ");
        let b = resolve_token_with(&env, None).unwrap();
        assert!(b.len() >= 40);
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_existing_token_reads_env_var() {
        let env = MapEnv::new().with(TOKEN_ENV, "client-token");
        assert_eq!(
            resolve_existing_token_with(&env, None).unwrap(),
            "client-token"
        );
    }

    #[test]
    fn resolve_existing_token_errors_without_source() {
        let err = resolve_existing_token_with(&MapEnv::new(), None).unwrap_err();
        assert!(err.to_string().contains(TOKEN_ENV));
    }

    #[test]
    fn resolve_existing_token_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tok");
        std::fs::write(&path, "  file-token\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(resolve_existing_token(Some(&path)).unwrap(), "file-token");
    }

    #[test]
    fn token_file_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        assert!(resolve_token(Some(&path)).is_err());
    }

    #[test]
    fn token_file_empty_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        std::fs::write(&path, "   \n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = resolve_token(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
