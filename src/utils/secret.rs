//! Redacting wrapper for secret string values.
//!
//! [`Secret`] holds sensitive credential material (API tokens, session
//! tokens) and guarantees it cannot leak through `Debug` formatting: the
//! hand-written `Debug` impl prints `<redacted>` instead of the value.
//! The type deliberately implements neither `Display` nor any serde
//! traits, so a secret cannot be `{}`-formatted or accidentally
//! serialized; the only way out is an explicit, greppable
//! [`expose_secret`](Secret::expose_secret) call.

use std::fmt;

/// A string secret that redacts itself in `Debug` output.
///
/// Wrapping a credential field in `Secret` makes a containing struct's
/// derived `Debug` print `<redacted>` for that field, so a future
/// `tracing::debug!("{creds:?}")` or `.context(format!("… {creds:?}"))`
/// cannot leak the value into logs or error chains.
///
/// `Secret` implements neither `Display` nor serde traits by design; the
/// wrapped value is only reachable through
/// [`expose_secret`](Self::expose_secret).
///
/// # Examples
///
/// ```
/// use omni_dev::utils::secret::Secret;
///
/// let token = Secret::new("s3cr3t");
/// assert_eq!(format!("{token:?}"), "<redacted>");
/// assert_eq!(token.expose_secret(), "s3cr3t");
/// ```
// PartialEq is derived (not constant-time): fine for test/config equality,
// never use it as an authentication check.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Creates a `Secret` wrapping the given value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the wrapped secret value.
    ///
    /// The name is deliberately loud: every call site is an auditable
    /// point where the secret leaves the redacting wrapper.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_is_redacted() {
        let secret = Secret::new("super-sekret-value");
        let debug = format!("{secret:?}");
        assert_eq!(debug, "<redacted>");
        assert!(!debug.contains("super-sekret-value"));
    }

    #[test]
    fn debug_redacts_inside_derived_container() {
        #[derive(Debug)]
        struct Holder {
            // Read only through the derived Debug impl, which dead-code
            // analysis intentionally ignores.
            #[allow(dead_code)]
            token: Secret,
        }
        let holder = Holder {
            token: "super-sekret-value".into(),
        };
        let debug = format!("{holder:?}");
        assert!(debug.contains("token: <redacted>"));
        assert!(!debug.contains("super-sekret-value"));
    }

    #[test]
    fn expose_secret_returns_the_wrapped_value() {
        assert_eq!(Secret::new("v").expose_secret(), "v");
    }

    #[test]
    fn from_string_and_str_compare_equal() {
        let a: Secret = "tok".into();
        let b: Secret = String::from("tok").into();
        assert_eq!(a, b);
        assert_eq!(a.clone(), a);
    }
}
