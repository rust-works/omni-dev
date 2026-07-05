//! Linux systemd **user**-unit integration for `omni-dev daemon start` / `stop`
//! and socket activation — the Linux mirror of the macOS `launchd` module.
//!
//! `start` writes a socket-activated `.socket` + `.service` pair under
//! `~/.config/systemd/user/` and enables the socket. Like the launchd model this
//! is **socket-activated**: systemd owns the control socket (declared in the
//! `.socket` unit's `ListenStream`) and spawns `omni-dev daemon run` the first
//! time a client connects, handing it the listening file descriptor via the
//! `sd_listen_fds` protocol ([`systemd_listener`]). There is no `Restart=` —
//! on-demand activation *is* the model, so a crashed daemon is re-activated on
//! the next connect for free, and `enable`-ing the socket into `sockets.target`
//! is what makes it come up at login. A clean `daemon stop` stops and disables
//! the socket so the daemon stays down. See ADR-0039 and issue #1174.
//!
//! When systemd is not managing the session (containers, non-systemd distros, or
//! the `OMNI_DEV_DAEMON_DISABLE_SYSTEMD` escape hatch), [`is_available`] returns
//! `false` and the caller falls back to the detached-spawn launcher.

use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::net::UnixListener;

use super::paths;

/// The socket-activated `.service` unit filename.
pub const SERVICE_UNIT: &str = "omni-dev-daemon.service";

/// The demand-activating `.socket` unit filename.
pub const SOCKET_UNIT: &str = "omni-dev-daemon.socket";

/// When set to a truthy value (`1`/`true`/`yes`/`on`), forces the detached-spawn
/// launcher even where systemd *is* available. Used by tests (so `daemon start`
/// does not install a real unit under the developer's `~/.config`) and by users
/// who prefer the old behavior.
const DISABLE_ENV: &str = "OMNI_DEV_DAEMON_DISABLE_SYSTEMD";

/// The per-user systemd unit directory (`~/.config/systemd/user`, honoring
/// `XDG_CONFIG_HOME`).
fn unit_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(base.join("systemd").join("user"))
}

/// Whether `OMNI_DEV_DAEMON_DISABLE_SYSTEMD` is set to a truthy value.
fn systemd_disabled() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|v| flag_is_truthy(&v))
}

/// Whether an environment-flag string counts as "on" (`1`/`true`/`yes`/`on`,
/// case- and surrounding-whitespace-insensitive).
fn flag_is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Whether a systemd **user** manager is running and can host the daemon.
///
/// A fast, subprocess-free probe: the system must be booted with systemd
/// (`/run/systemd/system` exists) *and* the current user's manager control
/// socket (`$XDG_RUNTIME_DIR/systemd/private`, defaulting to
/// `/run/user/<uid>/systemd/private`) must be present. Returns `false` under the
/// `OMNI_DEV_DAEMON_DISABLE_SYSTEMD` escape hatch. When this is `false` the
/// caller falls back to a detached `daemon run` spawn.
pub fn is_available() -> bool {
    if systemd_disabled() {
        return false;
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return false;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
        || PathBuf::from(format!("/run/user/{}", nix::unistd::getuid())),
        PathBuf::from,
    );
    runtime.join("systemd").join("private").exists()
}

/// Validates that a path can be faithfully placed in a UTF-8 systemd unit file
/// on a single directive line, returning it as `&str`.
///
/// A non-UTF-8 path cannot be represented in the unit file, and a path with an
/// embedded newline would corrupt the single-line `ExecStart=`/`ListenStream=`
/// directive — both are rejected here rather than silently mangled, mirroring
/// launchd's non-UTF-8 rejection (issue #991).
fn validate_path<'a>(path: &'a Path, what: &str) -> Result<&'a str> {
    let s = path
        .to_str()
        .with_context(|| format!("{what} path is not valid UTF-8: {}", path.display()))?;
    if s.contains(['\n', '\r']) {
        bail!("{what} path contains a newline, which cannot be placed in a systemd unit: {s}");
    }
    Ok(s)
}

