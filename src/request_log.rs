//! Append-only, local invocation + HTTP request log (`log.jsonl`).
//!
//! Every `omni-dev` invocation appends one `kind: "invocation"` line; every
//! outbound HTTP request made by one of the integration clients appends one
//! `kind: "http"` line correlated to it by a shared `invocation_id`. The log
//! is **local-machine state** written under the platform state/data directory
//! (`0700` dir / `0600` file, the same posture as [`crate::daemon::paths`]).
//!
//! Design invariants:
//!
//! - **Best effort.** [`record`] swallows every error (logging only at
//!   `tracing::debug`); a logging failure can never change the program's exit
//!   code. Honors `OMNI_DEV_LOG_DISABLE=1` for an absolute opt-out of this
//!   log only. The separate `audit.jsonl` (written by the Drive lease audit
//!   trail before it moved to gwi, #2203) is never rotated, pruned or disabled
//!   by it; omni-dev still reads that file but no longer writes it.
//! - **No secrets.** Auth headers/tokens are never written; only a non-secret
//!   `auth_principal` identity is kept. Headers are redacted centrally
//!   ([`redact_headers`]), secret-bearing URL query/fragment parameter values
//!   are redacted (`redact_url`) before writing, and request/response bodies
//!   are opt-in via `OMNI_DEV_LOG_BODIES=1`.
//! - **Forward compatible.** A single [`LogRecord`] is used for both writing
//!   and reading: every field is `#[serde(default)]`, and every optional field
//!   is `skip_serializing_if`, so a newer reader never chokes on an older line
//!   and an older reader never chokes on a newer one — the same forward-rolling
//!   contract the daemon wire types use.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::utils::env::{non_empty_var, truthy_var, EnvSource, SystemEnv};

/// Default log file name under the runtime directory.
const LOG_FILE_NAME: &str = "log.jsonl";

/// Default audit log file name under the runtime directory — a sibling of
/// [`LOG_FILE_NAME`] ([ADR-0080](../docs/adrs/adr-0080.md) §11). Distinctness
/// from the default `log.jsonl` path is enforced by
/// [`resolves_to_audit_file`], not merely implied by having a different
/// constant. omni-dev no longer writes this file (its only writer, the Drive
/// lease audit trail, moved to gwi, #2203); it still reads, and refuses to
/// rotate or prune, what an earlier version left there.
const AUDIT_FILE_NAME: &str = "audit.jsonl";

/// Number of rotated log files kept by default when
/// [`OMNI_DEV_LOG_MAX_SIZE`](rotation_config) enables rotation but
/// `OMNI_DEV_LOG_KEEP_FILES` is unset. (Rotation on write is unix-only.)
#[cfg(unix)]
const DEFAULT_KEEP_FILES: u32 = 3;

/// Which kind of record a line holds. Unknown future kinds deserialize to
/// [`RecordKind::Unknown`] rather than failing the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// One per process invocation (or per MCP tool call).
    #[default]
    Invocation,
    /// One per outbound HTTP request.
    Http,
    /// One per `gh` CLI subprocess invocation (the only path to the GitHub API;
    /// the token never enters our process, so these are subprocess records, not
    /// [`RecordKind::Http`]). See `crate::github_metrics`.
    Gh,
    /// One per wrapped `git worktree` subprocess invocation, carrying
    /// recovery-relevant metadata (path/branch/commit) in `context`.
    /// See `crate::cli::git` worktree subcommands.
    Worktree,
    /// One per Drive/Sheets/Docs content-mutating attempt. **Read-only legacy
    /// kind**: omni-dev no longer writes it (the Drive commands moved to
    /// [gwi](https://github.com/rust-works/gwi), #2203), but `omni-dev log`
    /// still decodes and renders the records an earlier version left in
    /// `log.jsonl`.
    DriveMutation,
    /// One per leased-write lifecycle event, from the separate `audit.jsonl`
    /// ([ADR-0080](../docs/adrs/adr-0080.md) §11). **Read-only legacy kind**:
    /// omni-dev no longer writes it (its only writer, the Drive lease audit
    /// trail, moved to gwi, #2203), but `omni-dev log --audit` still reads the
    /// file an earlier version left behind. See [`audit_file_path`].
    Audit,
    /// A kind written by a newer version that this reader does not know.
    #[serde(other)]
    Unknown,
}

impl RecordKind {
    /// Stable lowercase name, used for display and JSON map keys (matches the
    /// `serde(rename_all = "lowercase")` wire form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invocation => "invocation",
            Self::Http => "http",
            Self::Gh => "gh",
            Self::Worktree => "worktree",
            Self::DriveMutation => "drivemutation",
            Self::Audit => "audit",
            Self::Unknown => "unknown",
        }
    }
}

/// What drove an invocation. Unknown future sources deserialize to
/// [`Source::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A direct `omni-dev` CLI invocation.
    #[default]
    Cli,
    /// An `omni-dev-mcp` tool call.
    Mcp,
    /// Work performed inside the long-lived daemon process.
    Daemon,
    /// A source written by a newer version that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// One line of the log. Used for both writing and reading; every field is
/// `#[serde(default)]` (tolerant reads) and every optional field is
/// `skip_serializing_if` (compact, forward-compatible writes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogRecord {
    // --- Core fields (present on every record) ---
    /// Per-record, time-sortable id (see [`new_id`]).
    #[serde(default)]
    pub id: String,
    /// Shared by an invocation record and every HTTP record it spawned.
    #[serde(default)]
    pub invocation_id: String,
    /// Discriminates the record type.
    #[serde(default)]
    pub kind: RecordKind,
    /// RFC3339 timestamp with milliseconds.
    #[serde(default)]
    pub timestamp: String,
    /// Host the record was written on.
    #[serde(default)]
    pub hostname: String,
    /// Writing process id.
    #[serde(default)]
    pub pid: u32,
    /// `omni-dev` version that wrote the record.
    #[serde(default)]
    pub omni_dev_version: String,
    /// Working directory at write time.
    #[serde(default)]
    pub cwd: String,
    /// OS user that owns the process.
    #[serde(default)]
    pub system_user: String,

    // --- `kind: "invocation"` fields ---
    /// Resolved clap subcommand path, e.g. `["jira","read"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Full argv.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_line: Vec<String>,
    /// Process exit code (0 success, 1 error — matches `die`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Wall time of the whole invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Whitelisted, non-secret `OMNI_DEV_*` env snapshot.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What drove the run (`cli`/`mcp`/`daemon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// When `source = mcp`, the tool name that drove the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_tool: Option<String>,

    // --- `kind: "http"` fields ---
    /// Coarse service tag (`jira`/`confluence`/`datadog`/…) for fast filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// HTTP method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Request URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Response status; absent on a network/transport error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// Elapsed time of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// True when the request ran inside the daemon (bridge/Snowflake pool).
    #[serde(default, skip_serializing_if = "is_false")]
    pub via_daemon: bool,
    /// Which pooled daemon session served the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_session_id: Option<String>,
    /// Non-secret identity actually used (token id / OAuth principal) — never
    /// the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_principal: Option<String>,
    /// Redacted request headers (only when `OMNI_DEV_LOG_HEADERS=1`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_headers: BTreeMap<String, String>,
    /// Redacted response headers (only when `OMNI_DEV_LOG_HEADERS=1`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response_headers: BTreeMap<String, String>,
    /// Request body (only when `OMNI_DEV_LOG_BODIES=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// Response body (only when `OMNI_DEV_LOG_BODIES=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Free-form correlation tags.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,

    // --- shared optional ---
    /// Top-level error chain (invocation) or per-request error (http).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `skip_serializing_if` predicate for `bool` fields that default to `false`.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires `fn(&T) -> bool`
fn is_false(b: &bool) -> bool {
    !*b
}

impl LogRecord {
    /// Builds a record carrying only the always-present core fields.
    fn new(kind: RecordKind, invocation_id: String) -> Self {
        Self {
            id: new_id(),
            invocation_id,
            kind,
            timestamp: now_rfc3339_millis(),
            hostname: hostname(),
            pid: std::process::id(),
            omni_dev_version: crate::VERSION.to_string(),
            cwd: cwd(),
            system_user: system_user(),
            ..Self::default()
        }
    }
}

/// The per-invocation context every record is stamped with.
///
/// Held once per process in [`GLOBAL`] (CLI/daemon) and overridden per task in
/// [`CTX`] (the multiplexed MCP server), so HTTP records can find their parent
/// invocation without threading state through every call site.
#[derive(Debug, Clone)]
pub struct RequestLogContext {
    /// Shared id linking an invocation to the HTTP it spawned.
    pub invocation_id: String,
    /// What drove the run.
    pub source: Source,
    /// MCP tool name when `source = mcp`.
    pub mcp_tool: Option<String>,
}

impl Default for RequestLogContext {
    fn default() -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Cli,
            mcp_tool: None,
        }
    }
}

impl RequestLogContext {
    /// A CLI context with a freshly minted invocation id.
    pub fn cli() -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Cli,
            mcp_tool: None,
        }
    }

    /// An MCP context for a single tool call.
    pub fn mcp(tool: impl Into<String>) -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Mcp,
            mcp_tool: Some(tool.into()),
        }
    }
}

static GLOBAL: OnceLock<RequestLogContext> = OnceLock::new();

tokio::task_local! {
    /// Per-task context override, set around each MCP tool dispatch.
    pub static CTX: RequestLogContext;
}

/// Installs the process-global context. The first call wins (the CLI/daemon
/// shell sets it once, very early); later calls are ignored.
pub fn set_global(ctx: RequestLogContext) {
    let _ = GLOBAL.set(ctx);
}

/// Resolves the active context: task-local override first, then the
/// process-global default, then a synthesized fallback.
pub fn current_context() -> RequestLogContext {
    if let Ok(ctx) = CTX.try_with(RequestLogContext::clone) {
        return ctx;
    }
    if let Some(ctx) = GLOBAL.get() {
        return ctx.clone();
    }
    RequestLogContext::default()
}

/// Runs `fut` with the active context's `invocation_id` replaced by
/// `origin_id`, preserving `source` and `mcp_tool`.
///
/// The daemon and the browser bridge scope this around a request they serve on
/// behalf of a CLI/MCP client, so the HTTP records that request spawns
/// correlate to the *originating* invocation rather than the server's own
/// (#1198). `source` is deliberately preserved: a request served inside the
/// daemon keeps `source = Daemon`, so `via_daemon` detection is unaffected while
/// `invocation_id` now points at the caller's invocation record.
pub async fn scope_origin_id<F, T>(origin_id: String, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let mut ctx = current_context();
    ctx.invocation_id = origin_id;
    CTX.scope(ctx, fut).await
}

/// Whether logging is disabled entirely (`OMNI_DEV_LOG_DISABLE=1`).
pub fn disabled() -> bool {
    disabled_with(&SystemEnv)
}

/// [`disabled`], reading through an injected [`EnvSource`] (STYLE-0028).
fn disabled_with(env: &impl EnvSource) -> bool {
    truthy_var(env, "OMNI_DEV_LOG_DISABLE")
}

/// Whether request/response bodies may be recorded (`OMNI_DEV_LOG_BODIES=1`).
pub fn bodies_enabled() -> bool {
    bodies_enabled_with(&SystemEnv)
}

/// [`bodies_enabled`], reading through an injected [`EnvSource`] (STYLE-0028).
fn bodies_enabled_with(env: &impl EnvSource) -> bool {
    truthy_var(env, "OMNI_DEV_LOG_BODIES")
}

/// Whether (redacted) headers may be recorded (`OMNI_DEV_LOG_HEADERS=1`).
pub fn headers_enabled() -> bool {
    truthy_var(&SystemEnv, "OMNI_DEV_LOG_HEADERS")
}

