//! Environment-variable dependency-injection seam.
//!
//! Production code reads the process environment only through an
//! [`EnvSource`]. The real implementation, [`SystemEnv`], delegates to
//! [`std::env::var`]; tests inject a pure in-memory fake
//! (`crate::test_support::env::MapEnv`) instead of mutating the
//! process-global environment.
//!
//! This removes the shared mutable global that the cross-module test race
//! (issue #821) and the per-module env mutexes (#950, #1030) were fighting
//! over: a test that constructs its own [`EnvSource`] never touches process
//! env, so it needs no lock and runs fully in parallel. See
//! [STYLE-0028](../../docs/STYLE_GUIDE.md) and `docs/plan/issue-1030-env-di.md`.
//!
//! `EnvSource` abstracts the **raw** environment only. The
//! settings.json fallback layer composes on top of it — see
//! [`crate::utils::settings`].

/// A read-only view of environment variables.
///
/// Implemented by [`SystemEnv`] (the real process environment) in production
/// and by an in-memory map in tests, so env-parsing boundaries can be tested
/// without mutating the process-global environment.
pub trait EnvSource {
    /// Returns the value of `key`, or `None` if it is unset (or, for the
    /// process environment, not valid Unicode).
    fn var(&self, key: &str) -> Option<String>;

    /// Returns the first set value among `keys`, in order.
    fn var_any(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|k| self.var(k))
    }
}

/// The real process environment, backed by [`std::env::var`].
///
/// This is the production [`EnvSource`]; pass `&SystemEnv` from the thin
/// env-resolving wrapper that fronts each boundary seam.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// `&T` is an `EnvSource` whenever `T` is, so callers can pass `&SystemEnv`
/// or `&map_env` to functions taking `&impl EnvSource` without ceremony.
impl<T: EnvSource + ?Sized> EnvSource for &T {
    fn var(&self, key: &str) -> Option<String> {
        (**self).var(key)
    }
}

/// Returns `var`'s value when it is set and non-empty.
///
/// The docs promise resolution "stopping at the first non-empty value", and
/// treating `VAR=` as unset lets users neutralise an exported variable for a
/// single invocation.
pub(crate) fn non_empty_var(env: &impl EnvSource, key: &str) -> Option<String> {
    env.var(key).filter(|v| !v.is_empty())
}

/// Parses `key`'s value as a truthy boolean: `1`, `true`, or `yes` (trimmed,
/// case-insensitive) is `true`; `0`, `false`, or `no` is `false`; unset or
/// empty is `false` with no warning (the ordinary "not set" case). Anything
/// else — a typo like `treu` — is also treated as `false`, but logs a
/// warning first: a silently-discarded, unrecognized value is exactly the
/// broken-configuration case docs/STYLE_GUIDE.md's silent-discard rule
/// warns against (issue #1677 review finding).
pub(crate) fn truthy_var(env: &impl EnvSource, key: &str) -> bool {
    let Some(raw) = non_empty_var(env, key) else {
        return false;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => true,
        "0" | "false" | "no" => false,
        _ => {
            tracing::warn!(
                "{key}={raw:?} is not a recognized boolean (expected 1/true/yes or \
                 0/false/no); treating it as unset"
            );
            false
        }
    }
}

/// RAII guard that sets a process env var for the duration of a scope,
/// restoring (or removing) whatever was there before when the guard drops —
/// so a caller that mutates process env can be invoked more than once per
/// process without leaking state between calls (#1538).
pub(crate) struct ScopedEnvVar {
    key: &'static str,
    previous: Option<String>,
}

impl ScopedEnvVar {
    /// Sets `key` to `value`, snapshotting the previous value (if any) to
    /// restore on drop.
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for ScopedEnvVar {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    #[test]
    fn map_env_returns_inserted_values_and_none_otherwise() {
        let env = MapEnv::new().with("USE_OPENAI", "true");
        assert_eq!(env.var("USE_OPENAI").as_deref(), Some("true"));
        assert_eq!(env.var("MISSING"), None);
    }

    #[test]
    fn var_any_returns_first_set_key() {
        let env = MapEnv::new().with("ANTHROPIC_API_KEY", "k");
        assert_eq!(
            env.var_any(&["CLAUDE_API_KEY", "ANTHROPIC_API_KEY"])
                .as_deref(),
            Some("k")
        );
        assert_eq!(env.var_any(&["A", "B"]), None);
    }

    #[test]
    fn reference_forwards_to_inner_source() {
        let env = MapEnv::new().with("K", "v");
        fn read(src: &impl EnvSource) -> Option<String> {
            src.var("K")
        }
        // &MapEnv must itself satisfy `impl EnvSource`.
        assert_eq!(read(&&env).as_deref(), Some("v"));
    }

    #[test]
    fn truthy_var_parses_truthy_and_falsy_values_case_insensitively() {
        for v in ["1", "true", "TRUE", "yes", "YES"] {
            assert!(truthy_var(&MapEnv::new().with("FLAG", v), "FLAG"), "{v:?}");
        }
        for v in ["0", "false", "FALSE", "no", "NO"] {
            assert!(!truthy_var(&MapEnv::new().with("FLAG", v), "FLAG"), "{v:?}");
        }
    }

    #[test]
    fn truthy_var_treats_unset_as_false() {
        assert!(!truthy_var(&MapEnv::new(), "FLAG"));
    }

    #[test]
    fn truthy_var_treats_unrecognized_value_as_false() {
        assert!(!truthy_var(&MapEnv::new().with("FLAG", "treu"), "FLAG"));
    }
}
