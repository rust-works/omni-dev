//! The real [`super::Authenticator`]: macOS `LocalAuthentication`
//! (ADR-0080 §7).
//!
//! The crate sets `unsafe_code = "deny"`; STYLE-0013 allows an exception
//! only when it is justified in an ADR ([ADR-0080](../../../../docs/adrs/adr-0080.md)),
//! isolated in a dedicated module (this one), and carries `SAFETY:`
//! comments — the same shape `daemon::services::worktrees::geometry::ax`
//! already established for the Accessibility FFI (named without a doc
//! link deliberately, per that module's own convention for cross-module
//! references to a `pub(super)` item).
//!
//! `LAContext` is an Objective-C class whose `evaluatePolicy` dispatches
//! through Objective-C message sending and replies via a block-based async
//! completion handler — a materially different shape from Accessibility's
//! plain-C surface, which is why this uses the upstream-generated
//! `objc2-local-authentication` bindings rather than hand-rolled `extern
//! "C"` declarations (ADR-0080 §7 explains the asymmetry with
//! [ADR-0058](../../../../docs/adrs/adr-0058.md) §6's choice for
//! Accessibility). `drive lease acquire` is a synchronous CLI command, so
//! [`LocalAuthenticator::authenticate`] bridges the async reply over a
//! one-shot channel and blocks the calling thread on it.
//!
//! Before any of that, [`LocalAuthenticator::authenticate`] asks
//! Security.framework's `SessionGetInfo` whether the caller's session can
//! show UI at all (issue #1686). That one plain-C function *is* the
//! Accessibility shape, so it is a hand-rolled `extern "C"` declaration
//! rather than a second generated-framework dependency.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAPolicy};

use super::{
    classify_preflight_error, classify_reply_error, classify_session, AuthOutcome, AuthPolicy,
    Authenticator, LaError,
};

/// How long [`LocalAuthenticator::authenticate`] waits for a human to
/// answer the system prompt before invalidating it and reporting denied.
/// Long enough to walk to the machine; short enough that an unattended
/// `drive lease acquire` cannot hang a script indefinitely (ADR-0080 §7).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// `callerSecuritySession` from `AuthSession.h`: "the session of the
/// calling process", as a `SecuritySessionId` argument.
const CALLER_SECURITY_SESSION: u32 = u32::MAX;

#[link(name = "Security", kind = "framework")]
extern "C" {
    /// `OSStatus SessionGetInfo(SecuritySessionId session,
    /// SecuritySessionId *sessionId, SessionAttributeBits *attributes)`.
    fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
}

/// The calling process's `SessionAttributeBits`, or the failing `OSStatus`.
fn caller_session_attributes() -> Result<u32, i32> {
    let mut session_id = 0u32;
    let mut attributes = 0u32;
    #[allow(unsafe_code)]
    // SAFETY: `SessionGetInfo` is a synchronous C function; both out-params
    // point at live, writable, correctly-sized (`u32` = `SecuritySessionId`
    // / `SessionAttributeBits`) locals that outlive the call, and it retains
    // neither pointer.
    let status = unsafe {
        SessionGetInfo(
            CALLER_SECURITY_SESSION,
            &raw mut session_id,
            &raw mut attributes,
        )
    };
    if status == 0 {
        Ok(attributes)
    } else {
        Err(status)
    }
}

/// Flattens an `NSError` for the platform-independent classifiers.
fn la_error(err: &NSError) -> LaError {
    LaError {
        domain: err.domain().to_string(),
        code: err.code(),
        description: err.localizedDescription().to_string(),
    }
}

/// The real, human-present authenticator: `LAContext.evaluatePolicy`.
pub(crate) struct LocalAuthenticator;

impl Authenticator for LocalAuthenticator {
    fn authenticate(&self, reason: &str, policy: AuthPolicy) -> AuthOutcome {
        // Gate first: from a session without graphical access macOS still
        // renders the dialog — on the console — and never replies, so the
        // only safe answer is to not create a prompt at all.
        if let Some(outcome) = classify_session(caller_session_attributes()) {
            return outcome;
        }

        let la_policy = match policy {
            AuthPolicy::DeviceOwner => LAPolicy::DeviceOwnerAuthentication,
            AuthPolicy::BiometricsOnly => LAPolicy::DeviceOwnerAuthenticationWithBiometrics,
        };

        #[allow(unsafe_code)]
        // SAFETY: `LAContext::new` is a plain Objective-C `+new` allocation
        // with no preconditions; the returned `Retained<LAContext>` owns the
        // object and releases it on drop.
        let ctx = unsafe { LAContext::new() };

        #[allow(unsafe_code)]
        // SAFETY: `canEvaluatePolicy:error:` is a synchronous, side-effect-free
        // preflight check per Apple's documentation (explicitly safe to call
        // outside a reply block); `ctx` is a live, owned context.
        let preflight = unsafe { ctx.canEvaluatePolicy_error(la_policy) };
        if let Err(err) = preflight {
            return classify_preflight_error(la_error(&err));
        }

        let (tx, rx) = mpsc::channel::<(bool, Option<LaError>)>();
        let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
            let success = success.as_bool();
            let error_desc = if error.is_null() {
                None
            } else {
                #[allow(unsafe_code)]
                // SAFETY: `evaluatePolicy:localizedReason:reply:` hands the reply
                // block a valid, autoreleased `NSError*` (or null) for the
                // duration of this call, per Apple's documented contract for the
                // `reply` parameter.
                let error = unsafe { &*error };
                Some(la_error(error))
            };
            // The receiver may already be gone if `authenticate` timed out and
            // returned; a send into a dropped channel is a no-op error we
            // deliberately ignore.
            let _ = tx.send((success, error_desc));
        });

        let reason = NSString::from_str(reason);

        #[allow(unsafe_code)]
        // SAFETY: `ctx` is a live, owned context; `reason` is a valid, non-empty
        // `NSString` outliving the call; `reply` is a valid block whose captured
        // `Sender` is `Send` and dropped only after the block itself is (a
        // `RcBlock` reference-counts the captured state), satisfying
        // `evaluatePolicy_localizedReason_reply`'s own `# Safety` requirement
        // that the reply block be sendable.
        unsafe {
            ctx.evaluatePolicy_localizedReason_reply(la_policy, &reason, &reply);
        }

        match rx.recv_timeout(PROMPT_TIMEOUT) {
            Ok((true, _)) => AuthOutcome::Authorized,
            Ok((false, error)) => classify_reply_error(error),
            Err(_) => {
                #[allow(unsafe_code)]
                // SAFETY: `ctx` is still live (owned by this stack frame); `invalidate`
                // has no preconditions and is safe to call at any point in a
                // context's lifetime, including concurrently with an in-flight
                // evaluation, per Apple's documentation.
                unsafe {
                    ctx.invalidate();
                }
                AuthOutcome::Denied(format!(
                    "timed out after {}s waiting for a response to the authentication prompt",
                    PROMPT_TIMEOUT.as_secs()
                ))
            }
        }
    }
}
