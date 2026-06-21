//! macOS launchd integration for `omni-dev daemon start` / `stop`.
//!
//! `start` writes a per-user LaunchAgent plist and bootstraps it; the agent
//! execs `omni-dev daemon run`. `KeepAlive` is set to restart only on
//! *abnormal* exit (`SuccessfulExit = false`), so a clean `daemon stop` (which
//! drives the daemon to a zero exit) stays down rather than being respawned.
//! `RunAtLoad` makes it start at login. See ADR-0039.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
///
/// Paths are XML-escaped before interpolation: `&`, `<`, `>` are all legal in
/// filenames (a home or `--socket` dir named `A&B`), and unescaped they would
/// produce malformed plist XML that `launchctl bootstrap` rejects with an opaque
/// parse error. A non-UTF-8 path cannot be faithfully represented in the UTF-8
/// plist, so it is rejected here rather than silently corrupted by
/// `Path::display()`. See issue #991.
fn render_plist(exe: &Path, socket: &Path) -> Result<String> {
    let exe = exe
        .to_str()
        .with_context(|| format!("executable path is not valid UTF-8: {}", exe.display()))?;
    let socket = socket
        .to_str()
        .with_context(|| format!("socket path is not valid UTF-8: {}", socket.display()))?;
    Ok(format!(
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
        // `LAUNCHD_LABEL` is a compile-time ASCII reverse-DNS constant with no
        // XML metacharacters, so it needs no escaping.
        label = LAUNCHD_LABEL,
        exe = quick_xml::escape::escape(exe),
        socket = quick_xml::escape::escape(socket),
    ))
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
    std::fs::write(&plist, render_plist(&exe, socket)?)
        .with_context(|| format!("failed to write {}", plist.display()))?;

    let domain = gui_domain();
    // Bootout any prior instance first so bootstrap does not fail as already loaded.
    bootout(&domain);
    // `launchctl bootout` is asynchronous: it returns before launchd finishes
    // tearing the job down. Bootstrapping into that window races the teardown
    // and fails with EIO ("Bootstrap failed: 5: Input/output error") — the
    // failure `daemon restart` hits, since it boots the old agent out and
    // immediately re-bootstraps. Wait for the job to actually disappear first.
    wait_until_unloaded(&domain);

    // Even once the job is gone, launchd can briefly return a transient EIO
    // while it settles, so retry a few times before giving up.
    let mut last_err = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(200));
        }
        let output = Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist)
            .output()
            .context("failed to run `launchctl bootstrap`")?;
        if output.status.success() {
            return Ok(());
        }
        last_err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    }
    bail!("launchctl bootstrap failed: {last_err}");
}

/// Polls `launchctl print <domain>/<label>` until launchd no longer knows the
/// job (a non-zero exit / "Could not find service"), or ~5s elapses.
///
/// This closes the window opened by the asynchronous `launchctl bootout`: a
/// `bootstrap` issued while the prior job is still tearing down fails with EIO.
fn wait_until_unloaded(domain: &str) {
    let target = format!("{domain}/{LAUNCHD_LABEL}");
    for _ in 0..50 {
        let still_loaded = Command::new("launchctl")
            .args(["print", &target])
            .output()
            .is_ok_and(|out| out.status.success());
        if !still_loaded {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Boots out the agent (SIGTERM-ing a running daemon) so it stops and no longer
/// auto-starts. Best-effort: a not-loaded agent is not an error.
pub fn unload() -> Result<()> {
    bootout(&gui_domain());
    Ok(())
}

/// Runs `launchctl bootout <domain>/<label>`, the teardown relied on by
/// `start`/`restart` (clearing a prior instance before bootstrap, inside
/// `install_and_load`) and `stop` (disabling auto-start, via `unload`).
///
/// A non-zero exit is ignored on purpose: "already not loaded" is the common,
/// benign case. A *spawn* failure (e.g. `launchctl` missing or the GUI domain
/// unreachable) means the teardown silently did nothing, so it is logged rather
/// than discarded — otherwise `stop`/`restart` would claim to have disabled
/// auto-start when they had not. See issue #996.
fn bootout(domain: &str) {
    let target = format!("{domain}/{LAUNCHD_LABEL}");
    if let Err(e) = Command::new("launchctl")
        .args(["bootout", &target])
        .output()
    {
        tracing::warn!("failed to run `launchctl bootout {target}`: {e}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Parses `xml` to the end, asserting it is well-formed (no quick-xml
    /// error). This is the property that actually matters: an unescaped `&` in a
    /// path makes `launchctl bootstrap` reject the plist with an opaque parse
    /// error (issue #991).
    fn assert_well_formed(xml: &str) {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let mut reader = Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("plist XML is not well-formed: {e}\n---\n{xml}"),
            }
        }
    }

    #[test]
    fn escapes_xml_metacharacters_in_paths() {
        // `&`, `<`, `>` are all legal in filenames but must be escaped in XML.
        let plist = render_plist(
            Path::new("/Users/A&B/bin/omni-dev"),
            Path::new("/tmp/<sock>/daemon.sock"),
        )
        .expect("ASCII paths render");

        // The metacharacters are escaped...
        assert!(plist.contains("/Users/A&amp;B/bin/omni-dev"), "{plist}");
        assert!(plist.contains("/tmp/&lt;sock&gt;/daemon.sock"), "{plist}");
        // ...and no raw metacharacter survives inside the path strings.
        assert!(!plist.contains("A&B"), "{plist}");
        assert!(!plist.contains("<sock>"), "{plist}");

        assert_well_formed(&plist);
    }

    #[test]
    fn renders_plain_paths_verbatim() {
        let plist = render_plist(
            Path::new("/usr/local/bin/omni-dev"),
            Path::new("/tmp/omni-dev/daemon.sock"),
        )
        .expect("ASCII paths render");

        assert!(
            plist.contains("<string>/usr/local/bin/omni-dev</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/tmp/omni-dev/daemon.sock</string>"),
            "{plist}"
        );
        assert!(
            plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")),
            "{plist}"
        );
        assert_well_formed(&plist);
    }

    #[test]
    fn rejects_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // 0xFF is not valid UTF-8; such a path cannot be placed in the UTF-8
        // plist, so it must be rejected rather than silently corrupted.
        let bad = Path::new(OsStr::from_bytes(b"/tmp/\xFF/omni-dev"));
        assert!(render_plist(bad, Path::new("/tmp/daemon.sock")).is_err());
        assert!(render_plist(Path::new("/usr/bin/omni-dev"), bad).is_err());
    }
}