/// The default location of an omni-dev runtime file: `state_dir` (falling
/// back to `data_dir`) joined with `omni-dev/<component>`. `pub(crate)`
/// rather than private so other runtime files can resolve their location the
/// same way and share this rather than re-deriving it.
pub(crate) fn omni_dev_state_subpath(component: &str) -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(base.join("omni-dev").join(component))
}

/// Resolves the log file path: `OMNI_DEV_LOG_FILE` override, else
/// `state_dir` (falling back to `data_dir`) joined with `omni-dev/log.jsonl`.
pub fn log_file_path() -> Option<PathBuf> {
    log_file_path_with(&SystemEnv)
}

/// [`log_file_path`], reading through an injected [`EnvSource`]
/// (STYLE-0028). `pub(crate)` rather than private: `cli::log`'s
/// `LogCommand::resolve_path_with` needs it for its own env-injected test.
///
/// Refuses (returns `None`) when the resolved path names the same file as
/// [`audit_file_path_with`] — an `OMNI_DEV_LOG_FILE` misconfiguration that
/// would otherwise let ordinary best-effort records
/// (`try_record`/`append_with_rotation`) and `prune` reach the fail-closed
/// audit sink, since none of those consumers get a chance to guard against
/// it themselves once a colliding path has already resolved
/// ([#1747](https://github.com/rust-works/omni-dev/issues/1747), ADR-0080
/// §11). Centralizing the refusal here — rather than relying on each
/// consumer's own check, as `prune` and `append_with_rotation` still do for
/// defense in depth against a caller that bypasses this resolver — means
/// every future consumer of [`log_file_path`] is covered automatically.
pub(crate) fn log_file_path_with(env: &impl EnvSource) -> Option<PathBuf> {
    let path = non_empty_var(env, "OMNI_DEV_LOG_FILE")
        .map(PathBuf::from)
        .or_else(|| omni_dev_state_subpath(LOG_FILE_NAME))?;
    if resolves_to_audit_file_with(&path, env) {
        tracing::warn!(
            "request_log: OMNI_DEV_LOG_FILE resolves to the audit log ({}); refusing to use it \
             as the request log path — set OMNI_DEV_LOG_FILE and/or OMNI_DEV_AUDIT_LOG_FILE to \
             distinct paths",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// Resolves the audit log file path.
///
/// `OMNI_DEV_AUDIT_LOG_FILE` override, else `state_dir` (falling back to
/// `data_dir`) joined with `omni-dev/audit.jsonl` — a sibling of
/// [`log_file_path`]'s default, resolved through the same two helpers so
/// the two sinks' policy can't drift apart. Since either can be redirected
/// independently via its own env override, nothing here *guarantees* the
/// two stay distinct; [`resolves_to_audit_file`] checks that wherever omni-dev
/// would rotate or prune a path instead of merely assuming it.
///
/// The body is the same in every build; only `test_audit_file_override`
/// differs. In a release build it is always `None`, so the env override
/// and the real default are all there is. In a test build it is pinned to a
/// shared scratch file, so no test can read or pollute the machine's real
/// `audit.jsonl` by omission.
pub fn audit_file_path() -> Option<PathBuf> {
    test_audit_file_override().or_else(|| audit_file_path_with(&SystemEnv))
}

/// The pure override-or-default half of [`audit_file_path`]: honors
/// `OMNI_DEV_AUDIT_LOG_FILE` through an injected [`EnvSource`] (STYLE-0028),
/// deliberately **not** consulting `TEST_AUDIT_ROUTE` — that per-thread
/// routing is a test-build-only safety net for the *ambient* entry point,
/// orthogonal to this function's own override-or-default algorithm.
/// `pub(crate)` rather than private: `cli::log`'s `LogCommand::resolve_path_with`
/// needs it for its own env-injected test.
pub(crate) fn audit_file_path_with(env: &impl EnvSource) -> Option<PathBuf> {
    non_empty_var(env, "OMNI_DEV_AUDIT_LOG_FILE")
        .map(PathBuf::from)
        .or_else(default_audit_file_path)
}

/// Collapses `.`/`..` components **syntactically**, with no filesystem
/// access — so `a/sub/../b` becomes `a/b` even when `sub` does not exist on
/// disk. [`normalize`] needs this pass before it ever calls
/// [`std::fs::canonicalize`]: canonicalizing a parent directory (its
/// fallback for a path that doesn't exist yet) itself requires that parent
/// to exist, and a `..` segment routed through a nonexistent intermediate
/// directory would otherwise never resolve — exactly the `..`-spelling
/// [`same_file()`] exists to catch. A leading `..` (or one immediately after
/// a root/prefix) has nothing to pop, so it is kept as-is.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Bounds how many symlinks [`normalize`] follows by hand, purely so a
/// symlink cycle can't loop forever; a real cycle already fails
/// `canonicalize` and is vanishingly unlikely to matter here, but the
/// budget makes termination structural rather than assumed.
const MAX_HAND_FOLLOWED_SYMLINK_HOPS: u32 = 8;

/// Resolves `path` as close to a canonical form as possible without
/// requiring it to exist. Lexically collapses `.`/`..` first
/// ([`lexically_normalize`]), then tries [`std::fs::canonicalize`] (an
/// existing file, with every symlink in the chain followed); a target that
/// does not exist yet fails that outright, so a symlink is then followed by
/// hand — load-bearing for a `OMNI_DEV_LOG_FILE` symlink whose target
/// `audit.jsonl` has not been created yet, since a plain "canonicalize the
/// parent" would otherwise compare the link's own name against the target's
/// and answer "different" right up until the first write creates the target
/// through the link. Falls back to canonicalizing the parent and rejoining
/// the file name, and finally to the lexically-normalized path unchanged
/// when even that has nothing to canonicalize (`file_name()` is `None` for
/// `/`, `.`, a bare `..`, …).
///
/// This only exists for [`same_file()`]'s not-yet-existing-target fallback —
/// when both paths already exist, `same_file::is_same_file` handles
/// symlinks (and Windows file identity) itself and this is never reached.
fn normalize(path: &Path) -> PathBuf {
    normalize_with_budget(&lexically_normalize(path), MAX_HAND_FOLLOWED_SYMLINK_HOPS)
}

/// The recursive body of [`normalize`]. `path` is always already lexically
/// normalized on entry (both the initial call and the recursive one below
/// maintain that). Each `if let Ok(..)` below falls through to the next,
/// weaker resolution strategy on any error — expected and unremarkable here
/// (`NotFound` is the common case, since this only runs once
/// `same_file::is_same_file` has already failed to resolve one side), so
/// none of them are worth logging.
fn normalize_with_budget(path: &Path, hops: u32) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    if hops > 0 {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                if let Ok(target) = std::fs::read_link(path) {
                    let resolved = if target.is_absolute() {
                        target
                    } else {
                        path.parent()
                            .filter(|p| !p.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."))
                            .join(target)
                    };
                    return normalize_with_budget(&lexically_normalize(&resolved), hops - 1);
                }
            }
        }
    }
    let Some(file_name) = path.file_name() else {
        return path.to_path_buf();
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match std::fs::canonicalize(parent) {
        Ok(canon_parent) => canon_parent.join(file_name),
        Err(_) => path.to_path_buf(),
    }
}

/// Whether `a` and `b` name the same file, robust to `..` segments,
/// relative-vs-absolute spellings, and symlinks — unlike a raw `PathBuf`
/// compare, which all three pass straight through.
///
/// Delegates to the [`same_file`](mod@same_file) crate's `is_same_file`
/// first — real file-identity comparison (inode on unix, a file handle on
/// Windows), rather than a hand-rolled `(dev, ino)` check that would have
/// no non-unix equivalent. That call requires both paths to exist, though,
/// so it fails for the case that actually motivated this function: a
/// `OMNI_DEV_LOG_FILE` symlink whose target `audit.jsonl` has not been
/// created yet. [`normalize`] is the fallback for exactly that gap.
fn same_file(a: &Path, b: &Path) -> bool {
    if let Ok(same) = same_file::is_same_file(a, b) {
        return same;
    }
    normalize(a) == normalize(b)
}

/// Whether `path` resolves to the same file as [`audit_file_path`] — the
/// check [`prune`] and [`append_with_rotation`] refuse on, so neither
/// destructive operation can be pointed at the fail-closed audit sink by a
/// `..`-spelled, relative, or symlinked `OMNI_DEV_LOG_FILE` (ADR-0080 §11).
///
/// Both callers check this once, before reading/rewriting `path`, not
/// through a handle held across the whole operation — a symlink at `path`
/// repointed to the audit file in the narrow window between this check and
/// the subsequent read still slips through (an attacker able to time that
/// swap already needs write access to the log directory, i.e. this same
/// user's own account). This narrows that window, it does not close it —
/// the same "narrows, does not close" shape as
/// [`crate::daemon::paths::try_lock_file_exclusive`]'s own inode re-check.
fn resolves_to_audit_file(path: &Path) -> bool {
    audit_file_path().is_some_and(|audit| same_file(path, &audit))
}

/// [`resolves_to_audit_file`], reading the audit path through an injected
/// [`EnvSource`] instead of the ambient [`audit_file_path`] — the
/// [`log_file_path_with`] guard needs to compare against
/// [`audit_file_path_with`] (same env, same test seam), not the ambient
/// function's per-thread test override, which is orthogonal to this
/// comparison. Deliberately does **not** short-circuit via `?` on a
/// non-resolving audit path: an audit path that can't be resolved at all
/// means there is nothing to collide with, not that the request-log path
/// should be refused.
fn resolves_to_audit_file_with(path: &Path, env: &impl EnvSource) -> bool {
    audit_file_path_with(env).is_some_and(|audit| same_file(path, &audit))
}

/// Release builds have no per-thread override.
#[cfg(not(test))]
fn test_audit_file_override() -> Option<PathBuf> {
    None
}

/// Test builds resolve the audit file to the scratch default instead of the
/// real machine's, so a test that reads [`audit_file_path`] (the `log --audit`
/// tests) can never touch a developer's actual `audit.jsonl`. A test that wants
/// to observe `OMNI_DEV_AUDIT_LOG_FILE` honoring an override calls
/// [`audit_file_path_with`] directly with an injected [`EnvSource`] instead
/// (STYLE-0028).
#[cfg(test)]
fn test_audit_file_override() -> Option<PathBuf> {
    default_audit_file_path()
}

/// Where [`audit_file_path`] lands when `OMNI_DEV_AUDIT_LOG_FILE` is unset.
#[cfg(not(test))]
fn default_audit_file_path() -> Option<PathBuf> {
    omni_dev_state_subpath(AUDIT_FILE_NAME)
}

/// Test-build variant of the function above: one shared scratch path for
/// the whole test binary's process lifetime, never the real machine's
/// default. The `tempdir` is deliberately never cleaned up (`mem::forget`) —
/// a `cargo test` process is short-lived, and the alternative is writing
/// into a developer's or CI runner's actual `audit.jsonl`.
#[cfg(test)]
#[allow(clippy::expect_used)]
fn default_audit_file_path() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    Some(
        PATH.get_or_init(|| {
            let dir =
                tempfile::tempdir().expect("failed to create a scratch dir for test audit logs");
            let path = dir.path().join(AUDIT_FILE_NAME);
            std::mem::forget(dir);
            path
        })
        .clone(),
    )
}

/// Appends one record. Best effort: every error is swallowed (logged at
/// `tracing::debug`) so logging can never affect the caller's exit code.
pub fn record(entry: &LogRecord) {
    if disabled() {
        return;
    }
    if let Err(e) = try_record(entry) {
        tracing::debug!("request_log: failed to append record: {e}");
    }
}

/// The fallible append used by [`record`]; all errors flow back to be swallowed.
///
/// Refuses a [`RecordKind::Audit`] entry outright: that kind belongs to the
/// separate `audit.jsonl` (no longer written by omni-dev, #2203), and routing
/// it through this best-effort path would subject a forensic record to
/// `OMNI_DEV_LOG_DISABLE`, rotation and `prune`.
fn try_record(entry: &LogRecord) -> anyhow::Result<()> {
    use anyhow::{ensure, Context};

    ensure!(
        entry.kind != RecordKind::Audit,
        "an audit-kind record does not belong in log.jsonl; omni-dev no longer writes audit.jsonl"
    );
    let path = log_file_path().context("could not resolve the log file path")?;
    append_record_to(&path, entry, append_line)
}

/// Serializes `entry` and appends it to `path` via `append`, creating a
/// missing `0700` parent directory first. [`try_record`] passes
/// [`append_line`], which opts into rotation when configured.
fn append_record_to(
    path: &Path,
    entry: &LogRecord,
    append: fn(&Path, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use anyhow::Context;

    // Only create and tighten the parent when it's missing — re-`chmod`ing an
    // existing dir (e.g. a user-chosen OMNI_DEV_LOG_FILE location, or a shared
    // temp dir) is both wrong and may fail; the file itself is always 0600.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            crate::daemon::paths::ensure_dir_0700(parent)?;
        }
    }
    let mut line = serde_json::to_string(entry).context("failed to serialize record")?;
    line.push('\n');
    append(path, &line)?;
    Ok(())
}

