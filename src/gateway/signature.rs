//! The thinking-signature policy.
//!
//! Anthropic thinking blocks carry an opaque `signature` that binds them to the
//! account that produced them; a client sends it back with the block on the
//! next turn. No OpenAI-protocol upstream produces one, so the gateway signs
//! the blocks it emits itself:
//!
//! ```text
//! ogw1.<base64url payload>.<base64url HMAC-SHA256(key, "ogw1" ‖ len(payload) ‖ payload ‖ thinking)>
//! ```
//!
//! The key is derived from the gateway token, so signatures survive a restart
//! and stop verifying when the token is rotated. The payload is empty for Chat
//! Completions; for the Responses API it carries the reasoning item's
//! `encrypted_content`, which is what lets the next turn replay that reasoning
//! faithfully (`store: false` keeps no server-side state to point at).
//!
//! On replay, a block whose signature verifies is sent upstream as reasoning;
//! one that does not (produced by Anthropic, another gateway, or an older
//! token) is dropped and reported as ignored — the same binding Anthropic
//! applies to its own thinking blocks.

use aws_lc_rs::{digest, hmac};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Version prefix of every gateway signature.
const PREFIX: &str = "ogw1";

/// Domain separator for the key derivation.
const KEY_CONTEXT: &[u8] = b"omni-dev anthropic gateway thinking signature v1\0";

/// Signs and verifies thinking blocks.
pub(crate) struct Signer {
    key: hmac::Key,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Signer(<redacted>)")
    }
}

impl Signer {
    /// A signer keyed from the gateway token.
    pub(crate) fn from_token(token: &str) -> Self {
        let mut material = KEY_CONTEXT.to_vec();
        material.extend_from_slice(token.as_bytes());
        let derived = digest::digest(&digest::SHA256, &material);
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref()),
        }
    }

    /// Signs `thinking`, embedding `payload`.
    pub(crate) fn sign(&self, thinking: &str, payload: &str) -> String {
        let tag = hmac::sign(&self.key, &message(payload, thinking));
        format!(
            "{PREFIX}.{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        )
    }

    /// The embedded payload when `signature` is a valid gateway signature over
    /// `thinking`, else `None`.
    pub(crate) fn verify(&self, thinking: &str, signature: &str) -> Option<String> {
        let mut parts = signature.split('.');
        let (Some(PREFIX), Some(payload), Some(tag), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let payload = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        hmac::verify(&self.key, &message(&payload, thinking), &tag).ok()?;
        Some(payload)
    }
}

/// The MAC input: the prefix, the payload's length, the payload, then the
/// thinking text (length-prefixed so the boundary cannot shift).
fn message(payload: &str, thinking: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX.len() + 8 + payload.len() + thinking.len());
    out.extend_from_slice(PREFIX.as_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(thinking.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_verifies_and_returns_its_payload() {
        let signer = Signer::from_token("token");
        let sig = signer.sign("I think", "");
        assert!(sig.starts_with("ogw1."));
        assert_eq!(signer.verify("I think", &sig).as_deref(), Some(""));
        let with_payload = signer.sign("x", "{\"enc\":\"abc\"}");
        assert_eq!(
            signer.verify("x", &with_payload).as_deref(),
            Some("{\"enc\":\"abc\"}")
        );
    }

    #[test]
    fn tampering_or_another_key_fails() {
        let signer = Signer::from_token("token");
        let sig = signer.sign("I think", "p");
        assert!(signer.verify("I think!", &sig).is_none());
        assert!(Signer::from_token("other")
            .verify("I think", &sig)
            .is_none());
        let forged_payload = sig.replacen(".cA.", ".cQ.", 1);
        assert!(signer.verify("I think", &forged_payload).is_none());
    }

    #[test]
    fn foreign_and_malformed_signatures_fail() {
        let signer = Signer::from_token("token");
        for sig in [
            "",
            "EqQBCkYIBxgCKkA…anthropic",
            "ogw1",
            "ogw1.a",
            "ogw1.!!.AA",
            "ogw1.AA.!!",
            "ogw1.AA.AA.AA",
            "ogw2.AA.AA",
        ] {
            assert!(signer.verify("t", sig).is_none(), "{sig}");
        }
        // A payload that is not UTF-8 is rejected before the MAC check.
        let bad_utf8 = format!("ogw1.{}.AA", URL_SAFE_NO_PAD.encode([0xff, 0xfe]));
        assert!(signer.verify("t", &bad_utf8).is_none());
        assert_eq!(format!("{signer:?}"), "Signer(<redacted>)");
    }
}
