//! End-to-end integration test for the `omni-dev daemon` command tree.
//!
//! Spawns the real `daemon run` process on a temp socket and drives it through
//! the library [`DaemonClient`] (ping → status → shutdown), proving the CLI
//! wiring and graceful-shutdown path without touching any per-user location.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::time::Duration;

use omni_dev::daemon::client::DaemonClient;

/// Polls `f` until it returns `true` or the deadline passes.
async fn wait_for<F>(mut f: F) -> bool
where
    F: FnMut() -> bool,
{
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn daemon_run_status_stop_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");

    // Bind the bridge's TCP planes to random free ports so the test never
    // collides with a real daemon (or another test) on the default 9998/9999.
    let mut child = Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("daemon")
        .arg("run")
        .arg("--socket")
        .arg(&socket)
        .arg("--bridge-control-port")
        .arg("0")
        .arg("--bridge-ws-port")
        .arg("0")
        // Stay headless even when the binary is built with `--features menu-bar`,
        // so this test never tries to open a macOS tray in a headless run.
        .arg("--no-menu")
        .spawn()
        .expect("failed to spawn `omni-dev daemon run`");

    let client = DaemonClient::new(&socket);

    // The control socket comes up.
    let mut ready = false;
    for _ in 0..200 {
        if client.ping().await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "daemon did not become ready");

    // Status reports the registered browser-bridge service.
    let report = client.status().await.expect("status request failed");
    assert!(
        report
            .services
            .iter()
            .any(|s| s.name == "browser-bridge" && s.healthy),
        "expected a healthy browser-bridge service, got {report:?}"
    );

    // Graceful shutdown over the socket.
    client.shutdown().await.expect("shutdown request failed");

    // The process exits on its own (clean exit 0).
    let exited = wait_for(|| matches!(child.try_wait(), Ok(Some(_)))).await;
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "daemon did not exit after shutdown");

    let status = child.wait().unwrap();
    assert!(status.success(), "daemon exited with failure: {status:?}");
}

/// SIGHUP is a graceful-shutdown signal like SIGTERM/SIGINT (#1112): the
/// daemon drains and exits 0 instead of dying to the default disposition.
#[cfg(unix)]
#[tokio::test]
async fn daemon_run_shuts_down_gracefully_on_sighup() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");

    let mut child = Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("daemon")
        .arg("run")
        .arg("--socket")
        .arg(&socket)
        .arg("--bridge-control-port")
        .arg("0")
        .arg("--bridge-ws-port")
        .arg("0")
        .arg("--no-menu")
        .spawn()
        .expect("failed to spawn `omni-dev daemon run`");

    let client = DaemonClient::new(&socket);
    let ready = {
        let client = &client;
        let mut ok = false;
        for _ in 0..200 {
            if client.ping().await.is_ok() {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        ok
    };
    if !ready {
        let _ = child.kill();
    }
    assert!(ready, "daemon did not become ready");

    kill(
        Pid::from_raw(child.id().try_into().unwrap()),
        Signal::SIGHUP,
    )
    .expect("failed to send SIGHUP");

    let exited = wait_for(|| matches!(child.try_wait(), Ok(Some(_)))).await;
    if !exited {
        let _ = child.kill();
    }
    assert!(exited, "daemon did not exit after SIGHUP");

    let status = child.wait().unwrap();
    assert!(status.success(), "SIGHUP exit was not graceful: {status:?}");
    // Graceful shutdown unlinks the self-bound control socket.
    assert!(!socket.exists(), "socket was not unlinked on shutdown");
}

/// `daemon start` off macOS must leave a daemon that has fully left the
/// launcher's session (#1112): its own session id, and stdout/stderr appended
/// to a `0600` `daemon.log` beside the socket. Linux-only because `start` on
/// macOS installs a real launchd LaunchAgent, and because the session-id probe
/// reads `/proc`.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn daemon_start_detaches_into_its_own_session() {
    use std::os::unix::fs::PermissionsExt;

    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    /// Session id from `/proc/<pid>/stat` field 6 (pid, comm, state, ppid,
    /// pgrp, session). `comm` may contain spaces, so split after the last `)`.
    fn session_id(pid: &str) -> i32 {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
        after_comm
            .split_whitespace()
            .nth(3)
            .unwrap()
            .parse()
            .unwrap()
    }

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");

    // The launcher spawns the detached daemon, waits for readiness, and exits.
    // NOTE: the spawned `daemon run` uses the default bridge TCP ports; fine in
    // CI, but on a workstation a resident daemon may make its bridge fail —
    // the control socket (what this test exercises) still comes up.
    let launcher = Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("daemon")
        .arg("start")
        .arg("--socket")
        .arg(&socket)
        .status()
        .expect("failed to run `omni-dev daemon start`");
    assert!(launcher.success(), "daemon start failed: {launcher:?}");

    let client = DaemonClient::new(&socket);
    // The launcher has already exited, yet the daemon keeps answering.
    client.ping().await.expect("detached daemon is not alive");

    // Find the daemon: the process whose argv carries our socket path.
    let socket_str = socket.to_str().unwrap();
    let daemon_pid = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.chars().all(|c| c.is_ascii_digit()).then_some(name)
        })
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|argv| {
                let argv = String::from_utf8_lossy(&argv);
                argv.contains("daemon\0run\0") && argv.contains(socket_str)
            })
        })
        .expect("could not find the detached daemon process");

    // Detached: the daemon leads its own session, distinct from this test's.
    let daemon_sid = session_id(&daemon_pid);
    assert_eq!(
        daemon_sid,
        daemon_pid.parse::<i32>().unwrap(),
        "daemon is not a session leader"
    );
    assert_ne!(
        daemon_sid,
        session_id("self"),
        "daemon shares the launcher's session"
    );

    // Its stdio landed in a 0600 daemon.log beside the socket.
    let log = dir.path().join("daemon.log");
    assert!(
        log.is_file(),
        "daemon.log was not created beside the socket"
    );
    assert_eq!(
        std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600,
        "daemon.log is not 0600"
    );

    // Graceful stop; SIGKILL as a last-resort cleanup so a failure here can't
    // leak a daemon into the CI runner.
    let stopped = client.shutdown().await;
    let pid = Pid::from_raw(daemon_pid.parse().unwrap());
    let gone = wait_for(|| kill(pid, None).is_err()).await;
    if !gone {
        let _ = kill(pid, Signal::SIGKILL);
    }
    stopped.expect("shutdown request failed");
    assert!(gone, "daemon did not exit after shutdown");
}
