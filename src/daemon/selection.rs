//! Build-time selection of which default-registry services the daemon hosts.
//!
//! By default the daemon hosts every service (see
//! [`build_default_registry`](super::build_default_registry)). An operator can
//! narrow that to a subset — e.g. only `worktrees`, with no Snowflake auth
//! machinery and no browser-bridge TCP planes — via the `daemon run --services`
//! flag or the [`SERVICES_ENV`] environment variable.
//!
//! The selection is resolved at daemon **build** time and gates each service's
//! construction and registration; [`ServiceRegistry`](super::registry::ServiceRegistry)
//! stays append-only, so a *runtime* enable/disable toggle remains a follow-up
//! (#1318).
//!
//! Because launchd/systemd `exec` a fixed `daemon run` command with a minimal
//! environment, a shell env var never reaches a service-managed daemon; `daemon
//! start`/`restart` therefore bake the resolved selection into the generated
//! plist / unit as a `--services a,b,c` argument (see [`ServiceSelection::to_csv`]).

use clap::ValueEnum;

use super::services::{bridge, sessions, snowflake, worktrees};

/// Environment variable naming the service subset the daemon should host
/// (comma-separated canonical names). Honored by a manually-run `daemon run`;
/// overridden by an explicit `--services` flag.
pub const SERVICES_ENV: &str = "OMNI_DEV_DAEMON_SERVICES";

/// One of the daemon's default-registry services.
///
/// The `#[value(name = …)]` strings are the services' canonical
/// `SERVICE_NAME`s, so `--services`, the `status` op, and the control-socket
/// wire name all agree (guarded by a drift test below). The test-only `echo`
/// service is intentionally excluded — it is never in the default registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum DaemonServiceKind {
    /// The browser bridge (`browser-bridge`).
    #[value(name = "browser-bridge")]
    Bridge,
    /// The Snowflake query service (`snowflake`).
    #[value(name = "snowflake")]
    Snowflake,
    /// The cross-window worktrees registry (`worktrees`).
    #[value(name = "worktrees")]
    Worktrees,
    /// The Claude Code sessions tracker (`sessions`).
    #[value(name = "sessions")]
    Sessions,
}

impl DaemonServiceKind {
    /// Every kind, in canonical registration order.
    pub const ALL: [Self; 4] = [
        Self::Bridge,
        Self::Snowflake,
        Self::Worktrees,
        Self::Sessions,
    ];

    /// The service's canonical name — identical to its `SERVICE_NAME` constant
    /// and its clap value name.
    pub fn to_name(self) -> &'static str {
        match self {
            Self::Bridge => bridge::SERVICE_NAME,
            Self::Snowflake => snowflake::SERVICE_NAME,
            Self::Worktrees => worktrees::SERVICE_NAME,
            Self::Sessions => sessions::SERVICE_NAME,
        }
    }

    /// Resolves a canonical service name to its kind, or `None` for an unknown
    /// token.
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.to_name() == name)
    }
}

