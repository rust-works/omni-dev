#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end coverage of the `drive lease acquire/restore/release` exit
//! codes (issue #1775) through the real, compiled binary.
//!
//! The classification itself — which `AcquireResult`/`RestoreResult`/
//! `ReleaseResult` variant maps to which code — is already pinned by unit
//! tests next to each enum (`src/drive/lease/{acquire,restore,release}.rs`)
//! and by `src/cli/drive/lease.rs`'s own CLI-layer tests, which call the
//! private, non-exiting `run` method precisely so they can assert on a
//! refusal's code without `std::process::exit` aborting the test binary.
//! What only a spawned subprocess can prove is that `execute()` really does
//! call `std::process::exit` on top of that — this file exists solely for
//! that wiring, following the same spawn-the-real-binary pattern
//! `tests/integration_test.rs`'s
//! `binary_lint_stdin_reads_message_and_exits_nonzero_on_error` already
//! uses for `git commit message lint`'s own non-zero exit path.
//!
//! `release` is the one `drive lease` leaf that needs no Drive credentials
//! or network call at all (it's a pure ledger mutation — see
//! `src/drive/lease/release.rs`'s module doc), so it's the only leaf
//! reachable here with nothing but an isolated `HOME`; `acquire`/`restore`
//! always resolve a Drive client first, which needs a wiremock server the
//! existing in-process unit/CLI tests already provide.

fn hermetic_home() -> tempfile::TempDir {
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    std::fs::create_dir_all(&tmp_root).unwrap();
    tempfile::tempdir_in(&tmp_root).unwrap()
}

fn lease_release_cmd(home: &std::path::Path, token: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"));
    cmd.args(["drive", "lease", "release", token])
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("DRIVE_CLIENT_ID")
        .env_remove("DRIVE_CLIENT_SECRET")
        .env_remove("DRIVE_REFRESH_TOKEN");
    cmd
}

#[test]
fn release_of_an_unknown_token_exits_non_zero() {
    let home = hermetic_home();
    let output = lease_release_cmd(home.path(), "no-such-token")
        .output()
        .expect("failed to run binary");
    assert_eq!(
        output.status.code(),
        Some(1),
        "NoSuchToken must exit 1 — stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no lease in this ledger was ever acquired with that token"),
        "{stderr}"
    );
}
