//! The real [`super::Authenticator`]: macOS `LocalAuthentication`
//! (ADR-0080 §7).
//!
//! The crate sets `unsafe_code = "deny"`; STYLE-0013 allows an exception
//! only when it is justified in an ADR ([ADR-0080](../../../../docs/adrs/adr-0080.md)),
//! isolated in a dedicated module (this one), and carries `SAFETY:`
//! comments — the same shape [`daemon::services::worktrees::geometry::ax`]
//! already established for the Accessibility FFI.
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

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAPolicy};

use super::{AuthOutcome, AuthPolicy, Authenticator};

/// How long [`LocalAuthenticator::authenticate`] waits for a human to
/// answer the system prompt before invalidating it and reporting denied.
/// Long enough to walk to the machine; short enough that an unattended
/// `drive lease acquire` cannot hang a script indefinitely (ADR-0080 §7).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// The real, human-present authenticator: `LAContext.evaluatePolicy`.
pub(crate) struct LocalAuthenticator;

impl Authenticator for LocalAuthenticator {
    fn authenticate(&self, reason: &str, policy: AuthPolicy) -> AuthOutcome {
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
            return AuthOutcome::Unavailable(err.localizedDescription().to_string());
        }

        let (tx, rx) = mpsc::channel::<(bool, Option<String>)>();
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
                Some(error.localizedDescription().to_string())
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
            Ok((false, message)) => {
                AuthOutcome::Denied(message.unwrap_or_else(|| "authentication failed".to_string()))
            }
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
