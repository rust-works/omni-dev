//! Running a `<NAME>_COMMAND` secret helper (issue #2011,
//! [ADR-0090](../../../docs/adrs/adr-0090.md)).
//!
//! A `<NAME>_COMMAND` value names a program whose standard output is the
//! secret, in the style of git credential helpers and AWS `credential_process`.
//! The rules, each pinned by a test below:
//!
//! - The value is split into argv with [`shlex`] and run directly, **never
//!   through a shell**; a pipeline is written `sh -c '…'` explicitly.
//! - Standard input is `/dev/null`, standard output is captured (and capped),
//!   and standard error is captured rather than passed through. It appears in
//!   an error only when the command fails, escaped and truncated.
//! - The child inherits the environment minus every registered secret and its
//!   `_FILE`/`_COMMAND` companion, so a helper never receives sibling secrets.
//! - A timeout (default 60 s: a biometric prompt needs a human) kills the
//!   child. With no terminal attached (the daemon, the MCP server, a pipe) the
//!   helper runs in its own process group and the whole group is killed. With
//!   a terminal it stays in the foreground group, so a helper that prompts on
//!   `/dev/tty` (`pass`, `gpg`'s curses pinentry) is not stopped by `SIGTTIN`,
//!   and only the child itself is killed.
//! - A non-zero exit, output that is empty after trimming one trailing
//!   newline, or output that is not UTF-8 is an error.
//! - A success is cached per command string for a TTL (default 300 s), with
//!   concurrent callers sharing one run, so a burst of resolutions — `preflight`
//!   then the client build — prompts the user once.
//! - Errors and logs name the variable and the **program**, never the
//!   arguments (they can embed a token) and never standard output. A failing
//!   helper's own standard error is appended, escaped and truncated, because it
//!   is the only diagnostic; a helper that echoes its arguments there shows them.

use std::collections::HashMap;
use std::io::{IsTerminal, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::{command_var_name, file_var_name, SecretEnvError, SECRET_ENV_VARS};
use crate::utils::env::{non_empty_var, EnvSource};
use crate::utils::secret::Secret;

/// Variable overriding `DEFAULT_TIMEOUT_SECS`.
pub const TIMEOUT_VAR: &str = "OMNI_DEV_SECRET_COMMAND_TIMEOUT_SECS";

/// Variable overriding `DEFAULT_TTL_SECS`; `0` disables the cache.
pub const TTL_VAR: &str = "OMNI_DEV_SECRET_COMMAND_TTL_SECS";

/// How long a helper may run. Generous, because a biometric prompt needs a
/// human.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// How long a successful result is reused.
const DEFAULT_TTL_SECS: u64 = 300;

/// The most standard output accepted; a secret is a few hundred bytes, and
/// even a PEM private key is a few KiB.
const STDOUT_CAP: usize = 64 * 1024;

/// The most standard error kept while draining the pipe.
const STDERR_CAP: usize = 4 * 1024;

/// The most standard error shown in a failure message.
const STDERR_EXCERPT: usize = 512;

/// How long to wait for a pipe to reach end-of-file after the child has
/// exited, before settling for what was read (a grandchild can inherit it).
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// How often the exit of the child is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A helper's resource limits, read from the environment source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub(super) timeout: Duration,
    pub(super) ttl: Duration,
    /// Run the helper in its own process group, so a timeout can kill
    /// everything it forked. Off when a terminal is attached: a background
    /// group is stopped by `SIGTTIN` the moment it reads `/dev/tty`.
    pub(super) own_group: bool,
}

impl Limits {
    /// Reads [`TIMEOUT_VAR`] and [`TTL_VAR`] through `env`, warning about and
    /// replacing an unusable value with the default.
    pub(super) fn from_env(env: &impl EnvSource) -> Self {
        Self {
            timeout: Duration::from_secs(secs(env, TIMEOUT_VAR, DEFAULT_TIMEOUT_SECS, false)),
            ttl: Duration::from_secs(secs(env, TTL_VAR, DEFAULT_TTL_SECS, true)),
            own_group: !std::io::stdin().is_terminal(),
        }
    }
}

/// Parses `var` as whole seconds; a malformed value (or a zero where zero is
/// meaningless) warns and yields `default`.
fn secs(env: &impl EnvSource, var: &str, default: u64, zero_ok: bool) -> u64 {
    let Some(raw) = non_empty_var(env, var) else {
        return default;
    };
    match raw.trim().parse::<u64>() {
        Ok(0) if !zero_ok => {
            tracing::warn!("{var}=0 is not a usable value; using the default of {default}s");
            default
        }
        Ok(value) => value,
        Err(_) => {
            tracing::warn!(
                "{var}={raw:?} is not a whole number of seconds; using the default of {default}s"
            );
            default
        }
    }
}

