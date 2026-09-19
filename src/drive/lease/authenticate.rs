//! Device-owner authentication for `drive lease acquire`
//! ([ADR-0080](../../../docs/adrs/adr-0080.md) §7/§8).
//!
//! The platform capability is the narrow [`Authenticator`] trait, mirroring
//! `daemon::services::worktrees::geometry`'s `WindowBackend` split
//! (ADR-0058): everything above this trait — the ledger, the backup, the
//! CLI verb — is plain, platform-independent code that is unit-testable
//! with a fake, and the only real implementation is the macOS
//! LocalAuthentication FFI isolated in `macos` (STYLE-0013; named without a
//! doc link deliberately — it is a macOS-only module, so linking it would
//! break doc builds on other platforms, the same convention
//! `daemon::services::worktrees::geometry::ax` and `launchd_listener` use).
//! Every other target gets [`Unsupported`], which always reports no authenticator
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
///
/// `Authorized`/`Denied`/`PolicyUnsatisfiable` are constructed only by
/// `macos::LocalAuthenticator` (a real prompt can succeed or be refused, and
/// a real policy can be unsatisfiable); on every other target only
/// [`Unsupported`] implements [`Authenticator`], and it only ever returns
/// `NoAuthenticator` — so, like [`Unsupported`] itself, those three variants
/// are genuinely unconstructed in a non-macOS production build.
///
/// The split between `NoAuthenticator` and `PolicyUnsatisfiable` is what
/// scopes ADR-0080 §8's `allow_headless` waiver (issue #1686): only the
/// former is waivable. Keying the waiver on *where* a failure surfaced
/// (preflight vs. reply) instead of *what* it means once waived an attended
/// Mac whose Touch ID was merely locked out or behind a closed lid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthOutcome {
    /// A human answered the prompt and it succeeded.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Authorized,
    /// A human answered the prompt and it failed — declined, cancelled, or
    /// timed out waiting for a response. Carries the platform's own
    /// message.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Denied(String),
    /// No prompt can reach a human in this context at all: off-macOS, or a
    /// macOS process whose security session has no graphical access (SSH, a
    /// background launchd job, CI). Nothing was ever presented, so this is
    /// the fail-closed case ADR-0080 §8 requires — and the **only** outcome
    /// its `allow_headless` opt-out may waive.
    NoAuthenticator(String),
    /// A human may well be present, but the requested policy cannot be
    /// evaluated right now: Touch ID locked out, not enrolled, absent, or
    /// suspended by a closed lid, or no passcode set. Refused like
    /// `NoAuthenticator`, but **never** waived — an attended Mac is not
    /// headless (ADR-0080 §8), and waiving here would make the stricter
    /// `biometrics_only` policy strictly weaker than the default.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    PolicyUnsatisfiable(String),
}

/// `sessionHasGraphicAccess` from Security.framework's
/// `SessionAttributeBits` (`AuthSession.h`): the caller's security session
/// can put UI on a display. Clear for an SSH login and a background launchd
/// job; set for the console session.
pub(crate) const SESSION_HAS_GRAPHIC_ACCESS: u32 = 0x0010;

/// `sessionIsRemote` from `SessionAttributeBits`: the session was
/// established over the network. Informational only — it names the context
/// in the refusal message, but the decision keys on
/// [`SESSION_HAS_GRAPHIC_ACCESS`] alone.
pub(crate) const SESSION_IS_REMOTE: u32 = 0x1000;

/// `LAErrorDomain`, the `NSError` domain every LocalAuthentication error
/// carries (`kLAErrorDomain` in `LAPublicDefines.h`).
pub(crate) const LA_ERROR_DOMAIN: &str = "com.apple.LocalAuthentication";

/// `LAErrorNotInteractive`: the policy needs UI the process may not show.
const LA_ERROR_NOT_INTERACTIVE: isize = -1004;

/// A LocalAuthentication `NSError`, flattened to plain data so the
/// classification below is platform-independent and unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaError {
    /// The error's `domain`.
    pub domain: String,
    /// The error's `code` (an `LAError` value when `domain` is
    /// [`LA_ERROR_DOMAIN`]).
    pub code: isize,
    /// The error's `localizedDescription`.
    pub description: String,
}

impl LaError {
    fn is_not_interactive(&self) -> bool {
        self.domain == LA_ERROR_DOMAIN && self.code == LA_ERROR_NOT_INTERACTIVE
    }
}

/// Decides, from the caller's security session, whether a prompt could
/// reach a human at all — before one is created. `Ok(attrs)` is
/// `SessionGetInfo`'s attribute bits; `Err(status)` its failing `OSStatus`.
/// `None` means "go on and prompt".
///
/// This gate exists because macOS does **not** refuse a prompt from a
/// session without graphical access: measured over `ssh localhost`
/// (issue #1686), `canEvaluatePolicy` succeeds, `evaluatePolicy` never
/// replies (no `LAErrorNotInteractive`), and the dialog is rendered on the
/// **console** — where a person could approve a request they did not make.
/// A session-info failure fails closed as unsatisfiable, never as
/// waivable.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn classify_session(attrs: Result<u32, i32>) -> Option<AuthOutcome> {
    match attrs {
        Ok(bits) if bits & SESSION_HAS_GRAPHIC_ACCESS != 0 => None,
        Ok(bits) => Some(AuthOutcome::NoAuthenticator(format!(
            "this {} has no graphical access, so no authentication prompt can be shown",
            if bits & SESSION_IS_REMOTE != 0 {
                "remote session (e.g. SSH)"
            } else {
                "session"
            }
        ))),
        Err(status) => Some(AuthOutcome::PolicyUnsatisfiable(format!(
            "could not determine the security session's attributes (SessionGetInfo \
             returned OSStatus {status})"
        ))),
    }
}

