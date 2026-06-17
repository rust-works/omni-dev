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