/// Appends a single line with `O_APPEND | O_CREATE`, creating the file `0600`.
/// A pre-existing looser-perm file (an older version's, or a user-set
/// `OMNI_DEV_LOG_FILE` target) is re-tightened to `0600` on every open, via
/// the handle so there is no path race (#1139).
/// When bodies are enabled (lines may exceed the atomic-write size) an advisory
/// exclusive lock guards the write; the common no-body path relies on
/// `O_APPEND` single-write atomicity and takes no lock.
///
/// `log.jsonl`'s own writer — opts into size-capped rotation
/// ([`rotation_config`]) when configured.
#[cfg(unix)]
fn append_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    // Opt-in size-capped rotation takes over the write: it must stat, maybe
    // rotate, then open a fresh file, all under a stable-path lock (#1121).
    if let Some(cfg) = rotation_config() {
        return append_with_rotation(path, line, &cfg);
    }
    append_line_no_rotation(path, line)
}

/// The same append as [`append_line`], minus the `OMNI_DEV_LOG_MAX_SIZE`
/// rotation check — used by [`append_line`] itself once rotation is confirmed
/// inactive, and by the rotation path once it holds the lock.
#[cfg(unix)]
fn append_line_no_rotation(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    crate::daemon::paths::ensure_handle_0600(&file)?;

    if bodies_enabled() {
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive) {
            Ok(mut guard) => guard.write_all(line.as_bytes())?,
            Err((mut file, _)) => file.write_all(line.as_bytes())?,
        }
    } else {
        let mut file = file;
        file.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Non-unix fallback: `O_APPEND | O_CREATE` single write, no advisory lock and
/// no mode tightening (those are unix concepts). Size-capped rotation is a
/// unix-only feature and is not applied here.
#[cfg(not(unix))]
fn append_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

// --- Size management: rotation on write + `omni-dev log prune` ---
//
// The log is default-on for every invocation and every outbound request, so on
// an active machine it would otherwise grow without bound (#1121). Two bounds
// are offered, both opt-in:
//
//   * Automatic size-capped rotation on write, gated on `OMNI_DEV_LOG_MAX_SIZE`
//     (+ `OMNI_DEV_LOG_KEEP_FILES`) — numbered `log.jsonl.1`, `.2`, … files.
//   * The explicit `omni-dev log prune` command (age- and/or size-based), which
//     rewrites the file in place via a same-dir temp file + atomic rename.

/// Returns `path` with `suffix` appended to its final component (kept in the
/// same directory), e.g. `…/log.jsonl` + `.1` → `…/log.jsonl.1`.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The directory a log's siblings live in. A bare file name has an empty parent,
/// which `read_dir` rejects, so it is the current directory.
fn sibling_dir(path: &Path) -> Option<&Path> {
    let dir = path.parent()?;
    Some(if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    })
}

/// The entry `read_dir` yielded, or `None` for one it could not read, logged at
/// debug: that entry is skipped, and the rest of the directory is still listed.
fn readable_entry(
    entry: std::io::Result<std::fs::DirEntry>,
    dir: &Path,
) -> Option<std::fs::DirEntry> {
    entry
        .inspect_err(|e| {
            let shown = dir.display();
            tracing::debug!("request_log: skipping an unreadable entry of {shown}: {e}");
        })
        .ok()
}

/// The rotated siblings of the log at `path` that exist now: `<path>.<N>` for a
/// decimal `N` of at least 1, in ascending order of `N` — newest first, since
/// [`rotate`] shifts every file up and starts the numbering at `.1`.
///
/// Lists what is on disk rather than counting up to `OMNI_DEV_LOG_KEEP_FILES`,
/// because that variable is read per write by whichever process writes, so a
/// reader cannot know the value the rotating writer used. Only what rotation
/// itself writes counts: a regular file whose suffix is the canonical decimal
/// number, so `.lock`, an editor's `.1.bak`, `.0`, `.01` and a directory called
/// `.3` are not rotated files. A missing directory has none; one that cannot be
/// listed has none either, logged at debug because the caller then rebuilds
/// instead of following a rotation.
pub(crate) fn rotated_files(path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (sibling_dir(path), path.file_name()) else {
        return Vec::new();
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                let shown = dir.display();
                tracing::debug!("request_log: cannot list {shown} for rotated files: {e}");
            }
            return Vec::new();
        }
    };
    let mut prefix = name.to_owned();
    prefix.push(".");
    let prefix = prefix.to_string_lossy();
    let mut rotated: Vec<(u32, PathBuf)> = entries
        .filter_map(|entry| readable_entry(entry, dir))
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let suffix = file_name
                .to_string_lossy()
                .strip_prefix(&*prefix)?
                .to_owned();
            let number: u32 = suffix.parse().ok()?;
            let canonical = number >= 1 && number.to_string() == suffix;
            let regular = entry.file_type().is_ok_and(|kind| kind.is_file());
            (canonical && regular).then(|| (number, entry.path()))
        })
        .collect();
    rotated.sort_by_key(|&(number, _)| number);
    rotated.into_iter().map(|(_, path)| path).collect()
}

/// Parses a human byte size: a number (with optional decimal) and an optional
/// unit suffix — `b` (bytes, the default), `k`/`kb`/`kib`, `m`/`mb`/`mib`,
/// `g`/`gb`/`gib` (case-insensitive, all binary/1024-based).
pub(crate) fn parse_size(s: &str) -> anyhow::Result<u64> {
    use anyhow::Context as _;

    let lower = s.trim().to_ascii_lowercase();
    if lower.is_empty() {
        anyhow::bail!("empty size (expected e.g. 10mb, 512kb, 1048576)");
    }
    let split = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (num, unit) = lower.split_at(split);
    let value: f64 = num
        .parse()
        .with_context(|| format!("invalid size number: {s}"))?;
    if !value.is_finite() || value < 0.0 {
        anyhow::bail!("invalid size: {s}");
    }
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => anyhow::bail!("invalid size unit: {other} (use b, kb, mb, or gb)"),
    };
    Ok((value * mult as f64) as u64)
}

/// Resolved rotation policy from the environment. `None` means rotation is off
/// (the default): `OMNI_DEV_LOG_MAX_SIZE` unset, empty, invalid, or `0`.
/// Rotation on write is a unix-only feature.
#[cfg(unix)]
struct RotationConfig {
    /// Rotate before an append that would push the file past this many bytes.
    max_size: u64,
    /// Number of rotated `log.jsonl.N` files to retain.
    keep_files: u32,
}

/// Reads the rotation policy from `OMNI_DEV_LOG_MAX_SIZE` /
/// `OMNI_DEV_LOG_KEEP_FILES`. A set-but-invalid `OMNI_DEV_LOG_MAX_SIZE` logs at
/// debug and disables rotation rather than failing the write.
#[cfg(unix)]
fn rotation_config() -> Option<RotationConfig> {
    let raw = std::env::var("OMNI_DEV_LOG_MAX_SIZE").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let max_size = match parse_size(&raw) {
        Ok(0) => return None,
        Ok(n) => n,
        Err(e) => {
            tracing::debug!("request_log: ignoring invalid OMNI_DEV_LOG_MAX_SIZE: {e}");
            return None;
        }
    };
    let keep_files = std::env::var("OMNI_DEV_LOG_KEEP_FILES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_KEEP_FILES);
    Some(RotationConfig {
        max_size,
        keep_files,
    })
}

/// Rotates `log.jsonl` → `log.jsonl.1`, shifting existing numbered files up and
/// dropping any beyond `keep_files` (`keep_files == 0` simply discards the
/// current file). Rotated files inherit the `0600` mode of their source.
///
/// Only renames: a file keeps its `(device, inode)` as it moves up, which is how
/// the github counters follow their position across a rotation
/// ([`rotated_files`]). `pub(crate)` so their tests rotate with this function
/// instead of a copy of it.
#[cfg(unix)]
pub(crate) fn rotate(path: &Path, keep_files: u32) -> anyhow::Result<()> {
    if keep_files == 0 {
        // Retain no history: dropping the current file lets the caller start a
        // fresh one on the following append.
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    // Drop the oldest retained file, then shift .(N-1) → .N … .1 → .2.
    let _ = std::fs::remove_file(sibling(path, &format!(".{keep_files}")));
    for i in (1..keep_files).rev() {
        let from = sibling(path, &format!(".{i}"));
        if from.exists() {
            std::fs::rename(&from, sibling(path, &format!(".{}", i + 1)))?;
        }
    }
    std::fs::rename(path, sibling(path, ".1"))?;
    Ok(())
}

/// Size-capped append (unix): under an exclusive lock on a stable `<log>.lock`
/// file — so all rotation-aware writers serialize on an inode that is never
/// itself rotated — stat the log, rotate if this line would push a non-empty
/// file past the cap, then append to the (possibly fresh) file. A rotation
/// failure is logged at debug and the line is still appended (best effort).
///
/// Never rotates when `path` [resolves to the audit
/// file](resolves_to_audit_file) — checked first, before the `<path>.lock`
/// sibling below is created next to it — falling back to the plain
/// never-rotates append instead (ADR-0080 §11). The line is still appended:
/// this only withholds rotation, matching [`record`]'s best-effort, always
/// try to write it contract.
#[cfg(unix)]
fn append_with_rotation(path: &Path, line: &str, cfg: &RotationConfig) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if resolves_to_audit_file(path) {
        return append_line_no_rotation(path, line);
    }

    let lock_path = sibling(path, ".lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    crate::daemon::paths::ensure_handle_0600(&lock_file)?;
    // Hold the lock for the whole check-rotate-append. If the lock cannot be
    // taken, fall through unlocked rather than dropping the record.
    let _guard = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive).ok();

    let current = std::fs::metadata(path).map_or(0, |m| m.len());
    if current > 0 && current.saturating_add(line.len() as u64) > cfg.max_size {
        if let Err(e) = rotate(path, cfg.keep_files) {
            tracing::debug!("request_log: rotation failed, appending without rotating: {e}");
        }
    }

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    crate::daemon::paths::ensure_handle_0600(&file)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Options controlling [`prune`].
pub struct PruneOptions {
    /// Drop records whose timestamp is strictly older than this cutoff. A
    /// record with a missing/unparseable timestamp (or a malformed line) is
    /// conservatively kept.
    pub older_than: Option<DateTime<Utc>>,
    /// After age pruning, drop the oldest records until the file is at most
    /// this many bytes. At least the single most recent record is always kept.
    pub max_size: Option<u64>,
    /// Compute and report the outcome without modifying the file.
    pub dry_run: bool,
}

/// What a [`prune`] run did (or, when `dry_run`, would do).
pub struct PruneOutcome {
    /// Records removed.
    pub removed: usize,
    /// Records retained.
    pub kept: usize,
    /// File size before.
    pub bytes_before: u64,
    /// File size after (the size the retained records occupy).
    pub bytes_after: u64,
}

/// Prunes the log at `path` by age and/or size, rewriting it in place.
///
/// Non-empty lines are retained by two successive filters: age (`older_than`)
/// then size (`max_size`, keeping the most recent records that fit). The kept
/// lines are written to a same-directory temp file (`0600` on unix) and
/// atomically renamed over the original, so a reader never sees a half-written
/// file. A missing log is a no-op; a no-change prune skips the rewrite (leaving
/// the file's inode — and any concurrent appends — untouched).
///
/// Refuses outright when `path` [resolves to the audit file](resolves_to_audit_file)
/// — regardless of who computed `path`, since `--audit`'s own refusal
/// (`src/cli/log/prune.rs`) only catches that one flag, not a
/// `OMNI_DEV_LOG_FILE` override spelled to name `audit.jsonl` directly, via
/// `..`, or via a symlink. Exemption from pruning is the point (ADR-0080
/// §11); this applies even under `dry_run`, since `omni-dev log --audit` is
/// the supported way to read the file.
pub fn prune(path: &Path, opts: &PruneOptions) -> anyhow::Result<PruneOutcome> {
    use anyhow::Context as _;

    anyhow::ensure!(
        !resolves_to_audit_file(path),
        "refusing to prune {}: it resolves to the audit log, which is exempt from pruning by \
         design (ADR-0080 §11); read it with `omni-dev log --audit` instead",
        path.display()
    );

    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PruneOutcome {
                removed: 0,
                kept: 0,
                bytes_before: 0,
                bytes_after: 0,
            });
        }
        Err(e) => return Err(e).context("failed to read the log file"),
    };
    let bytes_before = data.len() as u64;
    let text = String::from_utf8_lossy(&data);

    // Every non-empty line, then the subset passing the age filter (both in
    // original — chronological — order).
    let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let aged: Vec<&str> = all
        .iter()
        .copied()
        .filter(|line| keep_by_age(line, opts.older_than))
        .collect();

    let kept: &[&str] = match opts.max_size {
        None => &aged,
        Some(max) => keep_by_size(&aged, max),
    };

    let bytes_after: u64 = kept.iter().map(|l| l.len() as u64 + 1).sum();
    let outcome = PruneOutcome {
        removed: all.len() - kept.len(),
        kept: kept.len(),
        bytes_before,
        bytes_after,
    };

    if !opts.dry_run && outcome.removed > 0 {
        rewrite_atomically(path, kept)?;
    }
    Ok(outcome)
}