/// Classifies a failed `canEvaluatePolicy` preflight. Only
/// `LAErrorNotInteractive` means no prompt can be shown; every other
/// failure (lockout, not enrolled, no hardware, lid closed, no passcode)
/// is a policy an attended machine cannot satisfy right now.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn classify_preflight_error(err: LaError) -> AuthOutcome {
    if err.is_not_interactive() {
        AuthOutcome::NoAuthenticator(err.description)
    } else {
        AuthOutcome::PolicyUnsatisfiable(err.description)
    }
}

/// Classifies a failed `evaluatePolicy` reply. `LAErrorNotInteractive` is
/// kept as a safety net (it was not observed over SSH — see
/// [`classify_session`]); anything else is a prompt that was shown and
/// not satisfied.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn classify_reply_error(err: Option<LaError>) -> AuthOutcome {
    match err {
        Some(err) if err.is_not_interactive() => AuthOutcome::NoAuthenticator(err.description),
        Some(err) => AuthOutcome::Denied(err.description),
        None => AuthOutcome::Denied("authentication failed".to_string()),
    }
}

/// Presents a device-owner authentication prompt and blocks until it
/// resolves. The only implementation with a real prompt is
/// `macos::LocalAuthenticator`; every other target uses [`Unsupported`].
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
/// [`AuthOutcome::NoAuthenticator`], unconditionally. Used on every non-macOS
/// target, and is the whole reason `drive lease acquire` fails closed there
/// by construction rather than by a runtime check that could be wrong. On
/// macOS itself [`platform_authenticator`] never selects it in production
/// (only `macos::LocalAuthenticator` does), so it is genuinely unused
/// there outside its own unit test.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) struct Unsupported;

impl Authenticator for Unsupported {
    fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
        AuthOutcome::NoAuthenticator(
            "no device-owner authenticator is available on this platform".to_string(),
        )
    }
}

/// Returns the platform's [`Authenticator`]: `macos::LocalAuthenticator`
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
    fn unsupported_is_always_no_authenticator() {
        let outcome = Unsupported.authenticate("test a write", AuthPolicy::DeviceOwner);
        assert!(matches!(outcome, AuthOutcome::NoAuthenticator(_)));
        let outcome = Unsupported.authenticate("test a write", AuthPolicy::BiometricsOnly);
        assert!(matches!(outcome, AuthOutcome::NoAuthenticator(_)));
    }

    fn la(code: isize) -> LaError {
        LaError {
            domain: LA_ERROR_DOMAIN.to_string(),
            code,
            description: format!("code {code}"),
        }
    }

    /// The attribute words measured on macOS 26 for issue #1686.
    const CONSOLE_ATTRS: u32 = 0x6030;
    const SSH_ATTRS: u32 = 0x5020;

    #[test]
    fn console_session_goes_on_to_prompt() {
        assert_eq!(classify_session(Ok(CONSOLE_ATTRS)), None);
    }

    #[test]
    fn ssh_session_has_no_authenticator_and_says_it_is_remote() {
        let Some(AuthOutcome::NoAuthenticator(detail)) = classify_session(Ok(SSH_ATTRS)) else {
            panic!("an SSH session must be waivable, never prompted");
        };
        assert!(detail.contains("remote session"), "{detail}");
    }

    #[test]
    fn local_session_without_graphic_access_has_no_authenticator() {
        // A background launchd job: not remote, but no display either.
        let outcome = classify_session(Ok(SSH_ATTRS & !SESSION_IS_REMOTE));
        assert!(matches!(outcome, Some(AuthOutcome::NoAuthenticator(_))));
    }

    #[test]
    fn session_info_failure_fails_closed_unwaivably() {
        assert!(matches!(
            classify_session(Err(-60008)),
            Some(AuthOutcome::PolicyUnsatisfiable(_))
        ));
    }

    #[test]
    fn preflight_failures_in_an_attended_session_are_never_waivable() {
        // passcodeNotSet, systemCancel (lid closed), biometryNotAvailable,
        // biometryNotEnrolled, biometryLockout.
        for code in [-5, -4, -6, -7, -8] {
            assert!(
                matches!(
                    classify_preflight_error(la(code)),
                    AuthOutcome::PolicyUnsatisfiable(_)
                ),
                "LAError {code} must not be waivable"
            );
        }
    }

    #[test]
    fn not_interactive_is_no_authenticator_wherever_it_surfaces() {
        assert!(matches!(
            classify_preflight_error(la(LA_ERROR_NOT_INTERACTIVE)),
            AuthOutcome::NoAuthenticator(_)
        ));
        assert!(matches!(
            classify_reply_error(Some(la(LA_ERROR_NOT_INTERACTIVE))),
            AuthOutcome::NoAuthenticator(_)
        ));
    }

    #[test]
    fn not_interactive_code_from_another_domain_is_not_trusted() {
        let foreign = LaError {
            domain: "NSOSStatusErrorDomain".to_string(),
            ..la(LA_ERROR_NOT_INTERACTIVE)
        };
        assert!(matches!(
            classify_preflight_error(foreign.clone()),
            AuthOutcome::PolicyUnsatisfiable(_)
        ));
        assert!(matches!(
            classify_reply_error(Some(foreign)),
            AuthOutcome::Denied(_)
        ));
    }

    #[test]
    fn reply_failures_are_denials() {
        // userCancel, authenticationFailed, appCancel, invalidContext (-9,
        // what `invalidate()` after the timeout yields).
        for code in [-2, -1, -9] {
            assert!(matches!(
                classify_reply_error(Some(la(code))),
                AuthOutcome::Denied(_)
            ));
        }
        assert_eq!(
            classify_reply_error(None),
            AuthOutcome::Denied("authentication failed".to_string())
        );
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
