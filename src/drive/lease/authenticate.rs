//! Device-owner authentication for `drive lease acquire`
//! ([ADR-0080](../../../docs/adrs/adr-0080.md) §7/§8).
//!
//! The platform capability is the narrow [`Authenticator`] trait, mirroring
//! `daemon::services::worktrees::geometry`'s `WindowBackend` split
//! (ADR-0058): everything above this trait — the ledger, the backup, the
//! CLI verb — is plain, platform-independent code that is unit-testable
//! with a fake, and the only real implementation is the macOS
//! LocalAuthentication FFI isolated in [`macos`] (STYLE-0013). Every other
//! target gets [`Unsupported`], which always reports no authenticator
//! available — the fail-closed default ADR-0080 §8 requires for headless
//! and off-macOS contexts, with no code path that could accidentally grant
//! a lease there.

#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// Which LocalAuthentication policy an acquisition presents (ADR-0080
/// §7/§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthPolicy {
    /// Touch ID, falling back to the account password when biometrics are
    /// unavailable, unenrolled or locked out — both collected by the
    /// system's own dialog. The default.
    DeviceOwner,
    /// Touch ID only; fails outright rather than falling back to a
    /// password. A global opt-in (§13) for operators who want no
    /// keyboard-answerable prompt at all, at the cost of requiring Touch ID
    /// hardware.
    BiometricsOnly,
}

/// The result of one [`Authenticator::authenticate`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    /// A human answered the prompt and it succeeded.
    Authorized,
    /// A human answered the prompt and it failed — declined, cancelled, or
    /// timed out waiting for a response. Carries the platform's own
    /// message.
    Denied(String),
    /// No authenticator is available in this context at all: off-macOS, or
    /// a macOS process with no attached GUI session to render a prompt in.
    /// Distinct from [`Self::Denied`] — nothing was ever presented to a
    /// human, so this is the fail-closed case ADR-0080 §8 requires, not a
    /// refusal by one.
    Unavailable(String),
}

/// Presents a device-owner authentication prompt and blocks until it
/// resolves. The only implementation with a real prompt is
/// [`macos::LocalAuthenticator`]; every other target uses [`Unsupported`].
///
/// `Send + Sync`: [`crate::drive::lease::acquire::acquire`] holds a `&dyn Authenticator` across
/// an `.await` point (the `files.get` preceding it), so the future it
/// returns is `Send` only if the trait object behind the reference is
/// `Sync` — required for the async runtime to move that future between
/// worker threads.
pub(crate) trait Authenticator: Send + Sync {
    /// `reason` is shown to the human as part of the system prompt's own
    /// sentence (`LAContext`'s `localizedReason`) — it must be non-empty,
    /// short, and describe what the write is for, not implementation
    /// detail.
    fn authenticate(&self, reason: &str, policy: AuthPolicy) -> AuthOutcome;
}

/// The [`Authenticator`] for every target without a real one: always
/// [`AuthOutcome::Unavailable`], unconditionally. Used on every non-macOS
/// target, and is the whole reason `drive lease acquire` fails closed there
/// by construction rather than by a runtime check that could be wrong. On
/// macOS itself [`platform_authenticator`] never selects it in production
/// (only [`macos::LocalAuthenticator`] does), so it is genuinely unused
/// there outside its own unit test.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) struct Unsupported;

impl Authenticator for Unsupported {
    fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
        AuthOutcome::Unavailable(
            "no device-owner authenticator is available on this platform".to_string(),
        )
    }
}

/// Returns the platform's [`Authenticator`]: [`macos::LocalAuthenticator`]
/// on macOS, [`Unsupported`] everywhere else.
pub(crate) fn platform_authenticator() -> Box<dyn Authenticator> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::LocalAuthenticator)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_always_unavailable() {
        let outcome = Unsupported.authenticate("test a write", AuthPolicy::DeviceOwner);
        assert!(matches!(outcome, AuthOutcome::Unavailable(_)));
        let outcome = Unsupported.authenticate("test a write", AuthPolicy::BiometricsOnly);
        assert!(matches!(outcome, AuthOutcome::Unavailable(_)));
    }

    #[test]
    fn platform_authenticator_resolves_without_panicking() {
        // Not asserting on the outcome kind: on macOS this is a real
        // LocalAuthenticator (its own behaviour is exercised only by the
        // manual verification ADR-0080 §7 records, never by an automated
        // test); off-macOS it is `Unsupported`. Constructing it must never
        // panic on either.
        let _ = platform_authenticator();
    }
}