/// A comma-joined list of every known service name, for error/warning text.
fn known_names() -> String {
    DaemonServiceKind::ALL
        .iter()
        .map(|k| k.to_name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Which default-registry services a daemon should host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceSelection {
    /// Host every service — the default when nothing is specified.
    All,
    /// Host exactly these services (deduped, first-seen order, non-empty).
    Only(Vec<DaemonServiceKind>),
}

impl ServiceSelection {
    /// Whether `kind` is hosted under this selection.
    pub fn includes(&self, kind: DaemonServiceKind) -> bool {
        match self {
            Self::All => true,
            Self::Only(kinds) => kinds.contains(&kind),
        }
    }

    /// The selection as a `--services` CSV value: `Some("a,b")` for a subset,
    /// `None` when hosting everything (so the launcher bakes no argument).
    pub fn to_csv(&self) -> Option<String> {
        match self {
            Self::All => None,
            Self::Only(kinds) => Some(
                kinds
                    .iter()
                    .map(|k| k.to_name())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        }
    }

    /// Resolves the selection from the CLI flag values and an optional env-var
    /// value. Pure (no environment access) so it is unit-testable.
    ///
    /// Precedence: a non-empty `flag` wins outright; otherwise the `env` CSV is
    /// parsed (blank and unknown tokens are warn-and-skipped); an empty result
    /// means host everything ([`ServiceSelection::All`]).
    pub fn resolve(flag: &[DaemonServiceKind], env: Option<&str>) -> Self {
        if !flag.is_empty() {
            return Self::from_kinds(flag.iter().copied());
        }
        if let Some(raw) = env {
            let mut kinds = Vec::new();
            for token in raw.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                if let Some(kind) = DaemonServiceKind::from_name(token) {
                    kinds.push(kind);
                } else {
                    tracing::warn!(
                        "ignoring unknown service `{token}` in {SERVICES_ENV} (known: {})",
                        known_names()
                    );
                }
            }
            if !kinds.is_empty() {
                return Self::from_kinds(kinds.into_iter());
            }
        }
        Self::All
    }

    /// Reads [`SERVICES_ENV`] and delegates to [`resolve`](Self::resolve). Used by
    /// `daemon run` (build side) and `daemon start` (bake side).
    pub fn from_flag_or_env(flag: &[DaemonServiceKind]) -> Self {
        Self::resolve(flag, std::env::var(SERVICES_ENV).ok().as_deref())
    }

    /// Builds a selection from live service names (a running daemon's `status`),
    /// for `daemon restart`'s subset-preserving path. Unknown names are ignored;
    /// an empty result means host everything.
    pub fn from_service_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self::from_kinds(names.into_iter().filter_map(DaemonServiceKind::from_name))
    }

    /// Dedupes `kinds` preserving first-seen order. Two results collapse to
    /// [`All`](Self::All): an *empty* set (a selection is never silently empty — a
    /// daemon with no services would be useless), and a *full* known-set. The
    /// full-set collapse means hosting every selectable service bakes no
    /// `--services` argument, so a `restart` of a full daemon stays byte-identical
    /// to its `start` and a future selectable service still auto-enables on a plain
    /// `restart` rather than being frozen out of the baked list (#1352 review).
    fn from_kinds(kinds: impl Iterator<Item = DaemonServiceKind>) -> Self {
        let mut seen: Vec<DaemonServiceKind> = Vec::new();
        for k in kinds {
            if !seen.contains(&k) {
                seen.push(k);
            }
        }
        // `seen` is deduped and every element is a valid kind, so a length equal to
        // the full set *is* the full set.
        if seen.is_empty() || seen.len() == DaemonServiceKind::ALL.len() {
            Self::All
        } else {
            Self::Only(seen)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use DaemonServiceKind::*;

    #[test]
    fn value_names_match_the_service_constants() {
        // The clap value name, `to_name()`, and each service's `SERVICE_NAME`
        // must all agree, so `--services`, `status`, and the wire name never
        // drift apart.
        for (kind, expected) in [
            (Bridge, bridge::SERVICE_NAME),
            (Snowflake, snowflake::SERVICE_NAME),
            (Worktrees, worktrees::SERVICE_NAME),
            (Sessions, sessions::SERVICE_NAME),
        ] {
            assert_eq!(kind.to_name(), expected);
            assert_eq!(
                kind.to_possible_value().unwrap().get_name(),
                expected,
                "clap value name drifted from SERVICE_NAME for {kind:?}"
            );
        }
    }

    #[test]
    fn flag_wins_over_env() {
        assert_eq!(
            ServiceSelection::resolve(&[Worktrees], Some("sessions")),
            ServiceSelection::Only(vec![Worktrees])
        );
    }

    #[test]
    fn env_is_read_when_the_flag_is_empty() {
        assert_eq!(
            ServiceSelection::resolve(&[], Some("worktrees,sessions")),
            ServiceSelection::Only(vec![Worktrees, Sessions])
        );
    }

    #[test]
    fn unknown_and_blank_env_tokens_are_skipped() {
        assert_eq!(
            ServiceSelection::resolve(&[], Some(" worktrees , bogus ,, sessions ")),
            ServiceSelection::Only(vec![Worktrees, Sessions])
        );
    }

    #[test]
    fn an_empty_or_all_unknown_selection_is_all() {
        assert_eq!(ServiceSelection::resolve(&[], None), ServiceSelection::All);
        assert_eq!(
            ServiceSelection::resolve(&[], Some("")),
            ServiceSelection::All
        );
        assert_eq!(
            ServiceSelection::resolve(&[], Some("nope,also-nope")),
            ServiceSelection::All
        );
    }

    #[test]
    fn selections_are_deduped_in_first_seen_order() {
        assert_eq!(
            ServiceSelection::resolve(&[Sessions, Worktrees, Sessions], None),
            ServiceSelection::Only(vec![Sessions, Worktrees])
        );
        assert_eq!(
            ServiceSelection::resolve(&[], Some("worktrees,worktrees")),
            ServiceSelection::Only(vec![Worktrees])
        );
    }

    #[test]
    fn a_full_known_set_collapses_to_all() {
        // Naming every selectable service is equivalent to `All`, so it bakes no
        // `--services` argument (#1352 review) — whether it arrives via the flag,
        // the env var, or a running daemon's status names (which also carry the
        // always-on `github` service, dropped as non-selectable).
        assert_eq!(
            ServiceSelection::resolve(&[Bridge, Snowflake, Worktrees, Sessions], None),
            ServiceSelection::All
        );
        assert_eq!(
            ServiceSelection::resolve(&[], Some("browser-bridge,snowflake,worktrees,sessions")),
            ServiceSelection::All
        );
        assert_eq!(
            ServiceSelection::from_service_names([
                "browser-bridge",
                "snowflake",
                "worktrees",
                "sessions",
                "github",
            ]),
            ServiceSelection::All
        );
        // A full set collapses even when it arrives out of order or with repeats.
        assert_eq!(
            ServiceSelection::resolve(&[Sessions, Worktrees, Snowflake, Bridge, Bridge], None),
            ServiceSelection::All
        );
    }

    #[test]
    fn to_csv_round_trips_and_omits_for_all() {
        assert_eq!(
            ServiceSelection::Only(vec![Worktrees, Sessions]).to_csv(),
            Some("worktrees,sessions".to_string())
        );
        assert_eq!(ServiceSelection::All.to_csv(), None);
    }

    #[test]
    fn includes_matches_membership() {
        assert!(ServiceSelection::All.includes(Bridge));
        let only = ServiceSelection::Only(vec![Worktrees]);
        assert!(only.includes(Worktrees));
        assert!(!only.includes(Bridge));
    }

    #[test]
    fn from_service_names_maps_and_ignores_unknowns() {
        assert_eq!(
            ServiceSelection::from_service_names(["worktrees", "browser-bridge"]),
            ServiceSelection::Only(vec![Worktrees, Bridge])
        );
        // Unknown names are dropped; an all-unknown or empty list is `All`.
        assert_eq!(
            ServiceSelection::from_service_names(["mystery"]),
            ServiceSelection::All
        );
        assert_eq!(
            ServiceSelection::from_service_names(std::iter::empty::<&str>()),
            ServiceSelection::All
        );
    }
}
