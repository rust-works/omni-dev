//! macOS launchd integration for `omni-dev daemon start` / `stop`.
//!
//! `start` writes a per-user LaunchAgent plist and bootstraps it; the agent
//! execs `omni-dev daemon run`. `KeepAlive` is set to restart only on
//! *abnormal* exit (`SuccessfulExit = false`), so a clean `daemon stop` (which
//! drives the daemon to a zero exit) stays down rather than being respawned.
//! `RunAtLoad` makes it start at login. See ADR-0039.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Reverse-DNS LaunchAgent label, derived from the project repository
/// (`github.com/rust-works/omni-dev`).
pub const LAUNCHD_LABEL: &str = "com.github.rust-works.omni-dev.daemon";

/// Path to the per-user LaunchAgent plist
/// (`~/Library/LaunchAgents/<label>.plist`).
pub fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

/// Renders a LaunchAgent plist that execs `omni-dev daemon run --socket <socket>`.
fn render_plist(exe: &Path, socket: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>run</string>
        <string>--socket</string>
        <string>{socket}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = exe.display(),
        socket = socket.display(),
    )
}

/// The current user's launchd GUI domain target (`gui/<uid>`).
fn gui_domain() -> String {
    format!("gui/{}", nix::unistd::getuid())
}

/// Writes the plist and bootstraps the agent so the daemon runs now and at
/// login.
pub fn install_and_load(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("could not resolve the current executable")?;
    let plist = plist_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&plist, render_plist(&exe, socket))
        .with_context(|| format!("failed to write {}", plist.display()))?;

    let domain = gui_domain();
    // Bootout any prior instance first so bootstrap does not fail as already loaded.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .output();
    let output = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist)
        .output()
        .context("failed to run `launchctl bootstrap`")?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Boots out the agent (SIGTERM-ing a running daemon) so it stops and no longer
/// auto-starts. Best-effort: a not-loaded agent is not an error.
pub fn unload() -> Result<()> {
    let domain = gui_domain();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .output();
    Ok(())
}