/// Whether a raw line survives the age filter. Absent filter keeps everything;
/// an undateable or malformed line is conservatively kept.
fn keep_by_age(line: &str, older_than: Option<DateTime<Utc>>) -> bool {
    let Some(cutoff) = older_than else {
        return true;
    };
    match serde_json::from_str::<LogRecord>(line) {
        Ok(rec) => match DateTime::parse_from_rfc3339(&rec.timestamp) {
            Ok(ts) => ts.with_timezone(&Utc) >= cutoff,
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// Longest suffix of `lines` whose bytes (each line + its newline) fit in `max`,
/// but never fewer than the single most recent line.
fn keep_by_size<'a>(lines: &'a [&'a str], max: u64) -> &'a [&'a str] {
    let mut acc = 0u64;
    let mut start = lines.len();
    for (i, line) in lines.iter().enumerate().rev() {
        acc += line.len() as u64 + 1;
        if acc > max {
            break;
        }
        start = i;
    }
    if start == lines.len() && !lines.is_empty() {
        start = lines.len() - 1; // keep at least the most recent record
    }
    &lines[start..]
}

/// Writes `lines` (each newline-terminated) to a same-directory temp file and
/// atomically renames it over `path`, preserving the `0600` posture on unix.
fn rewrite_atomically(path: &Path, lines: &[&str]) -> anyhow::Result<()> {
    let tmp = sibling(path, &format!(".prune.{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        crate::daemon::paths::ensure_handle_0600(&file)?;
        for line in lines {
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The outcome of an invocation, recorded once after `cli.execute()` returns.
#[derive(Debug, Clone)]
pub struct InvocationOutcome {
    /// Resolved clap subcommand path.
    pub command: Vec<String>,
    /// Full argv.
    pub command_line: Vec<String>,
    /// Process exit code.
    pub exit_code: i32,
    /// Rendered error chain, when the command failed.
    pub error: Option<String>,
    /// Wall time of the whole invocation.
    pub duration: Duration,
}

/// Appends one `kind: "invocation"` record from the active context.
pub fn record_invocation(outcome: InvocationOutcome) {
    let ctx = current_context();
    let mut rec = LogRecord::new(RecordKind::Invocation, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    rec.command = outcome.command;
    rec.command_line = scrub_argv(&outcome.command_line);
    rec.exit_code = Some(outcome.exit_code);
    rec.error = outcome.error;
    rec.duration_ms = Some(outcome.duration.as_millis() as u64);
    rec.env = whitelisted_env();
    record(&rec);
}

/// The outcome of one `gh` subprocess invocation, recorded by
/// `crate::github_metrics::run_gh` after the process exits.
#[derive(Debug, Clone)]
pub struct GhOutcome {
    /// The semantic subcommand label, e.g. `"api graphql"`, `"pr list"`. Split on
    /// spaces into the record's `command` so `--command`/`command:` queries work.
    pub label: String,
    /// Full argv passed to `gh` (without the binary path). Scrubbed before write.
    pub argv: Vec<String>,
    /// Process exit code; `None` when the process could not be spawned.
    pub exit_code: Option<i32>,
    /// Wall time of the subprocess.
    pub duration: Duration,
    /// Spawn/collection error, when the invocation did not complete.
    pub error: Option<String>,
}

/// Appends one `kind: "gh"` record from the active context.
///
/// Best effort and exit-code-safe: it goes through [`record`], which swallows
/// every error, so a logging failure can never change a `gh` caller's result.
pub fn record_gh(outcome: GhOutcome) {
    record(&build_gh_record(outcome, current_context()));
}

/// Builds the `kind: "gh"` record for `outcome` under `ctx`. Split out from
/// [`record_gh`] so the record shape (source stamping, subcommand split, argv
/// scrubbing) is unit-testable without touching the filesystem or environment.
fn build_gh_record(outcome: GhOutcome, ctx: RequestLogContext) -> LogRecord {
    let mut rec = LogRecord::new(RecordKind::Gh, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    rec.command = outcome
        .label
        .split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    // Same scrubbing as invocation records — defense in depth. `gh` manages its
    // own auth (the token never enters our argv), but any secret-bearing `--flag`
    // or URL-query value is redacted regardless.
    rec.command_line = scrub_argv(&outcome.argv);
    rec.exit_code = outcome.exit_code;
    rec.error = outcome.error;
    rec.duration_ms = Some(outcome.duration.as_millis() as u64);
    rec
}

/// The outcome of one wrapped `git worktree` subprocess, recorded by the
/// `crate::cli::git` worktree subcommands after the process exits.
#[derive(Debug, Clone)]
pub struct WorktreeOutcome {
    /// The verb (`add`/`remove`/`list`/`move`/`prune`/`repair`); the record's
    /// `command` becomes `["git", "worktree", verb]` so `--command`/`command:`
    /// queries work.
    pub verb: String,
    /// Full argv passed to `git` (without the binary path). Scrubbed before
    /// write.
    pub argv: Vec<String>,
    /// Process exit code; `None` when the process could not be spawned.
    pub exit_code: Option<i32>,
    /// Wall time of the subprocess (metadata enrichment excluded).
    pub duration: Duration,
    /// Spawn/collection error, when the invocation did not complete.
    pub error: Option<String>,
    /// Recovery-relevant per-verb fields (`path`/`branch`/`commit`/
    /// `had_uncommitted`/`used_force`/…), written to the record's `context`.
    pub context: BTreeMap<String, String>,
}

/// Appends one `kind: "worktree"` record from the active context.
///
/// Best effort and exit-code-safe: it goes through [`record`], which swallows
/// every error, so a logging failure can never change the wrapped git
/// operation's result.
pub fn record_worktree(outcome: WorktreeOutcome) {
    record(&build_worktree_record(outcome, current_context()));
}

/// Builds the `kind: "worktree"` record for `outcome` under `ctx`. Split out
/// from [`record_worktree`] so the record shape (source stamping, service tag,
/// context passthrough, argv scrubbing) is unit-testable without touching the
/// filesystem or environment.
fn build_worktree_record(outcome: WorktreeOutcome, ctx: RequestLogContext) -> LogRecord {
    let mut rec = LogRecord::new(RecordKind::Worktree, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    // The service tag makes `--service worktree` the canonical recovery query.
    rec.service = Some("worktree".to_string());
    rec.command = vec!["git".to_string(), "worktree".to_string(), outcome.verb];
    rec.command_line = scrub_argv(&outcome.argv);
    rec.exit_code = outcome.exit_code;
    rec.error = outcome.error;
    rec.duration_ms = Some(outcome.duration.as_millis() as u64);
    rec.context = outcome.context;
    rec
}

/// Optional, non-secret extras for an HTTP record. Bodies/headers are gated and
/// redacted centrally in [`record_http_with`], so callers may pass them freely.
#[derive(Debug, Clone, Default)]
pub struct HttpExtra {
    /// True when served inside the daemon.
    pub via_daemon: bool,
    /// Pooled daemon session id that served the request.
    pub daemon_session_id: Option<String>,
    /// Non-secret identity used (never the secret).
    pub auth_principal: Option<String>,
    /// Raw request headers (redacted + gated before writing).
    pub request_headers: BTreeMap<String, String>,
    /// Raw response headers (redacted + gated before writing).
    pub response_headers: BTreeMap<String, String>,
    /// Request body (gated before writing).
    pub request_body: Option<String>,
    /// Response body (gated before writing).
    pub response_body: Option<String>,
    /// Free-form correlation tags.
    pub context: BTreeMap<String, String>,
}

/// Appends one `kind: "http"` record with method/url/status/elapsed/error.
pub fn record_http(
    service: &str,
    method: &str,
    url: &str,
    started: Instant,
    status: Option<u16>,
    error: Option<&str>,
) {
    record_http_with(
        service,
        method,
        url,
        started,
        status,
        error,
        HttpExtra::default(),
    );
}

/// Appends one `kind: "http"` record from a `reqwest` send result, mapping
/// `Ok` → status code and `Err` → error message.
///
/// Collapses the `match result { Ok → status, Err → error }` shape the REST
/// clients previously each open-coded around [`record_http`] (#1152).
pub fn record_http_result(
    service: &str,
    method: &str,
    url: &str,
    started: Instant,
    result: &reqwest::Result<reqwest::Response>,
) {
    match result {
        Ok(response) => {
            record_http(
                service,
                method,
                url,
                started,
                Some(response.status().as_u16()),
                None,
            );
        }
        Err(error) => {
            record_http(
                service,
                method,
                url,
                started,
                None,
                Some(&error.to_string()),
            );
        }
    }
}

/// Appends one `kind: "http"` record with extra, non-secret fields.
///
/// Headers and bodies are dropped unless their opt-in env var is set, headers
/// are always redacted, and URL query/fragment values under secret-looking
/// keys are replaced with `REDACTED` (`redact_url`) — so no secret can be
/// written here under any caller.
#[allow(clippy::too_many_arguments)]
pub fn record_http_with(
    service: &str,
    method: &str,
    url: &str,
    started: Instant,
    status: Option<u16>,
    error: Option<&str>,
    extra: HttpExtra,
) {
    if disabled() {
        return;
    }
    let ctx = current_context();
    let mut rec = LogRecord::new(RecordKind::Http, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    rec.service = Some(service.to_string());
    rec.method = Some(method.to_string());
    rec.url = Some(redact_url(url));
    rec.status_code = status;
    rec.elapsed_ms = Some(started.elapsed().as_millis() as u64);
    rec.error = error.map(str::to_string);
    rec.via_daemon = extra.via_daemon;
    rec.daemon_session_id = extra.daemon_session_id;
    rec.auth_principal = extra.auth_principal;
    rec.context = extra.context;
    if headers_enabled() {
        rec.request_headers = redact_headers(&extra.request_headers);
        rec.response_headers = redact_headers(&extra.response_headers);
    }
    if bodies_enabled() {
        rec.request_body = extra.request_body;
        rec.response_body = extra.response_body;
    }
    record(&rec);
}

/// Header names whose values must never be written (compared lowercased).
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "dd-api-key",
    "dd-application-key",
    "x-datadog-api-key",
    "x-datadog-application-key",
    "x-omni-bridge",
    "x-omni-bridge-target",
];

/// Substrings that mark a header name as secret-bearing (compared lowercased),
/// guarding against off-list auth headers (e.g. `x-auth-token`,
/// `x-goog-api-key`). False positives redact harmlessly.
const SENSITIVE_HEADER_MARKERS: &[&str] = &[
    "auth",
    "token",
    "secret",
    "key",
    "cookie",
    "password",
    "session",
    "signature",
    "credential",
];

/// Replaces sensitive header values with `REDACTED`, passing others through.
///
/// A header is sensitive when its lowercased name is in [`SENSITIVE_HEADERS`]
/// or contains any [`SENSITIVE_HEADER_MARKERS`] substring.
pub fn redact_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let redacted = SENSITIVE_HEADERS.contains(&lower.as_str())
                || SENSITIVE_HEADER_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker));
            (
                name.clone(),
                if redacted {
                    "REDACTED".to_string()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}

/// Flag-name segments marking a long flag's value as secret-bearing — the argv
/// counterpart of [`SECRETISH`]. Matched per `-`/`_`-separated segment of the
/// flag name so `--api-key` is caught but a name like `--keyword` is not.
const SECRETISH_FLAG_WORDS: &[&str] = &["token", "secret", "password", "passwd", "key"];

/// True when the long flag `--<name>` takes a secret-bearing value. Flags whose
/// last segment is `file` or `path` carry paths, not secrets, and are exempt
/// (e.g. `--token-file`).
fn is_secretish_flag(name: &str) -> bool {
    let segments: Vec<String> = name
        .split(['-', '_'])
        .map(str::to_ascii_lowercase)
        .collect();
    let takes_path = matches!(segments.last().map(String::as_str), Some("file" | "path"));
    !takes_path
        && segments
            .iter()
            .any(|segment| SECRETISH_FLAG_WORDS.contains(&segment.as_str()))
}

/// Scrubs one `--header` value (`Name: Value`): values of [`SENSITIVE_HEADERS`]
/// are redacted keeping the name, other headers pass through (`None`), and a
/// value with no colon is redacted wholesale.
fn scrub_header_arg(value: &str) -> Option<String> {
    let Some((name, _)) = value.split_once(':') else {
        return Some("REDACTED".to_string());
    };
    SENSITIVE_HEADERS
        .contains(&name.trim().to_ascii_lowercase().as_str())
        .then(|| format!("{}: REDACTED", name.trim()))
}

/// Returns the scrubbed replacement for the value of flag `--<name>`, or
/// `None` when the value is safe to log verbatim. `--body` keeps `@file`
/// references (a path, not a secret).
fn scrub_flag_value(name: &str, value: &str) -> Option<String> {
    match name {
        "header" => scrub_header_arg(value),
        "body" => (!value.starts_with('@')).then(|| "REDACTED".to_string()),
        "find" | "replacement" => Some("REDACTED".to_string()),
        _ if is_secretish_flag(name) => Some("REDACTED".to_string()),
        _ => None,
    }
}

/// Scrubs secret-bearing values out of a raw argv before it is logged. Two
/// write-side layers, so the on-disk line is clean and every reader/format is
/// covered with no reader changes:
///
/// 1. [`scrub_flag_secrets`] — flag-aware whole-value redaction (`--header`/
///    `--body` plus any [`is_secretish_flag`] name, in both `--flag value` and
///    `--flag=value` forms).
/// 2. [`redact_url`] over every resulting element — a secret-bearing query or
///    fragment parameter on a URL argument (most naturally
///    `--url /path?access_token=…`, which no flag-name rule catches) has its
///    value redacted, while benign argv passes through byte-identical (#1162).
fn scrub_argv(argv: &[String]) -> Vec<String> {
    scrub_flag_secrets(argv)
        .iter()
        .map(|arg| redact_url(arg))
        .collect()
}

/// Flag-aware first layer of [`scrub_argv`]: redacts secret-bearing flag values
/// (`--header`/`--body` plus any [`is_secretish_flag`] name, in both
/// `--flag value` and `--flag=value` forms). Everything else passes through to
/// the URL layer.
fn scrub_flag_secrets(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        let Some(flag_body) = arg.strip_prefix("--") else {
            out.push(arg.clone());
            continue;
        };
        if let Some((name, value)) = flag_body.split_once('=') {
            match scrub_flag_value(name, value) {
                Some(scrubbed) => out.push(format!("--{name}={scrubbed}")),
                None => out.push(arg.clone()),
            }
        } else {
            out.push(arg.clone());
            let takes_secret_value =
                matches!(flag_body, "header" | "body" | "find" | "replacement")
                    || is_secretish_flag(flag_body);
            if takes_secret_value {
                if let Some(value) = argv.get(i) {
                    i += 1;
                    out.push(scrub_flag_value(flag_body, value).unwrap_or_else(|| value.clone()));
                }
            }
        }
    }
    out
}

/// Query/fragment keys that are secrets outright (compared decoded + lowercased).
const SENSITIVE_QUERY_KEYS: &[&str] = &["sig", "sas", "jwt", "auth"];

/// Key suffixes marking the open-ended secret families (`access_token`,
/// `client_secret`, `api_key`, …).
const SENSITIVE_QUERY_KEY_SUFFIXES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "signature",
    "apikey",
    "api_key",
    "api-key",
];

/// Key prefixes for cloud-storage signed-URL parameter families.
const SENSITIVE_QUERY_KEY_PREFIXES: &[&str] = &["x-amz-", "x-goog-"];

/// Returns whether a decoded query/fragment key looks secret-bearing.
fn sensitive_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_QUERY_KEYS.contains(&key.as_str())
        || SENSITIVE_QUERY_KEY_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
        || SENSITIVE_QUERY_KEY_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// Rewrites one `&`-separated pair list, replacing the values of
/// secret-bearing keys with `REDACTED` and passing every other segment
/// through byte-verbatim.
fn redact_pairs(pairs: &str) -> String {
    pairs
        .split('&')
        .map(|segment| match segment.split_once('=') {
            Some((raw_key, _)) => {
                // Decode only the key (handles `access%5Ftoken` and `+`); the
                // raw key text is preserved in the output.
                let sensitive = url::form_urlencoded::parse(raw_key.as_bytes())
                    .next()
                    .is_some_and(|(key, _)| sensitive_query_key(&key));
                if sensitive {
                    format!("{raw_key}=REDACTED")
                } else {
                    segment.to_string()
                }
            }
            // A bare key (no `=`) carries no value to leak.
            None => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Redacts secret-bearing query and fragment parameter values in a URL,
/// preserving scheme, host, path, and all parameter keys so `--url` substring
/// filtering stays useful. Handles relative URLs (the browser bridge logs
/// page-origin targets like `/api/foo?sig=…`), so this never requires the
/// input to parse as an absolute [`url::Url`].
fn redact_url(url: &str) -> String {
    let (rest, fragment) = url
        .split_once('#')
        .map_or((url, None), |(rest, fragment)| (rest, Some(fragment)));
    let (prefix, query) = rest
        .split_once('?')
        .map_or((rest, None), |(prefix, query)| (prefix, Some(query)));
    let mut out = prefix.to_string();
    if let Some(query) = query {
        out.push('?');
        out.push_str(&redact_pairs(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(&redact_pairs(fragment));
    }
    out
}

/// A time-sortable id: 13-digit zero-padded epoch-millis, a dash, then 16 hex.
///
/// Lexical order ≈ chronological order, which is all the reader needs. Mirrors
/// the uuid-shaped minting in [`crate::snowflake::client`] without adding a
/// crate.
pub fn new_id() -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0);
    let suffix = rand::random::<u64>();
    format!("{millis:013}-{suffix:016x}")
}

/// Current time as RFC3339 with millisecond precision, in UTC.
fn now_rfc3339_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Best-effort current working directory.
fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Best-effort OS username (`$USER`, then the passwd entry for the euid).
fn system_user() -> String {
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            return user;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::geteuid()) {
            return user.name;
        }
    }
    String::new()
}

/// Best-effort hostname (`gethostname`, then `$HOSTNAME`, then empty).
fn hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(name) = nix::unistd::gethostname() {
            if let Some(name) = name.to_str() {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_default()
}

/// Names matching these substrings have their env values redacted, guarding
/// against any future secret-bearing `OMNI_DEV_*` var.
const SECRETISH: &[&str] = &["TOKEN", "SECRET", "KEY", "PASSWORD", "PASSWD"];

/// Snapshot of `OMNI_DEV_*` env vars, with secret-looking values redacted.
fn whitelisted_env() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("OMNI_DEV_"))
        .map(|(k, v)| {
            let secretish = SECRETISH.iter().any(|needle| k.contains(needle));
            let value = if secretish { "REDACTED".to_string() } else { v };
            (k, value)
        })
        .collect()
}

/// Lines as omni-dev 0.46.0 wrote them, kept as literals rather than builder calls:
/// the Google writers move to gwi (#2203, ADR-0095), but the reader must keep
/// decoding and rendering what they left behind in `log.jsonl` and `audit.jsonl`.
#[cfg(test)]
pub(crate) mod legacy_lines {
    /// A `drivemutation` line from `log.jsonl`.
    pub(crate) const DRIVE_MUTATION: &str = r#"{"id":"rec-1","invocation_id":"inv-3","kind":"drivemutation","timestamp":"2026-06-22T12:34:56.789Z","hostname":"host","pid":4242,"omni_dev_version":"0.46.0","cwd":"/work/repo","system_user":"user","command":["drive","move"],"duration_ms":17,"source":"mcp","mcp_tool":"drive_file_move","service":"drive","context":{"added_principals":"alice@example.com","crosses_drive_boundary":"true","file_id":"f1","file_name":"report.pdf","resolved_folder_id":"dest1","status":"blocked"}}"#;

    /// An `audit` line from `audit.jsonl`.
    pub(crate) const AUDIT: &str = r#"{"id":"rec-2","invocation_id":"inv-3","kind":"audit","timestamp":"2026-06-22T12:35:00.000Z","hostname":"host","pid":4242,"omni_dev_version":"0.46.0","cwd":"/work/repo","system_user":"user","command":["drive","lease-acquire"],"source":"mcp","mcp_tool":"drive_file_move","service":"drive","context":{"file_id":"f1","integration":"drive","lease_id":"lease-1","verdict":"acquired","version_after":"7"}}"#;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn record_round_trips_through_json() {
        let mut rec = LogRecord::new(RecordKind::Http, "inv-1".to_string());
        rec.service = Some("jira".to_string());
        rec.method = Some("GET".to_string());
        rec.url = Some("https://example.atlassian.net/rest/api/3/issue/X-1".to_string());
        rec.status_code = Some(200);
        rec.elapsed_ms = Some(42);

        let line = serde_json::to_string(&rec).unwrap();
        let back: LogRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.invocation_id, "inv-1");
        assert_eq!(back.kind, RecordKind::Http);
        assert_eq!(back.service.as_deref(), Some("jira"));
        assert_eq!(back.status_code, Some(200));
    }

    #[test]
    fn reader_tolerates_unknown_fields() {
        let line = r#"{"id":"x","invocation_id":"i","kind":"http","method":"GET",
            "future_field":{"nested":true},"another":42}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Http);
        assert_eq!(rec.method.as_deref(), Some("GET"));
    }

    #[test]
    fn reader_tolerates_missing_newer_fields() {
        // An "old" line with only a couple of fields present.
        let line = r#"{"kind":"invocation","command":["git","view"]}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Invocation);
        assert_eq!(rec.command, vec!["git", "view"]);
        assert!(rec.status_code.is_none());
        assert!(rec.id.is_empty());
    }

    #[test]
    fn unknown_kind_and_source_do_not_fail() {
        let line = r#"{"kind":"telemetry","source":"webhook"}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Unknown);
        assert_eq!(rec.source, Some(Source::Unknown));
    }

    #[test]
    fn optional_fields_are_skipped_when_empty() {
        let rec = LogRecord::new(RecordKind::Invocation, "i".to_string());
        let line = serde_json::to_string(&rec).unwrap();
        // Empty collections / None options must not appear on the wire.
        assert!(!line.contains("status_code"));
        assert!(!line.contains("request_headers"));
        assert!(!line.contains("via_daemon"));
        assert!(!line.contains("\"env\""));
    }

    #[test]
    fn ids_are_time_sortable() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn sensitive_headers_are_redacted() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer secret".to_string());
        headers.insert("X-Api-Key".to_string(), "abc123".to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let out = redact_headers(&headers);
        assert_eq!(out["Authorization"], "REDACTED");
        assert_eq!(out["X-Api-Key"], "REDACTED");
        assert_eq!(out["Content-Type"], "application/json");
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().copied().map(String::from).collect()
    }

    #[test]
    fn build_gh_record_stamps_kind_source_and_split_command() {
        let ctx = RequestLogContext {
            invocation_id: "inv-1".to_string(),
            source: Source::Daemon,
            mcp_tool: None,
        };
        let rec = build_gh_record(
            GhOutcome {
                label: "api graphql".to_string(),
                argv: argv(&["api", "graphql", "-f", "query=xyz"]),
                exit_code: Some(0),
                duration: Duration::from_millis(120),
                error: None,
            },
            ctx,
        );
        assert_eq!(rec.kind, RecordKind::Gh);
        assert_eq!(rec.invocation_id, "inv-1");
        assert_eq!(rec.source, Some(Source::Daemon));
        // The label is split into `command` for per-subcommand aggregation.
        assert_eq!(rec.command, argv(&["api", "graphql"]));
        assert_eq!(
            rec.command_line,
            argv(&["api", "graphql", "-f", "query=xyz"])
        );
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.duration_ms, Some(120));
        assert!(rec.error.is_none());
    }

    #[test]
    fn build_gh_record_scrubs_secret_bearing_argv() {
        // Defense in depth: even though `gh` never receives our auth, a
        // secret-bearing flag value in the argv is redacted before write.
        let rec = build_gh_record(
            GhOutcome {
                label: "api graphql".to_string(),
                argv: argv(&["api", "--header", "Authorization: Bearer sekret"]),
                exit_code: Some(0),
                duration: Duration::from_millis(5),
                error: None,
            },
            RequestLogContext::default(),
        );
        assert_eq!(
            rec.command_line,
            argv(&["api", "--header", "Authorization: REDACTED"])
        );
    }

    #[test]
    fn build_worktree_record_stamps_kind_service_command_and_context() {
        let ctx = RequestLogContext {
            invocation_id: "inv-2".to_string(),
            source: Source::Mcp,
            mcp_tool: Some("some_tool".to_string()),
        };
        let mut context = BTreeMap::new();
        context.insert("path".to_string(), "/tmp/wt".to_string());
        context.insert("branch".to_string(), "demo-wt".to_string());
        context.insert("had_uncommitted".to_string(), "true".to_string());
        let rec = build_worktree_record(
            WorktreeOutcome {
                verb: "remove".to_string(),
                argv: argv(&["worktree", "remove", "--force", "/tmp/wt"]),
                exit_code: Some(0),
                duration: Duration::from_millis(42),
                error: None,
                context,
            },
            ctx,
        );
        assert_eq!(rec.kind, RecordKind::Worktree);
        assert_eq!(rec.invocation_id, "inv-2");
        assert_eq!(rec.source, Some(Source::Mcp));
        assert_eq!(rec.mcp_tool.as_deref(), Some("some_tool"));
        assert_eq!(rec.service.as_deref(), Some("worktree"));
        assert_eq!(rec.command, argv(&["git", "worktree", "remove"]));
        assert_eq!(
            rec.command_line,
            argv(&["worktree", "remove", "--force", "/tmp/wt"])
        );
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.duration_ms, Some(42));
        assert_eq!(
            rec.context.get("branch").map(String::as_str),
            Some("demo-wt")
        );
        assert_eq!(
            rec.context.get("had_uncommitted").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn record_kind_worktree_serializes_as_worktree_and_round_trips() {
        let rec = build_worktree_record(
            WorktreeOutcome {
                verb: "add".to_string(),
                argv: argv(&["worktree", "add", "wt"]),
                exit_code: Some(1),
                duration: Duration::from_millis(1),
                error: Some("boom".to_string()),
                context: BTreeMap::new(),
            },
            RequestLogContext::default(),
        );
        let line = serde_json::to_string(&rec).unwrap();
        assert!(line.contains("\"kind\":\"worktree\""), "line was: {line}");
        assert!(
            line.contains("\"service\":\"worktree\""),
            "line was: {line}"
        );
        // The display name matches the wire form.
        assert_eq!(RecordKind::Worktree.as_str(), "worktree");
        let back: LogRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, RecordKind::Worktree);
        assert_eq!(back.command, argv(&["git", "worktree", "add"]));
        assert_eq!(back.error.as_deref(), Some("boom"));
    }

    #[test]
    fn legacy_drive_mutation_line_still_decodes_with_its_kind_and_context() {
        let rec: LogRecord = serde_json::from_str(legacy_lines::DRIVE_MUTATION).unwrap();
        assert_eq!(rec.kind, RecordKind::DriveMutation);
        assert_eq!(rec.kind.as_str(), "drivemutation");
        assert_eq!(rec.invocation_id, "inv-3");
        assert_eq!(rec.source, Some(Source::Mcp));
        assert_eq!(rec.mcp_tool.as_deref(), Some("drive_file_move"));
        assert_eq!(rec.command, argv(&["drive", "move"]));
        assert_eq!(rec.duration_ms, Some(17));
        assert_eq!(
            rec.context.get("file_name").map(String::as_str),
            Some("report.pdf")
        );
        assert_eq!(
            rec.context.get("status").map(String::as_str),
            Some("blocked")
        );
    }

    #[test]
    fn legacy_audit_line_still_decodes_with_its_kind_and_context() {
        let rec: LogRecord = serde_json::from_str(legacy_lines::AUDIT).unwrap();
        assert_eq!(rec.kind, RecordKind::Audit);
        assert_eq!(rec.kind.as_str(), "audit");
        assert_eq!(rec.command, argv(&["drive", "lease-acquire"]));
        assert_eq!(
            rec.context.get("integration").map(String::as_str),
            Some("drive")
        );
        assert_eq!(
            rec.context.get("lease_id").map(String::as_str),
            Some("lease-1")
        );
        assert_eq!(
            rec.context.get("verdict").map(String::as_str),
            Some("acquired")
        );
    }

    #[test]
    fn audit_file_path_never_resolves_to_the_real_machine_default_when_unset() {
        // Regression test for a real incident: before this fallback
        // existed, any test exercising a leased write or `drive lease
        // acquire` without itself redirecting the audit log silently
        // wrote into this machine's actual `audit.jsonl`. With the var
        // unset, every call in a test build must land on the same shared
        // scratch path — stable across calls (so it's memoized once, not
        // re-created per call) and never the real
        // `dirs::state_dir()`/`dirs::data_dir()`-based default the
        // non-test build resolves to, whether resolved through the pure
        // `audit_file_path_with` seam or the ambient `audit_file_path`.
        let real_default = dirs::state_dir()
            .or_else(dirs::data_dir)
            .unwrap()
            .join("omni-dev")
            .join(AUDIT_FILE_NAME);

        let unset = MapEnv::new();
        let unrouted = audit_file_path_with(&unset).unwrap();
        assert_eq!(
            unrouted,
            audit_file_path_with(&unset).unwrap(),
            "the fallback path must be stable across calls"
        );
        assert_ne!(
            unrouted, real_default,
            "must never fall back to the real machine's default audit log path in a test build"
        );

        assert_eq!(
            audit_file_path().unwrap(),
            unrouted,
            "the ambient entry point with no thread-local route lands on the same scratch file"
        );
    }

    #[test]
    fn record_audit_fails_closed_when_the_write_errors() {
        // A directory is not a valid target for `OpenOptions::append`, so this
        // forces the same write failure a permission error or a missing parent
        // would — the point is that `record_audit`, unlike `record`, hands the
        // error back rather than swallowing it.
        let dir = tempfile::tempdir().unwrap();
        let rec = LogRecord {
            kind: RecordKind::Audit,
            id: new_id(),
            invocation_id: new_id(),
            ..LogRecord::default()
        };
        let err = append_record_to(dir.path(), &rec, append_line_no_rotation).unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn the_audit_write_path_never_gates_on_log_disable() {
        // The exemption test above pins `record_audit_with`, the injected-env
        // test seam — never the ambient `record_audit`/`record_audit_to` that
        // production actually calls. This closes that gap by grepping their
        // real source: a regression that added `if disabled() { return Ok(()) }`
        // to either, mirroring `record`'s own pattern, would fail here even
        // though it slips past the env-injected test above.
        let source = include_str!("request_log.rs");
        let start = source
            .find("pub fn record_audit(entry: &LogRecord) -> anyhow::Result<()> {")
            .expect("record_audit signature not found");
        let end = source[start..]
            .find("/// Serializes `entry` and appends it to `path` via `append`,")
            .expect("append_record_to doc comment not found")
            + start;
        let audit_write_path = &source[start..end];
        assert!(
            !audit_write_path.contains("disabled"),
            "the fail-closed audit sink must never gate on OMNI_DEV_LOG_DISABLE:\n{audit_write_path}"
        );
    }

    #[test]
    fn try_record_refuses_an_audit_kind_entry() {
        let rec = LogRecord {
            kind: RecordKind::Audit,
            id: new_id(),
            invocation_id: new_id(),
            ..LogRecord::default()
        };
        let err = try_record(&rec).unwrap_err();
        assert!(
            err.to_string().contains("no longer writes audit.jsonl"),
            "{err}"
        );
    }

    #[test]
    fn log_file_path_with_refuses_when_it_resolves_to_the_audit_file() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.jsonl");
        let shared_str = shared.to_str().unwrap();
        let env = MapEnv::new()
            .with("OMNI_DEV_LOG_FILE", shared_str)
            .with("OMNI_DEV_AUDIT_LOG_FILE", shared_str);

        assert!(log_file_path_with(&env).is_none());
    }

    #[test]
    fn log_file_path_with_refuses_a_dot_dot_spelling_of_the_audit_file() {
        // Mirrors `same_file_matches_a_dot_dot_spelling_routed_through_a_nonexistent_directory`:
        // the guard must compare by file identity, not raw spelling.
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.jsonl");
        std::fs::write(&audit, "{}\n").unwrap();
        let dotted = dir.path().join("sub/../audit.jsonl");
        let env = MapEnv::new()
            .with("OMNI_DEV_LOG_FILE", dotted.to_str().unwrap())
            .with("OMNI_DEV_AUDIT_LOG_FILE", audit.to_str().unwrap());

        assert!(log_file_path_with(&env).is_none());
    }

    #[test]
    fn scrub_argv_redacts_sensitive_header_in_both_forms() {
        let out = scrub_argv(&argv(&[
            "omni-dev",
            "--header",
            "Authorization: Bearer sekret",
            "--header=Cookie: session=abc",
        ]));
        assert_eq!(
            out,
            argv(&[
                "omni-dev",
                "--header",
                "Authorization: REDACTED",
                "--header=Cookie: REDACTED",
            ])
        );
    }

    #[test]
    fn scrub_argv_keeps_non_sensitive_headers() {
        let input = argv(&["omni-dev", "--header", "Content-Type: application/json"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_redacts_colonless_header_wholesale() {
        let out = scrub_argv(&argv(&["omni-dev", "--header", "sekret"]));
        assert_eq!(out, argv(&["omni-dev", "--header", "REDACTED"]));
    }

    #[test]
    fn scrub_argv_redacts_inline_body_but_keeps_at_file() {
        let out = scrub_argv(&argv(&["omni-dev", "--body", r#"{"secret":1}"#]));
        assert_eq!(out, argv(&["omni-dev", "--body", "REDACTED"]));

        let file_form = argv(&["omni-dev", "--body", "@payload.json"]);
        assert_eq!(scrub_argv(&file_form), file_form);

        let out = scrub_argv(&argv(&["omni-dev", "--body=sekret"]));
        assert_eq!(out, argv(&["omni-dev", "--body=REDACTED"]));
    }

    #[test]
    fn scrub_argv_redacts_find_replace_content_in_both_flag_forms() {
        let out = scrub_argv(&argv(&[
            "omni-dev",
            "--find",
            "customer@example.com",
            "--replacement=redacted",
        ]));
        assert_eq!(
            out,
            argv(&["omni-dev", "--find", "REDACTED", "--replacement=REDACTED",])
        );
    }

    #[test]
    fn scrub_argv_redacts_secretish_flag_values() {
        let out = scrub_argv(&argv(&["omni-dev", "--api-key", "abc", "--auth-token=xyz"]));
        assert_eq!(
            out,
            argv(&["omni-dev", "--api-key", "REDACTED", "--auth-token=REDACTED"])
        );
    }

    #[test]
    fn scrub_argv_exempts_path_flags_and_positionals() {
        let input = argv(&["omni-dev", "--token-file", "/tmp/t", "PROJ-123"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_redacts_secret_bearing_url_query_in_both_forms() {
        // `--url` is not a secret-ish flag name, so its value is caught by the
        // redact_url layer, not the flag layer (#1162). Both argv shapes plus a
        // bare positional URL are covered; the benign `page` param survives.
        let space = scrub_argv(&argv(&[
            "omni-dev",
            "browser",
            "bridge",
            "request",
            "--url",
            "/api/export?access_token=hunter2&sig=deadbeef&page=3",
        ]));
        assert_eq!(
            *space.last().unwrap(),
            "/api/export?access_token=REDACTED&sig=REDACTED&page=3"
        );

        let eq_form = scrub_argv(&argv(&[
            "omni-dev",
            "--url=/api/export?access_token=hunter2&page=3",
        ]));
        assert_eq!(
            *eq_form.last().unwrap(),
            "--url=/api/export?access_token=REDACTED&page=3"
        );

        let positional = scrub_argv(&argv(&["omni-dev", "https://h/cb#id_token=xyz"]));
        assert_eq!(
            *positional.last().unwrap(),
            "https://h/cb#id_token=REDACTED"
        );
    }

    #[test]
    fn scrub_argv_leaves_benign_argv_byte_identical() {
        let input = argv(&[
            "omni-dev",
            "browser",
            "bridge",
            "request",
            "--control-port",
            "19998",
            "--url",
            "/api/export?page=3&sort=asc",
        ]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_handles_trailing_flag_without_value() {
        let input = argv(&["omni-dev", "--body"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[cfg(unix)]
    #[test]
    fn append_line_creates_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        append_line(&path, "{\"kind\":\"http\"}\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"kind\":\"http\"}\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_line_retightens_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append_line(&path, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn url_without_query_is_unchanged() {
        assert_eq!(redact_url("https://h/p"), "https://h/p");
        assert_eq!(redact_url("/relative/p"), "/relative/p");
    }

    #[test]
    fn benign_query_is_byte_identical() {
        let url = "https://h/p?q=a%20b&page=2&&x=y+z&keyword=k&sort_key=s&token_type=bearer";
        assert_eq!(redact_url(url), url);
    }

    #[test]
    fn sensitive_query_values_are_redacted() {
        let url = "https://h/p?token=a&access_token=b&client_secret=c&api_key=d&x=1";
        assert_eq!(
            redact_url(url),
            "https://h/p?token=REDACTED&access_token=REDACTED&client_secret=REDACTED\
             &api_key=REDACTED&x=1"
        );
    }

    #[test]
    fn presigned_s3_query_is_redacted() {
        let url = "https://bucket.s3.amazonaws.com/key?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                   &X-Amz-Credential=AKIA%2F20260703%2Fus-east-1%2Fs3%2Faws4_request\
                   &X-Amz-Date=20260703T000000Z&X-Amz-Expires=3600\
                   &X-Amz-SignedHeaders=host&X-Amz-Signature=deadbeef";
        assert_eq!(
            redact_url(url),
            "https://bucket.s3.amazonaws.com/key?X-Amz-Algorithm=REDACTED\
             &X-Amz-Credential=REDACTED&X-Amz-Date=REDACTED&X-Amz-Expires=REDACTED\
             &X-Amz-SignedHeaders=REDACTED&X-Amz-Signature=REDACTED"
        );
    }

    #[test]
    fn key_matching_is_case_insensitive() {
        assert_eq!(
            redact_url("/p?TOKEN=x&Api_Key=y&X-Amz-Signature=z"),
            "/p?TOKEN=REDACTED&Api_Key=REDACTED&X-Amz-Signature=REDACTED"
        );
    }

    #[test]
    fn repeated_sensitive_keys_are_each_redacted() {
        assert_eq!(redact_url("/p?sig=a&sig=b"), "/p?sig=REDACTED&sig=REDACTED");
    }

    #[test]
    fn valueless_key_is_left_alone() {
        assert_eq!(redact_url("/p?token"), "/p?token");
        assert_eq!(redact_url("/p?token="), "/p?token=REDACTED");
    }

    #[test]
    fn relative_url_query_is_redacted() {
        assert_eq!(
            redact_url("/api/foo?sig=abc&x=y"),
            "/api/foo?sig=REDACTED&x=y"
        );
    }

    #[test]
    fn fragment_credentials_are_redacted() {
        assert_eq!(
            redact_url("https://h/cb#access_token=xyz&token_type=bearer"),
            "https://h/cb#access_token=REDACTED&token_type=bearer"
        );
    }

    #[test]
    fn query_and_fragment_are_scrubbed_independently() {
        assert_eq!(
            redact_url("/p?sig=a#id_token=b"),
            "/p?sig=REDACTED#id_token=REDACTED"
        );
    }

    #[test]
    fn question_mark_in_fragment_is_not_parsed_as_query() {
        // The fragment is split off before the query, so `?` inside it never
        // starts a query; the pseudo-key `frag?token` still redacts via the
        // suffix rule (over-redaction in the safe direction).
        assert_eq!(
            redact_url("https://h/p#frag?token=x"),
            "https://h/p#frag?token=REDACTED"
        );
    }

    #[test]
    fn encoded_sensitive_key_is_decoded_before_matching() {
        assert_eq!(
            redact_url("/p?access%5Ftoken=v"),
            "/p?access%5Ftoken=REDACTED"
        );
    }

    #[test]
    fn empty_query_is_unchanged() {
        assert_eq!(redact_url("https://h/p?"), "https://h/p?");
        assert_eq!(redact_url("https://h/p?#f"), "https://h/p?#f");
    }

    #[test]
    fn parse_size_handles_units_and_bare_bytes() {
        assert_eq!(parse_size("1048576").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("512b").unwrap(), 512);
        assert_eq!(parse_size("10kb").unwrap(), 10 * 1024);
        assert_eq!(parse_size("2K").unwrap(), 2 * 1024);
        assert_eq!(parse_size("3mb").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_size("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1.5mb").unwrap(), (1.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size(" 4mib ").unwrap(), 4 * 1024 * 1024);
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("mb").is_err());
        assert!(parse_size("10tb").is_err());
        assert!(parse_size("-5mb").is_err());
    }

    #[test]
    fn sibling_appends_to_final_component() {
        let base = Path::new("/tmp/omni/log.jsonl");
        assert_eq!(sibling(base, ".1"), Path::new("/tmp/omni/log.jsonl.1"));
        assert_eq!(
            sibling(base, ".lock"),
            Path::new("/tmp/omni/log.jsonl.lock")
        );
    }

    #[test]
    fn keep_by_size_keeps_most_recent_that_fit() {
        // Four 10-byte lines (11 bytes on disk each with the newline).
        let lines = ["aaaaaaaaaa", "bbbbbbbbbb", "cccccccccc", "dddddddddd"];
        let refs: Vec<&str> = lines.to_vec();

        // Budget for exactly two lines (22 bytes) keeps the last two.
        assert_eq!(keep_by_size(&refs, 22), &["cccccccccc", "dddddddddd"]);
        // A budget smaller than one line still keeps the single most recent.
        assert_eq!(keep_by_size(&refs, 1), &["dddddddddd"]);
        // A generous budget keeps everything.
        assert_eq!(keep_by_size(&refs, 10_000), &refs[..]);
        // Empty input yields empty output (no panic).
        assert!(keep_by_size(&[], 100).is_empty());
    }

    #[test]
    fn keep_by_age_is_conservative_on_undateable_lines() {
        let cutoff = DateTime::parse_from_rfc3339("2026-06-01T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let old = r#"{"kind":"http","timestamp":"2026-01-01T00:00:00.000Z"}"#;
        let new = r#"{"kind":"http","timestamp":"2026-12-01T00:00:00.000Z"}"#;
        let undated = r#"{"kind":"http"}"#;
        let malformed = "not json at all";

        assert!(!keep_by_age(old, Some(cutoff)));
        assert!(keep_by_age(new, Some(cutoff)));
        assert!(keep_by_age(undated, Some(cutoff)), "undated is kept");
        assert!(keep_by_age(malformed, Some(cutoff)), "malformed is kept");
        assert!(keep_by_age(old, None), "no filter keeps everything");
    }

    fn http_line(id: &str, ts: &str) -> String {
        format!(r#"{{"id":"{id}","kind":"http","timestamp":"{ts}"}}"#)
    }

    #[test]
    fn prune_by_age_drops_old_records_and_rewrites_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let body = format!(
            "{}\n{}\n{}\n",
            http_line("1", "2026-01-01T00:00:00.000Z"),
            http_line("2", "2026-06-15T00:00:00.000Z"),
            http_line("3", "2026-12-31T00:00:00.000Z"),
        );
        std::fs::write(&path, &body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let cutoff = DateTime::parse_from_rfc3339("2026-06-01T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let outcome = prune(
            &path,
            &PruneOptions {
                older_than: Some(cutoff),
                max_size: None,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.kept, 2);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains(r#""id":"1""#));
        assert!(contents.contains(r#""id":"2""#));
        assert!(contents.contains(r#""id":"3""#));
        // The atomic rewrite lands a fresh 0600 file regardless of the old mode.
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn prune_dry_run_reports_without_modifying() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let body = format!(
            "{}\n{}\n",
            http_line("1", "2026-01-01T00:00:00.000Z"),
            http_line("2", "2026-12-31T00:00:00.000Z"),
        );
        std::fs::write(&path, &body).unwrap();

        let cutoff = DateTime::parse_from_rfc3339("2026-06-01T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let outcome = prune(
            &path,
            &PruneOptions {
                older_than: Some(cutoff),
                max_size: None,
                dry_run: true,
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        // File is untouched by a dry run.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
    }

    #[test]
    fn prune_by_size_keeps_the_newest_that_fit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let l1 = http_line("1", "2026-01-01T00:00:00.000Z");
        let l2 = http_line("2", "2026-06-15T00:00:00.000Z");
        let l3 = http_line("3", "2026-12-31T00:00:00.000Z");
        std::fs::write(&path, format!("{l1}\n{l2}\n{l3}\n")).unwrap();

        // Budget that fits only the last two lines.
        let budget = (l2.len() + 1 + l3.len() + 1) as u64;
        let outcome = prune(
            &path,
            &PruneOptions {
                older_than: None,
                max_size: Some(budget),
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.kept, 2);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains(r#""id":"1""#));
        assert!(contents.contains(r#""id":"3""#));
    }

    #[test]
    fn prune_missing_file_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.jsonl");
        let outcome = prune(
            &path,
            &PruneOptions {
                older_than: None,
                max_size: Some(1),
                dry_run: false,
            },
        )
        .unwrap();
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.kept, 0);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rotation_shifts_numbered_files_and_drops_the_oldest() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // A tiny cap so every second short line rotates.
        let cfg = RotationConfig {
            max_size: 20,
            keep_files: 2,
        };

        let line = "0123456789012345\n"; // 17 bytes
        for _ in 0..4 {
            append_with_rotation(&path, line, &cfg).unwrap();
        }

        // The live file plus at most keep_files (2) rotated files exist; a .3
        // must never appear.
        assert!(path.exists());
        assert!(sibling(&path, ".1").exists());
        assert!(sibling(&path, ".2").exists());
        assert!(!sibling(&path, ".3").exists());
        // Rotated files keep the 0600 posture.
        assert_eq!(
            std::fs::metadata(sibling(&path, ".1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotation_keep_zero_discards_on_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let cfg = RotationConfig {
            max_size: 20,
            keep_files: 0,
        };
        let line = "0123456789012345\n"; // 17 bytes
        append_with_rotation(&path, line, &cfg).unwrap();
        append_with_rotation(&path, line, &cfg).unwrap();
        // No .1 is retained; only the current (single-line) file survives.
        assert!(!sibling(&path, ".1").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), line);
    }

    #[test]
    fn rotated_files_lists_numbered_siblings_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        for name in [
            "log.jsonl",
            "log.jsonl.1",
            "log.jsonl.2",
            "log.jsonl.10",
            "log.jsonl.9",
            // Not rotated files: the writers' lock, a backup of one, a trailing dot,
            // a non-number, a number too large for a count, another log's rotation.
            "log.jsonl.lock",
            "log.jsonl.1.bak",
            "log.jsonl.",
            "log.jsonl.x",
            "log.jsonl.99999999999",
            // Rotation numbers from 1 and writes no padding or sign.
            "log.jsonl.0",
            "log.jsonl.01",
            "log.jsonl.+2",
            "other.jsonl.3",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        // Opening a directory succeeds and reading it does not; it is no log.
        std::fs::create_dir(dir.path().join("log.jsonl.3")).unwrap();

        let names: Vec<_> = rotated_files(&path)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Numeric, not lexicographic: `.9` comes before `.10`.
        assert_eq!(
            names,
            ["log.jsonl.1", "log.jsonl.2", "log.jsonl.9", "log.jsonl.10"]
        );
    }

    #[test]
    fn a_bare_file_name_is_listed_in_the_current_directory() {
        // `Path::parent` of a bare name is empty, which `read_dir` rejects.
        assert_eq!(sibling_dir(Path::new("log.jsonl")), Some(Path::new(".")));
        assert_eq!(
            sibling_dir(Path::new("state/log.jsonl")),
            Some(Path::new("state"))
        );
        assert_eq!(sibling_dir(Path::new("/log.jsonl")), Some(Path::new("/")));
        assert_eq!(sibling_dir(Path::new("/")), None);
        assert!(std::fs::read_dir(sibling_dir(Path::new("log.jsonl")).unwrap()).is_ok());
    }

    #[test]
    fn rotated_files_of_a_missing_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(rotated_files(&dir.path().join("gone").join("log.jsonl")).is_empty());
        assert!(rotated_files(Path::new("/")).is_empty());
    }

    #[test]
    fn a_missing_directory_is_not_worth_a_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let logs = crate::test_support::capture_at(tracing::Level::DEBUG, || {
            assert!(rotated_files(&dir.path().join("gone").join("log.jsonl")).is_empty());
        });
        assert_eq!(logs, "");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_listed_has_no_rotated_files_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // A parent that is a file is not a missing directory: it fails with
        // `ENOTDIR`, which is not `NotFound`.
        let parent = dir.path().join("not-a-dir");
        std::fs::write(&parent, "").unwrap();

        let logs = crate::test_support::capture_at(tracing::Level::DEBUG, || {
            assert!(rotated_files(&parent.join("log.jsonl")).is_empty());
        });
        assert!(
            logs.contains(&format!(
                "request_log: cannot list {} for rotated files:",
                parent.display()
            )),
            "{logs}"
        );
    }

    #[test]
    fn an_unreadable_entry_is_skipped_and_logged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("log.jsonl.1"), "").unwrap();
        let entry = std::fs::read_dir(dir.path()).unwrap().next().unwrap();
        assert_eq!(
            readable_entry(entry, dir.path()).unwrap().file_name(),
            "log.jsonl.1"
        );

        let logs = crate::test_support::capture_at(tracing::Level::DEBUG, || {
            let failed = Err(std::io::Error::other("entry vanished"));
            assert!(readable_entry(failed, Path::new("state")).is_none());
        });
        assert!(
            logs.contains("request_log: skipping an unreadable entry of state: entry vanished"),
            "{logs}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rotated_files_finds_what_rotation_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let cfg = RotationConfig {
            max_size: 20,
            keep_files: 3,
        };
        let line = "0123456789012345\n"; // 17 bytes: every append rotates
        for _ in 0..7 {
            append_with_rotation(&path, line, &cfg).unwrap();
        }
        // The lock file is left next to them; it is not one.
        assert!(sibling(&path, ".lock").exists());
        assert_eq!(
            rotated_files(&path),
            [
                sibling(&path, ".1"),
                sibling(&path, ".2"),
                sibling(&path, ".3")
            ]
        );
    }

    #[test]
    fn parse_size_rejects_overflow_to_infinity() {
        // A number too large for f64 parses to a non-finite value, not a size.
        assert!(parse_size(&"9".repeat(400)).is_err());
    }

    // --- same_file / normalize / lexically_normalize (issue #1694) ---

    #[test]
    fn same_file_matches_identical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        assert!(same_file(&path, &path));
    }

    #[test]
    fn same_file_matches_a_dot_dot_spelling_routed_through_a_nonexistent_directory() {
        // `metadata` fails for the dotted spelling (it must traverse the
        // nonexistent `sub` to apply `..`), forcing the fallback through
        // `lexically_normalize`, which collapses the `..` with no filesystem
        // access before canonicalizing what remains.
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit.jsonl");
        std::fs::write(&audit, "{}\n").unwrap();
        let dotted = dir.path().join("sub/../audit.jsonl");
        assert!(same_file(&audit, &dotted));
    }

    #[test]
    fn same_file_matches_a_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("audit.jsonl");
        std::fs::write(&original, "{}\n").unwrap();
        let linked = dir.path().join("linked.jsonl");
        std::fs::hard_link(&original, &linked).unwrap();
        assert!(same_file(&original, &linked));
    }

    #[cfg(unix)]
    #[test]
    fn same_file_matches_a_symlink_to_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("audit.jsonl");
        std::fs::write(&original, "{}\n").unwrap();
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        assert!(same_file(&original, &link));
    }

    #[cfg(unix)]
    #[test]
    fn same_file_matches_a_symlink_to_a_not_yet_existing_target() {
        // `fs::metadata` follows symlinks, so a link whose target does not
        // exist yet fails both `metadata` calls; the fallback must resolve
        // the link by hand rather than comparing the link's own name against
        // the target's, which would answer "different" right up until the
        // first write creates the target through the link.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("audit.jsonl"); // never created
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(same_file(&target, &link));
    }

    #[test]
    fn same_file_rejects_two_distinct_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        assert!(!same_file(&a, &b));
    }

    #[test]
    fn same_file_rejects_two_distinct_paths_that_both_do_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        assert!(!same_file(&a, &b));
    }

    #[test]
    fn lexically_normalize_collapses_dot_dot_without_touching_disk() {
        assert_eq!(
            lexically_normalize(Path::new("a/sub/../b")),
            PathBuf::from("a/b")
        );
        assert_eq!(
            lexically_normalize(Path::new("./a/./b")),
            PathBuf::from("a/b")
        );
        // A leading `..` has nothing to pop, and must be kept rather than
        // dropped or turned into an error.
        assert_eq!(
            lexically_normalize(Path::new("../a")),
            PathBuf::from("../a")
        );
    }

    #[test]
    fn normalize_returns_the_lexical_form_unchanged_when_there_is_nothing_left_to_canonicalize() {
        // An empty path has no file name and does not canonicalize; the
        // defensive fallback must return it unchanged rather than panicking
        // on a `None` `file_name()`.
        assert_eq!(normalize(Path::new("")), PathBuf::new());
    }

    #[test]
    fn prune_surfaces_a_read_error() {
        // Reading a directory as the log yields an error other than NotFound,
        // which prune propagates rather than treating as an empty log.
        let dir = tempfile::tempdir().unwrap();
        let result = prune(
            dir.path(),
            &PruneOptions {
                older_than: None,
                max_size: Some(1),
                dry_run: false,
            },
        );
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn append_with_rotation_appends_even_when_rotate_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // Seed a file already over the cap so the next write attempts to rotate.
        std::fs::write(&path, "0123456789012345\n").unwrap();
        // Make the rotation target a directory so `rename(log, log.1)` fails.
        std::fs::create_dir(sibling(&path, ".1")).unwrap();
        let cfg = RotationConfig {
            max_size: 5,
            keep_files: 1,
        };
        // Rotation fails, but the line is still appended (best effort).
        append_with_rotation(&path, "new-line\n", &cfg).unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("new-line"),
            "the record is appended despite the rotation failure"
        );
    }

    #[test]
    fn prune_cleans_up_temp_on_rewrite_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                http_line("a", "2999-01-01T00:00:00.000Z"),
                http_line("b", "2999-01-01T00:00:00.000Z"),
            ),
        )
        .unwrap();
        // Pre-create the exact temp path (same-process pid) as a directory so
        // the atomic rewrite's open fails, exercising the cleanup path.
        let tmp = sibling(&path, &format!(".prune.{}.tmp", std::process::id()));
        std::fs::create_dir(&tmp).unwrap();

        let result = prune(
            &path,
            &PruneOptions {
                older_than: None,
                max_size: Some(1),
                dry_run: false,
            },
        );
        assert!(result.is_err(), "a failing rewrite surfaces as an error");
        let _ = std::fs::remove_dir(&tmp);
    }

    #[tokio::test]
    async fn scope_origin_id_overwrites_id_but_preserves_source() {
        // A daemon-side base context: source = Daemon (so `via_daemon` detection
        // keeps working) with the daemon's own invocation id.
        let base = RequestLogContext {
            invocation_id: "daemon-1".to_string(),
            source: Source::Daemon,
            mcp_tool: None,
        };
        CTX.scope(base, async {
            scope_origin_id("cli-42".to_string(), async {
                let ctx = current_context();
                // Correlation id now points at the originating CLI invocation…
                assert_eq!(ctx.invocation_id, "cli-42");
                // …while the source stays Daemon, so `via_daemon` is unaffected.
                assert_eq!(ctx.source, Source::Daemon);
            })
            .await;
            // The override is scoped: outside it, the base id is restored.
            assert_eq!(current_context().invocation_id, "daemon-1");
        })
        .await;
    }
}
