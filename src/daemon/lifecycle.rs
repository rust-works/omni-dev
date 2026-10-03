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
    raise_nofile_limit_with(read_nofile_limits, write_nofile_limits, NOFILE_CEILING);
}

/// The process's current `(soft, hard)` `RLIMIT_NOFILE`.
fn read_nofile_limits() -> nix::Result<(nix::libc::rlim_t, nix::libc::rlim_t)> {
    nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
}

/// Sets the process's `RLIMIT_NOFILE` to `(soft, hard)`.
fn write_nofile_limits(soft: nix::libc::rlim_t, hard: nix::libc::rlim_t) -> nix::Result<()> {
    nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE, soft, hard)
}

/// What [`raise_nofile_limit_with`] did.
#[derive(Debug, PartialEq, Eq)]
enum NofileOutcome {
    /// The current limits could not be read, so nothing was changed.
    Unreadable,
    /// The soft limit already met every candidate, so nothing was changed.
    Adequate,
    /// The soft limit was raised to this value.
    Raised(nix::libc::rlim_t),
    /// The host refused every candidate; the soft limit is as it was.
    Refused,
}

/// [`raise_nofile_limit`] over injected limit accessors, so the fallback ladder
/// can be driven without touching the test process's real limit.
fn raise_nofile_limit_with(
    get: impl FnOnce() -> nix::Result<(nix::libc::rlim_t, nix::libc::rlim_t)>,
    mut set: impl FnMut(nix::libc::rlim_t, nix::libc::rlim_t) -> nix::Result<()>,
    ceiling: nix::libc::rlim_t,
) -> NofileOutcome {
    let (soft, hard) = match get() {
        Ok(limits) => limits,
        Err(e) => {
            tracing::warn!("could not read the open-file limit: {e}");
            return NofileOutcome::Unreadable;
        }
    };
    let candidates = nofile_candidates(soft, hard, ceiling);
    if candidates.is_empty() {
        tracing::debug!("open-file limit {soft} (hard {hard}) needs no raising; leaving it");
        return NofileOutcome::Adequate;
    }
    for target in candidates {
        match set(target, hard) {
            Ok(()) => {
                tracing::info!("raised the open-file limit from {soft} to {target}");
                return NofileOutcome::Raised(target);
            }
            Err(e) => {
                tracing::warn!("could not raise the open-file limit from {soft} to {target}: {e}");
            }
        }
    }
    NofileOutcome::Refused
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

    /// Runs [`raise_nofile_limit_with`] over `limits`, with `refuse` deciding which
    /// targets the "host" rejects, and returns the outcome plus every target it
    /// was asked to set.
    fn raise(
        limits: nix::Result<(nix::libc::rlim_t, nix::libc::rlim_t)>,
        refuse: impl Fn(nix::libc::rlim_t) -> bool,
    ) -> (NofileOutcome, Vec<nix::libc::rlim_t>) {
        let mut attempts = Vec::new();
        let outcome = raise_nofile_limit_with(
            || limits,
            |target, _hard| {
                attempts.push(target);
                if refuse(target) {
                    Err(nix::errno::Errno::EINVAL)
                } else {
                    Ok(())
                }
            },
            4096,
        );
        (outcome, attempts)
    }

    #[test]
    fn an_unreadable_limit_is_left_alone() {
        let (outcome, attempts) = raise(Err(nix::errno::Errno::EPERM), |_| false);
        assert_eq!(outcome, NofileOutcome::Unreadable);
        assert!(attempts.is_empty());
    }

    #[test]
    fn an_adequate_limit_is_not_touched() {
        let (outcome, attempts) = raise(Ok((8192, nix::libc::RLIM_INFINITY)), |_| false);
        assert_eq!(outcome, NofileOutcome::Adequate);
        assert!(attempts.is_empty());
    }

    #[test]
    fn the_limit_is_raised_to_the_ceiling_when_the_host_allows_it() {
        let (outcome, attempts) = raise(Ok((256, nix::libc::RLIM_INFINITY)), |_| false);
        assert_eq!(outcome, NofileOutcome::Raised(4096));
        assert_eq!(attempts, vec![4096], "no further attempt after a success");
    }

    #[test]
    fn a_refused_target_falls_back_to_the_next_smaller_one() {
        // macOS refuses a soft limit above `kern.maxfilesperproc`.
        let (outcome, attempts) = raise(Ok((256, nix::libc::RLIM_INFINITY)), |t| t > 1024);
        assert_eq!(outcome, NofileOutcome::Raised(1024));
        assert_eq!(attempts, vec![4096, 2048, 1024]);
    }

    #[test]
    fn a_host_that_refuses_every_target_leaves_the_limit_as_it_was() {
        let (outcome, attempts) = raise(Ok((256, nix::libc::RLIM_INFINITY)), |_| true);
        assert_eq!(outcome, NofileOutcome::Refused);
        assert_eq!(attempts, vec![4096, 2048, 1024, 512]);
    }

    /// The real accessors round-trip. Re-applying the limits the process already
    /// has changes nothing, so this cannot starve a concurrently running test —
    /// which lowering the soft limit to exercise a real raise would.
    #[test]
    fn the_real_limit_accessors_round_trip() {
        let (soft, hard) = read_nofile_limits().unwrap();

        write_nofile_limits(soft, hard).unwrap();

        assert_eq!(read_nofile_limits().unwrap(), (soft, hard));
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