/// Runs the helper `command`, named by `command_var`, and returns its output
/// as the secret — from the cache when a fresh result is held.
///
/// Blocks for as long as the helper runs (up to the timeout). On a
/// multi-thread tokio runtime the wait is announced with `block_in_place`, so
/// an async caller does not starve its worker's neighbours.
///
/// # Errors
///
/// See [`SecretEnvError`]: unparseable command, spawn failure, non-zero exit,
/// timeout, oversized, empty or non-UTF-8 output.
pub(super) fn resolve(
    command_var: &str,
    command: &str,
    limits: Limits,
) -> Result<Secret, SecretEnvError> {
    let argv = split(command_var, command)?;
    blocking(|| {
        if limits.ttl.is_zero() {
            run(command_var, &argv, limits)
        } else {
            cached(command, limits.ttl, || run(command_var, &argv, limits))
        }
    })
}

/// Runs `f`, telling a multi-thread tokio runtime that this thread is about
/// to block.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// Splits `command` into argv, refusing an empty or unbalanced one.
fn split(command_var: &str, command: &str) -> Result<Vec<String>, SecretEnvError> {
    let refused = |reason| SecretEnvError::CommandSplit {
        command_var: command_var.to_string(),
        reason,
    };
    match shlex::split(command) {
        None => Err(refused(
            "it has an unbalanced quote or a trailing backslash",
        )),
        Some(argv) if argv.is_empty() => Err(refused("it names no program")),
        Some(argv) => Ok(argv),
    }
}

/// One cached result: when it was fetched and what it was.
type Slot = Arc<Mutex<Option<(Instant, Secret)>>>;

/// The process-wide cache, keyed by the command string.
fn cache() -> &'static Mutex<HashMap<String, Slot>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns the fresh cached result for `key`, or runs `fetch` and caches a
/// success, forgetting results that have expired. The per-key slot stays locked while `fetch` runs, so concurrent
/// callers wait for one run rather than each prompting. Failures are not
/// cached.
fn cached(
    key: &str,
    ttl: Duration,
    fetch: impl FnOnce() -> Result<Secret, SecretEnvError>,
) -> Result<Secret, SecretEnvError> {
    let slot = {
        let mut map = cache().lock().unwrap_or_else(PoisonError::into_inner);
        // Drop expired results (and never-filled slots) so a secret that was
        // rotated away is not kept in memory for the life of a long-lived
        // process. A slot someone is filling right now is locked: keep it.
        map.retain(|other, slot| {
            other == key
                || slot.try_lock().map_or(true, |held| {
                    held.as_ref()
                        .is_some_and(|(fetched, _)| fetched.elapsed() < ttl)
                })
        });
        map.entry(key.to_string()).or_default().clone()
    };
    let mut held = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((fetched, secret)) = held.as_ref() {
        if fetched.elapsed() < ttl {
            return Ok(secret.clone());
        }
    }
    let secret = fetch()?;
    *held = Some((Instant::now(), secret.clone()));
    Ok(secret)
}

/// Builds the helper's [`Command`]: argv, null stdin, piped output, its own
/// process group when `own_group`, and no registered secret in its environment.
fn build(argv: &[String], own_group: bool) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `SNOWFLAKE_PRIVATE_KEY_PATH` is the legacy alias of a registered secret
    // (ADR-0089 point 7); it names the key's file, so it goes too.
    for name in SECRET_ENV_VARS
        .iter()
        .chain(&["SNOWFLAKE_PRIVATE_KEY_PATH"])
    {
        command
            .env_remove(name)
            .env_remove(file_var_name(name))
            .env_remove(command_var_name(name));
    }
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt;
        // A helper such as `op` or a wrapper script forks; the group lets a
        // timeout kill all of it.
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = own_group;
    command
}

