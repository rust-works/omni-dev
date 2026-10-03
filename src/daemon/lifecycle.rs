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

/// Smaller soft limits to fall back to when the first choice is refused.
const NOFILE_FALLBACKS: [nix::libc::rlim_t; 3] = [2048, 1024, 512];

/// The soft `RLIMIT_NOFILE` values to try raising to, best first; empty when the
/// limit should be left alone.
///
/// Aims for the lower of the hard limit and `ceiling`, then the
/// [`NOFILE_FALLBACKS`] below it — some hosts cap the soft limit under the hard one
/// (macOS's `kern.maxfilesperproc`), and a partial raise still beats none. A soft
/// limit already at or above a candidate never gets it, so nothing is ever lowered:
/// an operator who set a higher one meant it.
fn nofile_candidates(
    soft: nix::libc::rlim_t,
    hard: nix::libc::rlim_t,
    ceiling: nix::libc::rlim_t,
) -> Vec<nix::libc::rlim_t> {
    let top = hard.min(ceiling);
    let mut candidates = vec![top];
    candidates.extend(NOFILE_FALLBACKS.iter().copied().filter(|&c| c < top));
    candidates.retain(|&c| c > soft);
    candidates
}

/// Raises the soft `RLIMIT_NOFILE` toward the hard limit, up to `NOFILE_CEILING`
/// (4096), settling for a smaller value if the host refuses that one.
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
    let candidates = nofile_candidates(soft, hard, NOFILE_CEILING);
    if candidates.is_empty() {
        tracing::debug!("open-file limit {soft} (hard {hard}) needs no raising; leaving it");
        return;
    }
    let mut last_error = None;
    for target in candidates {
        match setrlimit(Resource::RLIMIT_NOFILE, target, hard) {
            Ok(()) => {
                tracing::info!("raised the open-file limit from {soft} to {target}");
                return;
            }
            Err(e) => last_error = Some((target, e)),
        }
    }
    if let Some((target, e)) = last_error {
        tracing::warn!(
            "could not raise the open-file limit from {soft} (last tried {target}): {e}"
        );
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
    fn candidates_lead_with_the_ceiling_then_fall_back_to_smaller_values() {
        // macOS under launchd: soft 256, hard unlimited.
        assert_eq!(
            nofile_candidates(256, nix::libc::RLIM_INFINITY, 4096),
            vec![4096, 2048, 1024, 512]
        );
    }

    #[test]
    fn candidates_never_exceed_the_hard_limit() {
        assert_eq!(nofile_candidates(256, 1024, 4096), vec![1024, 512]);
        assert_eq!(nofile_candidates(256, 300, 4096), vec![300]);
    }

    #[test]
    fn candidates_never_lower_or_rewrite_an_adequate_limit() {
        assert!(nofile_candidates(4096, nix::libc::RLIM_INFINITY, 4096).is_empty());
        assert!(nofile_candidates(65536, nix::libc::RLIM_INFINITY, 4096).is_empty());
        // Already at the hard limit: nothing to gain.
        assert!(nofile_candidates(1024, 1024, 4096).is_empty());
    }

    #[test]
    fn candidates_skip_values_at_or_below_the_current_soft_limit() {
        assert_eq!(
            nofile_candidates(1500, nix::libc::RLIM_INFINITY, 4096),
            vec![4096, 2048]
        );
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
