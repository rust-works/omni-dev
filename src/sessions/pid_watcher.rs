//! The pid-based liveness watcher (#1916), supplementing every hook-fed feed.
//!
//! A hook-fed Claude Code session (a terminal, or VS Code without the stream
//! wrapper) has no liveness signal beyond activity, so [`SessionsRegistry`]
//! falls back on a flat idle TTL ([`DEFAULT_SESSION_TTL`](super::DEFAULT_SESSION_TTL))
//! in both directions: a live session idle at the prompt longer than the TTL
//! disappears, and a `claude` that exits without firing `SessionEnd` lingers in
//! its last state for up to the TTL.
//!
//! Every hook and `claude-wrap` already report the owning agent process's pid
//! (#1948, for a different reason — telling a resumed session's new process
//! from the old one it replaced). This watcher is the *only* consumer of that
//! pid for liveness purposes, and it is engine-owned background polling, the
//! [`codex_watcher`](super::codex_watcher) precedent one agent over: on its own
//! [`WATCH_INTERVAL`] tick, it snapshots every pid-bearing session cheaply
//! (no I/O, briefly under the registry lock), then does every process check —
//! a bare `kill(pid, 0)` and, on macOS, a `ps` shell-out for the identity token
//! — **off** that lock, on a blocking thread, never on a hot path a hook or the
//! tray depends on.
//!
//! Two decisions come out of a tick, applied through the registry's ordinary
//! [`end`](SessionsRegistry::end) and a new
//! [`confirm_pid_liveness`](SessionsRegistry::confirm_pid_liveness):
//!
//! - **A pid this watcher once confirmed alive is now gone:** end the session
//!   immediately, through the same short `ended_ttl` linger a clean
//!   `SessionEnd` uses, instead of waiting out the TTL.
//! - **A pid is alive, its identity token still matches (or is being captured
//!   for the first time), the session has had at least one `UserPromptSubmit`,
//!   and it is the most recently started `session_id` among every candidate
//!   sharing that pid:** refresh `last_seen`, keeping the ordinary
//!   [`reap_sessions`](super::reap_sessions) TTL from ever seeing it go stale.
//!
//! **Why a pid must be independently confirmed alive before its death is ever
//! trusted.** #1948's docs allow for a hook command wrapped in a shell, whose
//! parent — the pid a hook reports — is a fresh shell that exits within
//! milliseconds of the hook finishing. Trusting a bare "is `pid` gone right
//! now" reading would end such a session after every single hook. Instead,
//! `plan` remembers, across ticks, which pids it has *itself* seen alive; a pid
//! it never catches alive (astronomically likely for a shell that lives only
//! milliseconds, against a 10-second poll) never triggers an end — it simply
//! never enters into pid-based liveness at all, and the session ages out on
//! the ordinary TTL exactly as it did before this watcher existed. A real
//! `claude`/`codex` process, alive for the session's whole lifetime, is caught
//! alive on its very first tick.
//!
//! **Why the identity token is never sent by a client.** An earlier version of
//! this feature had the hook sink and `claude-wrap` read and send the token
//! themselves. Two problems with that, found in review: reading it costs a
//! `ps` fork on macOS, adding latency to every hook — against the sink's
//! "never blocks a turn" contract; and `ps -o lstart=` is locale/timezone
//! dependent, so a token read in the *client's* environment (a user's shell,
//! VS Code's integrated terminal) would almost never equal one read later in
//! the *daemon's* (a service manager's minimal environment), silently
//! defeating the whole feature. Having the daemon read a pid's token itself —
//! always in its own environment, both when it first captures one and every
//! time it later compares — sidesteps both: the client sends nothing but the
//! bare `pid` it already sent for #1948, and every token comparison this
//! module makes is apples to apples.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{pid_liveness, PidCandidate, SessionsRegistry};

/// How often the watcher polls. Matches the Claude transcript and Codex
/// rollout watchers.
const WATCH_INTERVAL: Duration = Duration::from_secs(10);

/// What a single pid's probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PidStatus {
    /// `kill(pid, 0)` says nothing is running under this pid any more.
    Gone,
    /// The pid exists; its current identity token, when it could be read.
    Alive { start_token: Option<String> },
}

/// Probes one pid for [`PidStatus`] — the seam `plan`'s tests fake.
fn probe_pid(pid: u32) -> PidStatus {
    if pid_liveness::process_exists(pid) {
        PidStatus::Alive {
            start_token: pid_liveness::process_start_token(pid),
        }
    } else {
        PidStatus::Gone
    }
}

/// One thing a tick asks the registry to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// This pid is gone, and this watcher had previously confirmed it alive.
    End { session_id: String },
    /// This pid is alive, its identity confirmed, and this candidate is the
    /// one eligible for the TTL exemption.
    Confirm {
        session_id: String,
        pid_start: String,
    },
}

