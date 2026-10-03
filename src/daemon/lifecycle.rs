//! Process-lifecycle wiring: translate OS termination signals into a graceful
//! shutdown of the daemon's [`CancellationToken`], and lift the descriptor limit
//! the service manager starts it with.

use tokio_util::sync::CancellationToken;

/// The most descriptors the daemon asks for (#2111).
///
/// Far above the daemon's real need — two push streams per VS Code window plus
/// the requests in flight — yet below macOS's per-process cap (`OPEN_MAX`, 10240),
/// above which `setrlimit` fails outright. It stays modest on purpose: the limit
/// is inherited by every `gh`/`git`/`code` child the daemon spawns, and a very
/// large one makes close-all-descriptors loops and `select`-based tools slow or
/// wrong.
const NOFILE_CEILING: nix::libc::rlim_t = 4096;

/// The soft `RLIMIT_NOFILE` to raise to, or `None` when it should be left alone.
///
/// Aims for the lower of the hard limit and `ceiling`. A soft limit already at or
/// above that is never lowered — an operator who set a higher one meant it.
fn nofile_target(
    soft: nix::libc::rlim_t,
    hard: nix::libc::rlim_t,
    ceiling: nix::libc::rlim_t,
) -> Option<nix::libc::rlim_t> {
    let target = hard.min(ceiling);
    (target > soft).then_some(target)
}

/// Raises the soft `RLIMIT_NOFILE` toward the hard limit, up to [`NOFILE_CEILING`].
///
/// launchd starts the daemon with macOS's default soft limit of 256, which a busy
/// day of open windows and in-flight requests crosses — and once `accept` fails
/// with `EMFILE` the daemon cannot hear heartbeats (#2111). Best-effort: a failure
/// is logged and the daemon carries on with the limit it has.
pub fn raise_nofile_limit() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};

    let (soft, hard) = match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok(limits) => limits,
        Err(e) => {
            tracing::warn!("could not read the open-file limit: {e}");
            return;
        }
    };
    let Some(target) = nofile_target(soft, hard, NOFILE_CEILING) else {
        tracing::debug!("open-file limit {soft} (hard {hard}) needs no raising; leaving it");
        return;
    };
    match setrlimit(Resource::RLIMIT_NOFILE, target, hard) {
        Ok(()) => tracing::info!("raised the open-file limit from {soft} to {target}"),
        Err(e) => {
            tracing::warn!("could not raise the open-file limit from {soft} to {target}: {e}");
        }
    }
}

/// Spawns a task that cancels `shutdown` when the process is asked to stop.
///
/// On Unix this listens for `SIGTERM` (what `launchctl bootout` and service
/// managers send), `SIGINT` (Ctrl-C in a foreground `daemon run`), and
/// `SIGHUP` (the default disposition would hard-kill; treating it as a
/// graceful stop keeps the socket unlinked and services drained even though a
/// `daemon start`-launched daemon sits in its own session and never sees a
/// terminal hangup). Elsewhere it listens for Ctrl-C only.
pub fn install_signal_handlers(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        tokio::spawn(async move {
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to install SIGTERM handler: {e}");
                    return;
                }
            };
            let mut interrupt = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to install SIGINT handler: {e}");
                    return;
                }
            };
            let mut hangup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to install SIGHUP handler: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => tracing::info!("received SIGTERM; shutting down"),
                _ = interrupt.recv() => tracing::info!("received SIGINT; shutting down"),
                _ = hangup.recv() => tracing::info!("received SIGHUP; shutting down"),
            }
            shutdown.cancel();
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("received Ctrl-C; shutting down");
                shutdown.cancel();
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn nofile_target_raises_a_low_soft_limit_to_the_ceiling() {
        // macOS under launchd: soft 256, hard unlimited.
        assert_eq!(
            nofile_target(256, nix::libc::RLIM_INFINITY, 4096),
            Some(4096)
        );
    }

    #[test]
    fn nofile_target_never_exceeds_the_hard_limit() {
        assert_eq!(nofile_target(256, 1024, 4096), Some(1024));
    }

    #[test]
    fn nofile_target_never_lowers_or_rewrites_an_adequate_limit() {
        assert_eq!(nofile_target(4096, nix::libc::RLIM_INFINITY, 4096), None);
        assert_eq!(nofile_target(65536, nix::libc::RLIM_INFINITY, 4096), None);
        // Already at the hard limit: nothing to gain.
        assert_eq!(nofile_target(1024, 1024, 4096), None);
    }

    #[test]
    fn raise_nofile_limit_leaves_the_soft_limit_no_lower_than_it_found_it() {
        use nix::sys::resource::{getrlimit, Resource};

        let (before, _) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        raise_nofile_limit();
        let (after, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
        assert!(after >= before, "soft limit fell from {before} to {after}");
        assert!(after <= hard);
    }
}