/// Runs `argv` to completion within `timeout` and turns its output into a
/// secret.
fn run(command_var: &str, argv: &[String], limits: Limits) -> Result<Secret, SecretEnvError> {
    let Limits {
        timeout, own_group, ..
    } = limits;
    let program = argv[0].clone();
    tracing::debug!(command_var, program = %program, "running a secret command");
    let mut child =
        build(argv, own_group)
            .spawn()
            .map_err(|source| SecretEnvError::CommandSpawn {
                command_var: command_var.to_string(),
                program: program.clone(),
                hint: if source.kind() == std::io::ErrorKind::NotFound
                    && !std::path::Path::new(&program).is_absolute()
                {
                    "; give an absolute path if omni-dev runs as a daemon, whose PATH is minimal"
                } else {
                    ""
                },
                source,
            })?;
    let stdout = capture(child.stdout.take(), STDOUT_CAP);
    let stderr = capture(child.stderr.take(), STDERR_CAP);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                kill_and_reap(&mut child, own_group);
                return Err(SecretEnvError::CommandTimedOut {
                    command_var: command_var.to_string(),
                    program,
                    secs: timeout.as_secs(),
                });
            }
            // patchcov: coverage ignore reason="try_wait on a live, owned child fails only if waitpid itself errors (ECHILD/EINTR from outside the process); no in-process test can provoke it, and the arm only reaps and reports"
            Err(source) => {
                kill_and_reap(&mut child, own_group);
                return Err(SecretEnvError::CommandSpawn {
                    command_var: command_var.to_string(),
                    program,
                    hint: "",
                    source,
                });
            } // patchcov: coverage end
        }
    };
    let stdout = stdout.finish();
    let stderr = stderr.finish();

    if !status.success() {
        return Err(SecretEnvError::CommandFailed {
            command_var: command_var.to_string(),
            program,
            status: describe(status),
            detail: excerpt(&stderr),
        });
    }
    if stdout.len() > STDOUT_CAP {
        return Err(SecretEnvError::CommandOutputTooLarge {
            command_var: command_var.to_string(),
            program,
            limit: STDOUT_CAP,
        });
    }
    let mut text = String::from_utf8(stdout).map_err(|_| SecretEnvError::CommandNotUtf8 {
        command_var: command_var.to_string(),
        program: program.clone(),
    })?;
    super::trim_one_newline(&mut text);
    if text.is_empty() {
        return Err(SecretEnvError::CommandEmptyOutput {
            command_var: command_var.to_string(),
            program,
        });
    }
    Ok(Secret::new(text))
}

/// Kills the child's process group when it has one (Unix), else the child
/// alone, then reaps it.
fn kill_and_reap(child: &mut Child, own_group: bool) {
    if !(own_group && kill_group(child)) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// SIGKILLs the child's process group; whether it went (or was already gone).
#[cfg(unix)]
fn kill_group(child: &Child) -> bool {
    // PIDs always fit in i32: Linux caps at ~2^22, macOS at 99999.
    let group = nix::unistd::Pid::from_raw(child.id() as i32);
    group_kill_succeeded(nix::sys::signal::killpg(
        group,
        nix::sys::signal::Signal::SIGKILL,
    ))
}

/// Whether a `killpg` result means the group is gone; an error is logged and
/// sends the caller to killing the child directly.
#[cfg(unix)]
fn group_kill_succeeded(result: nix::Result<()>) -> bool {
    match result {
        // ESRCH: the group had already gone.
        Ok(()) | Err(nix::errno::Errno::ESRCH) => true,
        Err(e) => {
            tracing::debug!(error = %e, "killpg failed; killing the child directly");
            false
        }
    }
}

/// Windows has no process groups to signal.
#[cfg(not(unix))]
fn kill_group(_child: &Child) -> bool {
    false
}

/// A pipe being drained on its own thread, so a chatty child can never fill
/// it and block.
struct Capture {
    buf: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

/// Drains `pipe` on a thread, keeping at most `cap + 1` bytes (the extra byte
/// tells an over-long output from one exactly `cap` long) and discarding the
/// rest.
fn capture<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> Capture {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (tx, done) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        let sink = Arc::clone(&buf);
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut held = sink.lock().unwrap_or_else(PoisonError::into_inner);
                let room = (cap + 1).saturating_sub(held.len());
                held.extend_from_slice(&chunk[..n.min(room)]);
            }
            let _ = tx.send(());
        });
    } else {
        let _ = tx.send(());
    }
    Capture { buf, done }
}

