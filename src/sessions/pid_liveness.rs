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

use chrono::{DateTime, Utc};

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

/// When `pid`'s process started, as a timestamp, or `None` when it cannot be
/// determined (no such pid, a read error, or an unsupported platform).
///
/// Unlike [`process_start_token`] this is *comparable*: replaying a hook journal
/// after a daemon restart (#2108) has no earlier token to compare against, so it
/// asks instead whether the process was already running when the journal's first
/// event from that pid was written. A recycled pid started later, and fails.
pub(crate) fn process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    #[cfg(target_os = "macos")]
    {
        parse_ps_lstart(&process_start_token(pid)?)
    }
    #[cfg(target_os = "linux")]
    {
        let ticks = proc_start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
        let boot = boot_time_secs(&std::fs::read_to_string("/proc/stat").ok()?)?;
        proc_start_time(boot, ticks)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Parses `ps -o lstart=` output (`Sat Oct  3 15:16:52 2026`), which
/// [`process_start_token`] reads under `TZ=UTC`, so it is a UTC instant.
#[cfg(any(target_os = "macos", test))]
fn parse_ps_lstart(text: &str) -> Option<DateTime<Utc>> {
    chrono::NaiveDateTime::parse_from_str(text.trim(), "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|naive| naive.and_utc())
}

/// The `starttime` field of a `/proc/<pid>/stat` line: clock ticks since boot.
#[cfg(any(target_os = "linux", test))]
fn proc_start_ticks(stat: &str) -> Option<u64> {
    // After the last `)` (the parenthesised `comm` may contain anything), state
    // is field 3 and `starttime` field 22, so index 19.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// The `btime` line of `/proc/stat`: the boot time, in seconds since the epoch.
#[cfg(any(target_os = "linux", test))]
fn boot_time_secs(proc_stat: &str) -> Option<i64> {
    proc_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// A process's start time from the boot time and its `starttime` ticks.
/// `USER_HZ`, the unit of `/proc` tick counts, is 100 on every Linux ABI.
#[cfg(any(target_os = "linux", test))]
fn proc_start_time(boot_secs: i64, ticks: u64) -> Option<DateTime<Utc>> {
    const USER_HZ: u64 = 100;
    let secs = boot_secs.checked_add(i64::try_from(ticks / USER_HZ).ok()?)?;
    DateTime::from_timestamp(secs, 0)
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

    #[test]
    fn ps_lstart_is_parsed_as_utc() {
        let parsed = parse_ps_lstart("Sat Oct  3 15:16:52 2026\n").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2026-10-03T15:16:52+00:00");
        // A two-digit day parses too, and junk does not.
        assert!(parse_ps_lstart("Mon Oct 12 01:02:03 2026").is_some());
        assert!(parse_ps_lstart("").is_none());
        assert!(parse_ps_lstart("not a date").is_none());
    }

    #[test]
    fn proc_stat_fields_are_read_after_the_last_paren_of_comm() {
        // `comm` may itself hold spaces and parens; `starttime` is field 22.
        let stat = "123 (we ird) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 99999 20";
        assert_eq!(proc_start_ticks(stat), Some(99999));
        assert_eq!(proc_start_ticks("1 (x) S 1 2"), None);
        assert_eq!(proc_start_ticks("garbage"), None);
        assert_eq!(
            boot_time_secs("cpu 1 2 3\nbtime 1790000000\nprocs 4\n"),
            Some(1_790_000_000)
        );
        assert_eq!(boot_time_secs("cpu 1 2 3\n"), None);
        assert_eq!(
            proc_start_time(1_790_000_000, 250).unwrap().timestamp(),
            1_790_000_002
        );
        assert!(proc_start_time(i64::MAX, 100).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn this_process_started_in_the_past() {
        let started = process_start_time(std::process::id()).expect("a start time");
        let now = Utc::now();
        assert!(
            started <= now + chrono::Duration::seconds(2),
            "{started} vs {now}"
        );
        assert!(started > now - chrono::Duration::days(365));
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_child_has_no_start_time() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait for `true`");
        assert_eq!(process_start_time(pid), None);
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