impl Action {
    fn apply(self, registry: &SessionsRegistry, now: DateTime<Utc>) {
        match self {
            Self::End { session_id } => {
                registry.end(&session_id, Some("pid liveness watcher"), None);
            }
            Self::Confirm {
                session_id,
                pid_start,
            } => {
                registry.confirm_pid_liveness(&session_id, now, &pid_start);
            }
        }
    }
}

/// Plans one tick's actions from a registry snapshot, given which pids this
/// watcher has independently confirmed alive on an earlier tick (`confirmed`,
/// updated in place). `probe` is injected so tests can script pid states
/// without touching real processes. Pure apart from `confirmed`.
///
/// One [`probe`] call per **distinct** pid among `candidates`: several
/// `session_id`s sharing a pid (a resumed process, several Codex threads under
/// one app-server) are checked once, not once each.
fn plan(
    candidates: &[PidCandidate],
    confirmed: &mut HashSet<u32>,
    probe: impl Fn(u32) -> PidStatus,
) -> Vec<Action> {
    let candidate_pids: HashSet<u32> = candidates.iter().map(|c| c.pid).collect();
    // Forget any pid no longer among live candidates (its session ended some
    // other way), so this does not grow across the daemon's whole lifetime.
    confirmed.retain(|pid| candidate_pids.contains(pid));

    let mut status: HashMap<u32, PidStatus> = HashMap::new();
    for c in candidates {
        status.entry(c.pid).or_insert_with(|| probe(c.pid));
    }

    // The most recently started session_id among the candidates sharing each
    // pid: `/clear` (and possibly `/resume`) can start a new session_id in the
    // same process without necessarily firing `SessionEnd` for the old one, so
    // only the newest may be exempted — an older one falls back to the TTL.
    let mut newest_started_at: HashMap<u32, DateTime<Utc>> = HashMap::new();
    for c in candidates {
        newest_started_at
            .entry(c.pid)
            .and_modify(|t| *t = (*t).max(c.started_at))
            .or_insert(c.started_at);
    }

    let mut actions = Vec::new();
    for c in candidates {
        match &status[&c.pid] {
            PidStatus::Gone => {
                if confirmed.remove(&c.pid) {
                    actions.push(Action::End {
                        session_id: c.session_id.clone(),
                    });
                }
                // Else: never independently confirmed alive — most likely a
                // per-hook shell that lived only milliseconds. Left alone, it
                // ages out on the ordinary TTL exactly as it always did.
            }
            PidStatus::Alive { start_token } => {
                match (c.pid_start.as_deref(), start_token.as_deref()) {
                    // A previously captured token no longer matches this pid's
                    // current one: the OS has recycled the pid number onto an
                    // unrelated process since this was last confirmed, so the
                    // process this entry identifies is gone.
                    (Some(stored), Some(current)) if stored != current => {
                        confirmed.remove(&c.pid);
                        actions.push(Action::End {
                            session_id: c.session_id.clone(),
                        });
                    }
                    _ => {
                        confirmed.insert(c.pid);
                        let is_newest = newest_started_at.get(&c.pid) == Some(&c.started_at);
                        if c.prompted && is_newest {
                            if let Some(token) = start_token {
                                actions.push(Action::Confirm {
                                    session_id: c.session_id.clone(),
                                    pid_start: token.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    actions
}

/// Spawns the watcher loop, returning its [`JoinHandle`].
///
/// Polls every [`WATCH_INTERVAL`] and applies each action to `registry`, until
/// `token` is cancelled. The registry snapshot is cheap and lock-scoped; the
/// probing and planning run on a blocking thread, since a macOS `ps` shell-out
/// is blocking I/O. Must be called from within a tokio runtime.
pub(crate) fn spawn(registry: Arc<SessionsRegistry>, token: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut confirmed: HashSet<u32> = HashSet::new();
        loop {
            let candidates = registry.pid_liveness_candidates();
            let mut owned = std::mem::take(&mut confirmed);
            let (returned, actions) = tokio::task::spawn_blocking(move || {
                let actions = plan(&candidates, &mut owned, probe_pid);
                (owned, actions)
            })
            .await
            .unwrap_or_else(|_| (HashSet::new(), Vec::new()));
            confirmed = returned;
            let now = Utc::now();
            for action in actions {
                action.apply(&registry, now);
            }
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(WATCH_INTERVAL) => {}
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn candidate(
        session_id: &str,
        pid: u32,
        pid_start: Option<&str>,
        prompted: bool,
    ) -> PidCandidate {
        PidCandidate {
            session_id: session_id.to_string(),
            pid,
            pid_start: pid_start.map(str::to_string),
            prompted,
            started_at: Utc::now(),
        }
    }

    fn probe_of(statuses: &HashMap<u32, PidStatus>) -> impl Fn(u32) -> PidStatus {
        let statuses = statuses.clone();
        move |pid| statuses.get(&pid).cloned().unwrap_or(PidStatus::Gone)
    }

    #[test]
    fn a_pid_never_seen_alive_is_never_ended_when_it_is_gone() {
        // The per-hook-shell case: the very first sighting of this pid is
        // already dead, so nothing has ever confirmed it alive.
        let candidates = vec![candidate("s1", 100, None, true)];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(100, PidStatus::Gone)]));
        assert!(plan(&candidates, &mut confirmed, probe).is_empty());
    }

    #[test]
    fn a_confirmed_pid_going_gone_ends_its_session() {
        let candidates = vec![candidate("s1", 100, Some("tok"), true)];
        let mut confirmed = HashSet::from([100]);
        let probe = probe_of(&HashMap::from([(100, PidStatus::Gone)]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert_eq!(
            actions,
            vec![Action::End {
                session_id: "s1".to_string()
            }]
        );
        assert!(!confirmed.contains(&100));
    }

    #[test]
    fn an_alive_prompted_newest_pid_is_confirmed() {
        let candidates = vec![candidate("s1", 100, Some("tok"), true)];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive {
                start_token: Some("tok".to_string()),
            },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert_eq!(
            actions,
            vec![Action::Confirm {
                session_id: "s1".to_string(),
                pid_start: "tok".to_string()
            }]
        );
        assert!(confirmed.contains(&100));
    }

    #[test]
    fn an_unprompted_alive_pid_is_tracked_but_not_confirmed() {
        // The #1454-style pinning guard: a spare, never-prompted process must
        // not be exempted just because it is alive.
        let candidates = vec![candidate("s1", 100, Some("tok"), false)];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive {
                start_token: Some("tok".to_string()),
            },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert!(actions.is_empty());
        // Still tracked as independently confirmed, so a later death is caught.
        assert!(confirmed.contains(&100));
    }

    #[test]
    fn only_the_newest_session_under_a_shared_pid_is_confirmed() {
        // The `/clear`-style guard: an older session_id under a pid a newer
        // one has since taken over falls back to the ordinary TTL.
        let mut old = candidate("old", 100, Some("tok"), true);
        old.started_at = Utc::now() - chrono::Duration::seconds(100);
        let new = candidate("new", 100, Some("tok"), true);
        let candidates = vec![old, new];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive {
                start_token: Some("tok".to_string()),
            },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert_eq!(
            actions,
            vec![Action::Confirm {
                session_id: "new".to_string(),
                pid_start: "tok".to_string()
            }]
        );
    }

    #[test]
    fn a_mismatched_token_ends_the_session_even_though_the_pid_is_alive() {
        // The OS has recycled this pid onto an unrelated process since it was
        // last confirmed.
        let candidates = vec![candidate("s1", 100, Some("old-tok"), true)];
        let mut confirmed = HashSet::from([100]);
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive {
                start_token: Some("new-tok".to_string()),
            },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert_eq!(
            actions,
            vec![Action::End {
                session_id: "s1".to_string()
            }]
        );
        assert!(!confirmed.contains(&100));
    }

    #[test]
    fn a_pid_with_no_stored_token_captures_the_freshly_probed_one() {
        let candidates = vec![candidate("s1", 100, None, true)];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive {
                start_token: Some("first-tok".to_string()),
            },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert_eq!(
            actions,
            vec![Action::Confirm {
                session_id: "s1".to_string(),
                pid_start: "first-tok".to_string()
            }]
        );
    }

    #[test]
    fn an_unreadable_token_on_an_alive_pid_tracks_but_does_not_confirm() {
        let candidates = vec![candidate("s1", 100, None, true)];
        let mut confirmed = HashSet::new();
        let probe = probe_of(&HashMap::from([(
            100,
            PidStatus::Alive { start_token: None },
        )]));
        let actions = plan(&candidates, &mut confirmed, probe);
        assert!(actions.is_empty());
        assert!(confirmed.contains(&100));
    }

    #[test]
    fn a_distinct_pid_is_probed_only_once_for_several_candidates() {
        let candidates = vec![
            candidate("s1", 100, None, true),
            candidate("s2", 100, None, false),
        ];
        let mut confirmed = HashSet::new();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let calls_clone = calls.clone();
        let probe = move |pid| {
            *calls_clone.lock().unwrap() += 1;
            assert_eq!(pid, 100);
            PidStatus::Alive {
                start_token: Some("tok".to_string()),
            }
        };
        plan(&candidates, &mut confirmed, probe);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn a_pid_no_longer_among_candidates_is_forgotten() {
        let mut confirmed = HashSet::from([100, 200]);
        let probe = probe_of(&HashMap::new());
        // Only pid 200 is still a candidate this tick.
        let candidates = vec![candidate("s2", 200, Some("tok"), false)];
        plan(&candidates, &mut confirmed, probe);
        assert!(!confirmed.contains(&100));
    }
}