/// Escapes a value for a systemd command line (`ExecStart=`) argument: wraps it
/// in double quotes and escapes every character systemd would otherwise
/// interpret. `ExecStart` is whitespace-split into arguments, so a path with a
/// space *must* be quoted; inside double quotes systemd applies C-style
/// backslash unescaping (`\\`, `\"`), textual specifier expansion (`%%` → `%`),
/// and environment expansion (`$$` → `$`).
fn exec_arg(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\") // backslash — the double-quote C-unescape metachar
        .replace('"', "\\\"") // embedded double quote
        .replace('%', "%%") // systemd specifier
        .replace('$', "$$"); // environment expansion
    format!("\"{escaped}\"")
}

/// Escapes a systemd config value taken literally to end-of-line (e.g.
/// `ListenStream=`): only the `%` specifier is special there (no word-splitting,
/// no `$` expansion).
fn escape_value(s: &str) -> String {
    s.replace('%', "%%")
}

/// Renders the socket-activated `.service` unit that execs
/// `omni-dev daemon run --socket <socket>`.
///
/// It carries no `[Install]` section and no `Restart=`: the daemon is activated
/// solely by the companion `.socket` unit (the systemd analogue of launchd's
/// no-`KeepAlive` socket-activation model). The `--socket` argument mirrors the
/// `.socket` unit's `ListenStream` path so the daemon resolves the same path for
/// its co-located bridge token file even though it binds the inherited fd.
fn render_service(exe: &Path, socket: &Path) -> Result<String> {
    let exe = validate_path(exe, "executable")?;
    let socket = validate_path(socket, "socket")?;
    Ok(format!(
        r"[Unit]
Description=omni-dev daemon
Requires={socket_unit}
After={socket_unit}

[Service]
ExecStart={exe} daemon run --socket {socket}
",
        socket_unit = SOCKET_UNIT,
        exe = exec_arg(exe),
        socket = exec_arg(socket),
    ))
}

/// Renders the demand-activating `.socket` unit.
///
/// `WantedBy=sockets.target` is what `systemctl --user enable` hooks into the
/// login startup sequence; `SocketMode=0600` makes systemd create the control
/// socket owner-private from birth (the analogue of launchd's `SockPathMode`
/// 384). systemd owns and creates the socket at `ListenStream` before the daemon
/// process runs.
fn render_socket(socket: &Path) -> Result<String> {
    let socket = validate_path(socket, "socket")?;
    Ok(format!(
        r"[Unit]
Description=omni-dev daemon control socket

[Socket]
ListenStream={socket}
SocketMode=0600

[Install]
WantedBy=sockets.target
",
        socket = escape_value(socket),
    ))
}

/// A `systemctl --user …` command builder.
fn systemctl() -> Command {
    let mut command = Command::new("systemctl");
    command.arg("--user");
    command
}