impl Capture {
    /// Waits up to [`PIPE_GRACE`] for end-of-file, then returns what was read.
    fn finish(self) -> Vec<u8> {
        let _ = self.done.recv_timeout(PIPE_GRACE);
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `exit code N`, or the signal that ended the process.
fn describe(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "no exit code".to_string(),
    }
}

/// The failure detail for a helper's standard error: `""` when it wrote none,
/// otherwise `: <text>` with control characters escaped and the length capped.
fn excerpt(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::from(": ");
    for c in text.chars() {
        let piece = if c.is_control() {
            c.escape_default().to_string()
        } else {
            c.to_string()
        };
        if out.len() - 2 + piece.len() > STDERR_EXCERPT {
            out.push('…');
            break;
        }
        out.push_str(&piece);
    }
    out
}

// The tests run real helpers through `/bin/sh`.
#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    const VAR: &str = "DATADOG_API_KEY_COMMAND";

    /// A `/bin/sh -c <script>` command line, with `script` quoted.
    fn sh(script: &str) -> String {
        format!("/bin/sh -c {}", shlex::try_quote(script).unwrap())
    }

    #[test]
    fn a_failed_killpg_falls_back_to_killing_the_child_directly() {
        use nix::errno::Errno;
        assert!(group_kill_succeeded(Ok(())));
        assert!(group_kill_succeeded(Err(Errno::ESRCH)));
        assert!(!group_kill_succeeded(Err(Errno::EPERM)));
    }

    #[test]
    fn capture_without_a_pipe_finishes_empty() {
        let none = capture::<std::io::Empty>(None, 16);
        assert!(none.finish().is_empty());
    }

    #[test]
    fn a_status_with_neither_code_nor_signal_is_described_plainly() {
        use std::os::unix::process::ExitStatusExt;
        // A stopped (not exited, not signalled) wait status: 0x7f.
        assert_eq!(describe(ExitStatus::from_raw(0x7f)), "no exit code");
    }

    fn limits(timeout_secs: u64) -> Limits {
        Limits {
            timeout: Duration::from_secs(timeout_secs),
            ttl: Duration::ZERO,
            own_group: true,
        }
    }

    fn resolve_now(command: &str) -> Result<String, SecretEnvError> {
        resolve(VAR, command, limits(30)).map(|s| s.expose_secret().to_string())
    }

    #[test]
    fn stdout_is_the_secret_and_one_trailing_newline_is_trimmed() {
        assert_eq!(resolve_now(&sh("printf 'tok\\n'")).unwrap(), "tok");
        assert_eq!(resolve_now(&sh("printf 'tok\\r\\n'")).unwrap(), "tok");
        // Only one: any other whitespace is part of the secret.
        assert_eq!(resolve_now(&sh("printf ' tok \\n\\n'")).unwrap(), " tok \n");
    }

    #[test]
    fn the_command_is_not_run_through_a_shell() {
        // Without a shell, `;` and `$(…)` are plain arguments to printf.
        let got = resolve_now("/usr/bin/printf %s 'a;b $(echo x)'").unwrap();
        assert_eq!(got, "a;b $(echo x)");
    }

