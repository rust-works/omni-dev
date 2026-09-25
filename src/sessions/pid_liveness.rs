//! Cross-platform process-identity checks backing the pid-based liveness
//! watcher (#1916): whether a pid is still running, and an opaque per-process
//! identity token used to tell the original process from an unrelated one the
//! OS has since recycled the same pid number onto.
//!
//! Both are called **only** from [`super::pid_watcher`], never from the hook
//! sink or `claude-wrap`: those clients send nothing but a bare `pid` (#1948),
//! and the daemon — the one process that will ever need to compare a token
//! against a later reading — is also the only one that ever reads one. That
//! keeps every reading in one environment (the daemon's own, under whatever
//! service manager started it), so nothing about the *caller's* locale, `TZ`,
//! or shell ever enters into it.
//!
//! Two independent axes of platform support, each with a safe, inert fallback:
//! [`process_exists`] needs only `kill(pid, 0)`, available on any unix (`nix`'s
//! `signal` feature); [`process_start_token`] has no portable equivalent, so it
//! is implemented per `target_os` and returns `None` elsewhere. A pid the
//! watcher can only confirm as "cannot tell" (rather than "definitely gone" or
//! "definitely still this process") never causes an end or an exemption — see
//! [`super::pid_watcher`]'s `plan` for how each is used.

/// Whether a process with this pid currently exists.
///
/// `#[cfg(unix)]`: a bare `kill(pid, 0)` (no signal actually sent) — the
/// existing idiom this crate already uses for the same check in
/// `claude_cli.rs`'s process-group tests. `Err(EPERM)` still means the process
/// exists (it belongs to another user); only `ESRCH` means gone.
///
/// On a non-unix target there is no cheap equivalent, so this fails open
/// (`true`): the caller must never treat "cannot confirm" as "confirmed gone".
#[cfg(unix)]
pub(crate) fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

#[cfg(not(unix))]
pub(crate) fn process_exists(_pid: u32) -> bool {
    true
}

/// Reads an opaque identity token for `pid`'s start time, or `None` when it
/// cannot be determined (no such pid, a read error, or an unsupported
/// platform). This is **never** a parseable timestamp — only ever compared for
/// equality against a token read earlier for the same pid, always by this same
/// process — so the two platform implementations are free to use whatever
/// native representation is cheapest.
#[cfg(target_os = "linux")]
pub(crate) fn process_start_token(pid: u32) -> Option<String> {
    // `/proc/<pid>/stat` field 22 (`starttime`, in clock ticks since boot) is a
    // stable per-process identity on a running Linux system — no boot-time
    // correlation needed since we only ever compare it to another reading from
    // the same host. `comm` (field 2) is parenthesised and may itself contain
    // spaces or parens, so splitting after the *last* `)` is the standard
    // robust way to reach the fixed-format fields that follow; state (field 3)
    // is then index 0, so starttime (field 22) is index 19.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let starttime = after_comm.split_whitespace().nth(19)?;
    Some(starttime.to_string())
}

/// macOS has no `/proc`, so this shells out to `ps` instead of reaching for
/// `sysctl`/`proc_pidinfo` FFI — the same "one batched `ps` call rather than
/// more `unsafe`" precedent `geometry/ax.rs` uses for pid info (ADR-0058's
/// reasoning applies here too: a subprocess needs no ADR, new `unsafe`, or new
/// dependency). `lstart` is the full start timestamp string; kept as an opaque
/// string rather than parsed, since only equality against a later reading
/// matters. `LC_ALL=C`/`TZ=UTC` pin the format regardless of the *daemon's*
/// ambient locale/timezone — moot for two readings from the same environment,
/// but a cheap, deterministic belt-and-suspenders since a service manager's
/// environment can change across a restart. Tolerant of a dead pid producing a
/// diagnostic line instead of data (checked by emptiness, not exit status,
/// matching `app_pids_via_ps`).
#[cfg(target_os = "macos")]
pub(crate) fn process_start_token(pid: u32) -> Option<String> {
    let output = std::process::Command::new("/bin/ps")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn process_start_token(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn this_process_exists() {
        assert!(process_exists(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_child_does_not_exist() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        child.wait().expect("wait for `true`");
        assert!(!process_exists(child.id()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_has_a_stable_start_token() {
        let pid = std::process::id();
        let a = process_start_token(pid).expect("a token for this running process");
        let b = process_start_token(pid).expect("a second reading");
        assert_eq!(
            a, b,
            "the same still-running process must read the same token twice"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_child_has_no_start_token() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for `true`");
        assert_eq!(process_start_token(pid), None);
    }
}