/// Runs `systemctl --user daemon-reload` so a freshly written unit is picked up.
fn daemon_reload() -> Result<()> {
    let output = systemctl()
        .arg("daemon-reload")
        .output()
        .context("failed to run `systemctl --user daemon-reload`")?;
    if !output.status.success() {
        bail!(
            "`systemctl --user daemon-reload` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Enables and starts the `.socket` unit (auto-start at login via
/// `sockets.target`, plus arming the demand socket now), retrying a few times on
/// a transient failure the way launchd's `bootstrap` does.
fn enable_now() -> Result<()> {
    let mut last_err = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let output = systemctl()
            .args(["enable", "--now", SOCKET_UNIT])
            .output()
            .context("failed to run `systemctl --user enable --now`")?;
        if output.status.success() {
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    bail!("`systemctl --user enable --now {SOCKET_UNIT}` failed: {last_err}");
}

/// Writes the unit files and enables the socket so systemd listens on the demand
/// socket and spawns the daemon on the first client connect (and at login).
pub fn install_and_load(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve the current executable")?;
    let dir = unit_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let service_unit = dir.join(SERVICE_UNIT);
    let socket_unit = dir.join(SOCKET_UNIT);
    std::fs::write(&service_unit, render_service(&exe, socket)?)
        .with_context(|| format!("failed to write {}", service_unit.display()))?;
    std::fs::write(&socket_unit, render_socket(socket)?)
        .with_context(|| format!("failed to write {}", socket_unit.display()))?;

    // systemd creates the demand socket at `ListenStream` when the socket unit
    // starts — *before* our process runs — and does not create missing parent
    // directories. Ensure the owner-only (`0700`) runtime directory exists now so
    // that socket creation succeeds (the same reason launchd needs it, #1081).
    if let Some(parent) = socket.parent() {
        paths::ensure_dir_0700(parent)?;
    }

    daemon_reload()?;
    enable_now()
}

/// Best-effort `systemctl --user …`: a non-zero exit is ignored (the common
/// "already stopped / not enabled" case), but a *spawn* failure (systemd absent)
/// is logged rather than discarded so `stop` does not silently claim to have
/// disabled auto-start when it had not — the same posture as launchd's `bootout`
/// (issue #996).
fn run_best_effort(args: &[&str]) {
    if let Err(e) = systemctl().args(args).output() {
        tracing::warn!("failed to run `systemctl --user {}`: {e}", args.join(" "));
    }
}

/// Stops and disables the socket (and stops any running daemon) so it is not
/// re-activated on the next client connect or at the next login.
///
/// The systemd analogue of launchd's `bootout`. Best-effort: a not-installed
/// unit is not an error. The unit files are left on disk (a disabled unit does
/// not auto-start); a later `daemon start` rewrites and re-enables them.
pub fn unload() -> Result<()> {
    // Stop the socket first so it is disarmed before the service is torn down;
    // then drop the login-time enablement.
    run_best_effort(&["stop", SOCKET_UNIT, SERVICE_UNIT]);
    run_best_effort(&["disable", SOCKET_UNIT]);
    Ok(())
}

/// Sets `FD_CLOEXEC` on `fd` so the inherited listening socket is not leaked into
/// the child processes the daemon later spawns (git, `claude` CLI, `code`).
///
/// `fcntl(F_SETFD)` operates on the single descriptor and is thread-safe (no
/// process-global state), unlike the `LISTEN_*` environment variables which we
/// deliberately leave in place — clearing them mid-runtime would be an unsafe
/// process-global env write, and our children ignore `LISTEN_PID` (it names our
/// pid) regardless.
#[allow(unsafe_code)]
fn set_cloexec(fd: RawFd) -> Result<()> {
    use nix::libc;

    // SAFETY: `F_GETFD`/`F_SETFD` on a valid descriptor are simple integer
    // syscalls with no memory effects; we check the -1 error return each time.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error())
            .context("F_GETFD on the systemd-activated socket failed");
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error())
            .context("setting FD_CLOEXEC on the systemd-activated socket failed");
    }
    Ok(())
}

/// Adopts the listening socket systemd created for us when the daemon was
/// **socket-activated** (the `.socket` unit's single `ListenStream`), via the
/// `sd_listen_fds` protocol reimplemented directly (no libsystemd dependency).
///
/// Returns `Ok(None)` when the process was *not* launched by systemd with an
/// activation socket — the manual / dev / CI case (`omni-dev daemon run` from a
/// shell) or the detached-spawn fallback — where the caller binds the socket
/// itself via [`single_instance::bind_or_reclaim`](super::single_instance::bind_or_reclaim).
///
/// systemd passes activation fds starting at descriptor `3`, sets `LISTEN_FDS` to
/// the count, and sets `LISTEN_PID` to the service's main pid so an inherited-but-
/// stale value from a parent is ignored. We declare exactly one `ListenStream`,
/// so we adopt descriptor `3`.
#[allow(unsafe_code)]
pub(crate) fn systemd_listener() -> Result<Option<UnixListener>> {
    /// The first descriptor systemd passes for socket activation.
    const SD_LISTEN_FDS_START: RawFd = 3;

    // The activation fds are ours only if `LISTEN_PID` names this process.
    let for_us = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|pid| pid.parse::<u32>().ok())
        .is_some_and(|pid| pid == std::process::id());
    if !for_us {
        return Ok(None);
    }
    let count = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse::<i32>().ok())
        .unwrap_or(0);
    if count < 1 {
        return Ok(None);
    }

    let raw = SD_LISTEN_FDS_START;
    set_cloexec(raw)?;

    // SAFETY: `raw` (descriptor 3) is a listening Unix-domain socket handed off by
    // systemd and now owned solely by us; adopting it into a std listener transfers
    // ownership (closed on drop). It is converted to a Tokio listener after being
    // set non-blocking, as the runtime requires.
    let std_listener = unsafe { StdUnixListener::from_raw_fd(raw) };
    std_listener
        .set_nonblocking(true)
        .context("failed to set the systemd socket non-blocking")?;
    let listener = UnixListener::from_std(std_listener)
        .context("failed to adopt the systemd socket into the Tokio runtime")?;
    Ok(Some(listener))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Asserts a rendered unit is structurally well-formed: every non-blank line
    /// is a `[Section]` header or a `Key=Value` directive. This is the property
    /// that matters — a stray newline smuggled in from a path would produce a
    /// line that is neither, corrupting the unit.
    fn assert_well_formed(unit: &str) {
        for line in unit.lines() {
            if line.is_empty() {
                continue;
            }
            let is_section = line.starts_with('[') && line.ends_with(']');
            let is_directive = line.contains('=');
            assert!(
                is_section || is_directive,
                "malformed systemd unit line: {line:?}\n---\n{unit}"
            );
        }
    }

    #[test]
    fn renders_plain_paths_verbatim() {
        let service = render_service(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        assert!(
            service.contains(
                "ExecStart=\"/usr/local/bin/omni-dev\" daemon run --socket \"/tmp/omni-dev/daemon.sock\""
            ),
            "{service}"
        );
        assert!(
            service.contains(&format!("Requires={SOCKET_UNIT}")),
            "{service}"
        );
        assert_well_formed(&service);
    }

    #[test]
    fn service_has_no_install_or_restart() {
        // Activation is the socket's job: the service must not restart on its own
        // or install itself into a login target.
        let service = render_service(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        assert!(!service.contains("Restart"), "{service}");
        assert!(!service.contains("KeepAlive"), "{service}");
        assert!(!service.contains("[Install]"), "{service}");
    }

    #[test]
    fn exec_start_quotes_paths_with_spaces() {
        // A space in a path must be quoted or systemd would split it into two
        // arguments.
        let service = render_service(
            Path::new("/opt/A B/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("spaced path renders");

        assert!(
            service.contains("ExecStart=\"/opt/A B/omni-dev\" daemon run"),
            "{service}"
        );
        assert_well_formed(&service);
    }

    #[test]
    fn escapes_percent_specifier() {
        // `%` is systemd's specifier prefix and must be doubled everywhere.
        let service = render_service(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/50%/daemon.sock"),
        )
        .expect("percent path renders");
        assert!(
            service.contains("--socket \"/tmp/50%%/daemon.sock\""),
            "{service}"
        );

        let socket = render_socket(Path::new("/tmp/50%/daemon.sock")).expect("renders");
        assert!(
            socket.contains("ListenStream=/tmp/50%%/daemon.sock"),
            "{socket}"
        );
        assert!(!socket.contains("50%/"), "{socket}");
        assert_well_formed(&socket);
    }

    #[test]
    fn passes_xml_metacharacters_through_literally() {
        // Unlike launchd's plist, systemd units are not XML — `&`, `<`, `>` are
        // ordinary path characters and must survive verbatim.
        let service = render_service(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/a&b<c>d/daemon.sock"),
        )
        .expect("renders");
        assert!(service.contains("/tmp/a&b<c>d/daemon.sock"), "{service}");
        assert!(!service.contains("&amp;"), "{service}");
        assert_well_formed(&service);
    }

    #[test]
    fn renders_a_socket_activated_unit() {
        let socket = render_socket(Path::new("/tmp/omni-dev/daemon.sock")).expect("renders");
        assert!(socket.contains("[Socket]"), "{socket}");
        assert!(
            socket.contains("ListenStream=/tmp/omni-dev/daemon.sock"),
            "{socket}"
        );
        assert!(socket.contains("SocketMode=0600"), "{socket}");
        assert!(socket.contains("WantedBy=sockets.target"), "{socket}");
        assert_well_formed(&socket);
    }

    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xFF/omni-dev"));
        assert!(render_service(bad, Path::new("/tmp/daemon.sock")).is_err());
        assert!(render_service(Path::new("/usr/bin/omni-dev"), bad).is_err());
        assert!(render_socket(bad).is_err());
    }

    #[test]
    fn rejects_newline_in_path() {
        let bad = Path::new("/tmp/a\nb/daemon.sock");
        assert!(render_service(Path::new("/usr/bin/omni-dev"), bad).is_err());
        assert!(render_socket(bad).is_err());
    }

    /// The unit-test binary is not socket-activated, so the activation lookup
    /// must report "no inherited socket" rather than error, letting the daemon
    /// fall back to self-binding. (`LISTEN_PID` is unset, so this never touches
    /// the reactor.)
    #[test]
    fn systemd_listener_is_none_when_not_activated() {
        let result = systemd_listener();
        assert!(
            matches!(result, Ok(None)),
            "expected Ok(None) outside systemd activation, got {result:?}"
        );
    }

    #[test]
    fn unit_dir_sits_under_systemd_user() {
        let dir = unit_dir().expect("config dir resolves in test");
        assert!(dir.ends_with("systemd/user"), "{}", dir.display());
    }

    #[test]
    fn systemctl_targets_the_user_manager() {
        let cmd = systemctl();
        assert_eq!(cmd.get_program().to_str(), Some("systemctl"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, ["--user"]);
    }

    #[test]
    fn flag_is_truthy_recognises_on_and_off_values() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(flag_is_truthy(on), "{on:?} should be truthy");
        }
        for off in ["0", "false", "no", "", "off", "disable"] {
            assert!(!flag_is_truthy(off), "{off:?} should be falsey");
        }
    }

    #[test]
    fn is_available_is_a_total_bool() {
        // Environment-dependent, so we only assert it never panics — exercising the
        // disabled/booted/runtime-socket probe path in whatever env the test runs.
        let _ = is_available();
    }

    /// `set_cloexec` must actually set the flag: clear it first, then confirm the
    /// call turns it back on.
    #[allow(unsafe_code)]
    #[test]
    fn set_cloexec_sets_the_flag() {
        use std::os::unix::io::AsRawFd;

        use nix::libc;

        let dir = tempfile::tempdir().unwrap();
        let listener = StdUnixListener::bind(dir.path().join("s.sock")).unwrap();
        let fd = listener.as_raw_fd();

        // Rust sets `FD_CLOEXEC` on sockets it creates, so clear it first to make
        // the assertion meaningful.
        // SAFETY: `F_SETFD`/`F_GETFD` on a live owned descriptor are simple flag
        // syscalls with no memory effects.
        let cleared = unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        assert_eq!(cleared, 0, "clearing FD_CLOEXEC failed");
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "precondition: FD_CLOEXEC should be cleared"
        );

        set_cloexec(fd).expect("set_cloexec succeeds on a valid fd");

        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0,
            "FD_CLOEXEC should be set after set_cloexec"
        );
    }
}