    #[test]
    fn quoted_arguments_keep_their_spaces() {
        assert_eq!(
            resolve_now(r#"/usr/bin/printf %s "two words""#).unwrap(),
            "two words"
        );
    }

    #[test]
    fn an_unbalanced_quote_is_refused_before_anything_runs() {
        let err = resolve_now("/usr/bin/printf 'oops").unwrap_err();
        assert!(matches!(err, SecretEnvError::CommandSplit { .. }), "{err}");
        assert!(err.to_string().contains(VAR), "{err}");
    }

    #[test]
    fn a_blank_command_is_refused() {
        let err = resolve_now("   ").unwrap_err();
        assert!(matches!(err, SecretEnvError::CommandSplit { .. }), "{err}");
    }

    #[test]
    fn a_missing_bare_program_names_it_and_hints_at_an_absolute_path() {
        let err = resolve_now("omni-dev-no-such-helper --flag").unwrap_err();
        assert!(matches!(err, SecretEnvError::CommandSpawn { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("omni-dev-no-such-helper"), "{text}");
        assert!(text.contains("absolute path"), "{text}");
        assert!(!text.contains("--flag"), "arguments leaked: {text}");
    }

    #[test]
    fn a_missing_absolute_program_does_not_advise_what_it_already_did() {
        let text = resolve_now("/nonexistent/omni-dev-helper")
            .unwrap_err()
            .to_string();
        assert!(text.contains("/nonexistent/omni-dev-helper"), "{text}");
        assert!(!text.contains("absolute path"), "{text}");
    }

    #[test]
    fn a_non_zero_exit_fails_with_the_code_and_the_escaped_stderr() {
        let err = resolve_now(&sh("printf 'not signed in\\tplease\\n' >&2; exit 3")).unwrap_err();
        assert!(matches!(err, SecretEnvError::CommandFailed { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("exit code 3"), "{text}");
        assert!(text.contains("not signed in\\tplease"), "{text}");
        assert!(!text.contains('\t'), "control characters must be escaped");
    }

    #[test]
    fn a_failure_does_not_echo_stdout() {
        let err = resolve_now(&sh("printf hunter2; exit 1")).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn stderr_is_dropped_when_the_command_succeeds() {
        assert_eq!(
            resolve_now(&sh("printf tok; printf noise >&2")).unwrap(),
            "tok"
        );
    }

    #[test]
    fn a_long_stderr_is_truncated_in_the_message() {
        let err = resolve_now(&sh("head -c 5000 /dev/zero | tr '\\0' e >&2; exit 1")).unwrap_err();
        let text = err.to_string();
        assert!(text.contains('…'), "{text}");
        assert!(text.len() < 1000, "{} bytes", text.len());
    }

    #[test]
    fn empty_output_is_an_error_not_unset() {
        for script in ["printf ''", "printf '\\n'"] {
            let err = resolve_now(&sh(script)).unwrap_err();
            assert!(
                matches!(err, SecretEnvError::CommandEmptyOutput { .. }),
                "{err}"
            );
        }
    }

    #[test]
    fn non_utf8_output_is_an_error() {
        let err = resolve_now(&sh("printf '\\377\\376'")).unwrap_err();
        assert!(
            matches!(err, SecretEnvError::CommandNotUtf8 { .. }),
            "{err}"
        );
    }

    #[test]
    fn output_over_the_cap_is_an_error() {
        let script = format!("head -c {} /dev/zero | tr '\\0' a", STDOUT_CAP + 1);
        let err = resolve_now(&sh(&script)).unwrap_err();
        assert!(
            matches!(err, SecretEnvError::CommandOutputTooLarge { .. }),
            "{err}"
        );
        // Exactly the cap is accepted.
        let script = format!("head -c {STDOUT_CAP} /dev/zero | tr '\\0' a");
        assert_eq!(resolve_now(&sh(&script)).unwrap().len(), STDOUT_CAP);
    }

    #[test]
    fn stdin_is_closed() {
        // `cat` returns at once on /dev/null; with an inherited stdin it would
        // block until the timeout.
        let err = resolve_now("/bin/cat").unwrap_err();
        assert!(
            matches!(err, SecretEnvError::CommandEmptyOutput { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_slow_command_times_out_and_its_whole_group_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // The backgrounded grandchild would write the marker if it lived.
        let script = format!(
            "(sleep 3; touch {}) & sleep 30; printf tok",
            marker.display()
        );
        let started = Instant::now();
        let err = resolve(VAR, &sh(&script), limits(1)).unwrap_err();
        assert!(
            matches!(err, SecretEnvError::CommandTimedOut { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("1s"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a grandchild outlived the timeout");
    }

    #[test]
    fn a_timeout_without_its_own_group_still_kills_the_child() {
        // The terminal-attached mode: no process group, so only the child dies.
        let limits = Limits {
            own_group: false,
            ..limits(1)
        };
        let started = Instant::now();
        let err = resolve(VAR, "/bin/sleep 30", limits).unwrap_err();
        assert!(
            matches!(err, SecretEnvError::CommandTimedOut { .. }),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_foreground_group_helper_still_returns_its_output() {
        let limits = Limits {
            own_group: false,
            ..limits(30)
        };
        let got = resolve(VAR, &sh("printf tok"), limits).unwrap();
        assert_eq!(got.expose_secret(), "tok");
    }

    #[test]
    fn a_signalled_command_reports_the_signal() {
        let err = resolve_now(&sh("kill -9 $$")).unwrap_err();
        assert!(err.to_string().contains("signal 9"), "{err}");
    }

    #[test]
    fn the_child_environment_drops_every_secret_and_companion() {
        let command = build(&["/bin/true".to_string()], true);
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for name in SECRET_ENV_VARS {
            for var in [
                (*name).to_string(),
                file_var_name(name),
                command_var_name(name),
            ] {
                assert!(removed.contains(&var), "{var} reaches the helper");
            }
        }
        assert!(removed.contains(&"SNOWFLAKE_PRIVATE_KEY_PATH".to_string()));
    }

    #[test]
    fn the_limits_default_and_read_the_env_source() {
        let defaults = Limits::from_env(&MapEnv::new());
        assert_eq!(defaults.timeout, Duration::from_secs(60));
        assert_eq!(defaults.ttl, Duration::from_secs(300));
        let env = MapEnv::new().with(TIMEOUT_VAR, "5").with(TTL_VAR, "0");
        let limits = Limits::from_env(&env);
        assert_eq!(limits.timeout, Duration::from_secs(5));
        assert_eq!(limits.ttl, Duration::ZERO, "0 disables the cache");
    }

    #[test]
    fn unusable_limits_fall_back_to_the_defaults() {
        let env = MapEnv::new().with(TIMEOUT_VAR, "0").with(TTL_VAR, "soon");
        let limits = Limits::from_env(&env);
        assert_eq!(limits.timeout, Duration::from_secs(60), "0s never runs");
        assert_eq!(limits.ttl, Duration::from_secs(300));
    }

    // ── cache ──
    //
    // The cache is process-wide, so each test keys it with a command (or key)
    // unique to itself — a temp-dir path — instead of clearing it under the
    // feet of the tests running beside it.

    /// A command that appends a line to `marker` each time it really runs.
    fn counting(marker: &std::path::Path) -> String {
        sh(&format!("echo run >> {}; printf tok", marker.display()))
    }

    fn runs(marker: &std::path::Path) -> usize {
        std::fs::read_to_string(marker).map_or(0, |t| t.lines().count())
    }

    #[test]
    fn a_fresh_result_is_reused_within_the_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs");
        let command = counting(&marker);
        let limits = Limits {
            timeout: Duration::from_secs(30),
            ttl: Duration::from_secs(60),
            own_group: true,
        };
        for _ in 0..3 {
            assert_eq!(
                resolve(VAR, &command, limits).unwrap().expose_secret(),
                "tok"
            );
        }
        assert_eq!(runs(&marker), 1);
    }

    #[test]
    fn a_zero_ttl_never_caches() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs");
        let command = counting(&marker);
        for _ in 0..3 {
            resolve(VAR, &command, limits(30)).unwrap();
        }
        assert_eq!(runs(&marker), 3);
    }

    #[test]
    fn an_expired_result_is_fetched_again() {
        let mut calls = 0;
        let mut fetch = || {
            calls += 1;
            Ok(Secret::new(format!("v{calls}")))
        };
        let ttl = Duration::from_millis(50);
        let key = "cache-expiry-test";
        assert_eq!(cached(key, ttl, &mut fetch).unwrap().expose_secret(), "v1");
        assert_eq!(cached(key, ttl, &mut fetch).unwrap().expose_secret(), "v1");
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(cached(key, ttl, &mut fetch).unwrap().expose_secret(), "v2");
    }

    #[test]
    fn expired_results_are_forgotten_when_another_command_is_cached() {
        let ttl = Duration::from_millis(30);
        let old = "sweep-test-old";
        cached(old, ttl, || Ok(Secret::new("old"))).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        cached("sweep-test-new", ttl, || Ok(Secret::new("new"))).unwrap();
        let held = cache().lock().unwrap();
        assert!(!held.contains_key(old), "an expired result is still held");
        assert!(held.contains_key("sweep-test-new"));
    }

    #[test]
    fn a_failure_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("ok");
        let command = sh(&format!("test -e {} && printf tok", flag.display()));
        let limits = Limits {
            timeout: Duration::from_secs(30),
            ttl: Duration::from_secs(60),
            own_group: true,
        };
        assert!(resolve(VAR, &command, limits).is_err());
        std::fs::write(&flag, "").unwrap();
        assert_eq!(
            resolve(VAR, &command, limits).unwrap().expose_secret(),
            "tok"
        );
    }

    #[test]
    fn concurrent_callers_share_one_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs");
        // Slow enough that every thread arrives while the first still runs.
        let command = sh(&format!(
            "echo run >> {}; sleep 1; printf tok",
            marker.display()
        ));
        let limits = Limits {
            timeout: Duration::from_secs(30),
            ttl: Duration::from_secs(60),
            own_group: true,
        };
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let command = command.clone();
                std::thread::spawn(move || {
                    resolve(VAR, &command, limits)
                        .unwrap()
                        .expose_secret()
                        .to_string()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), "tok");
        }
        assert_eq!(runs(&marker), 1, "callers each ran the helper");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolving_on_a_multi_thread_runtime_works() {
        assert_eq!(resolve_now(&sh("printf tok")).unwrap(), "tok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolving_on_a_current_thread_runtime_works() {
        assert_eq!(resolve_now(&sh("printf tok")).unwrap(), "tok");
    }
}
