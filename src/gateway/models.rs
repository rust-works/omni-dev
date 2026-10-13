//! Mapping the client's model names onto upstream models.
//!
//! Resolution stops at the first match: an exact map key, then the first glob
//! key that matches (`*` matches any run of characters), then — for a name
//! that does not start with `claude-` — the name itself, so a client can name
//! an upstream model directly; then the default model. A `claude-…` name with
//! no match and no default is a `not_found_error`, since silently answering it
//! with some other model would hide a configuration mistake.

use anyhow::{bail, Result};

use super::error::GatewayError;

/// The client model names the gateway serves, and what each becomes upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelMap {
    /// `(pattern, upstream model)` in configuration order.
    rules: Vec<(String, String)>,
    /// The fallback upstream model.
    default: Option<String>,
}

impl ModelMap {
    /// Builds a map from `pattern=model` entries and an optional default.
    pub fn new(entries: &[String], default: Option<String>) -> Result<Self> {
        let mut rules = Vec::new();
        for entry in entries {
            for item in entry.split(',').map(str::trim).filter(|i| !i.is_empty()) {
                let Some((pattern, model)) = item.split_once('=') else {
                    bail!("model map entry `{item}` is not `pattern=model`");
                };
                let (pattern, model) = (pattern.trim(), model.trim());
                if pattern.is_empty() || model.is_empty() {
                    bail!("model map entry `{item}` has an empty side");
                }
                rules.push((pattern.to_string(), model.to_string()));
            }
        }
        Ok(Self {
            rules,
            default: default.filter(|d| !d.is_empty()),
        })
    }

    /// The upstream model for `requested`.
    pub(crate) fn resolve(&self, requested: &str) -> Result<String, GatewayError> {
        if let Some((_, model)) = self.rules.iter().find(|(p, _)| p == requested) {
            return Ok(model.clone());
        }
        if let Some((_, model)) = self
            .rules
            .iter()
            .find(|(p, _)| p.contains('*') && glob_matches(p, requested))
        {
            return Ok(model.clone());
        }
        if !requested.starts_with("claude-") && !requested.is_empty() {
            return Ok(requested.to_string());
        }
        self.default.clone().ok_or_else(|| {
            GatewayError::not_found(format!(
                "model `{requested}` is not mapped to an upstream model; add it to \
                 OMNI_DEV_GATEWAY_MODEL_MAP or set OMNI_DEV_GATEWAY_MODEL"
            ))
        })
    }

    /// The model ids listed by `GET /v1/models`: the exact (non-glob) keys,
    /// then the default model.
    pub(crate) fn listed(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let exact = self
            .rules
            .iter()
            .map(|(p, _)| p)
            .filter(|p| !p.contains('*'));
        for id in exact.chain(self.default.iter()) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// The default upstream model.
    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The configured `(pattern, model)` rules.
    #[must_use]
    pub fn rules(&self) -> &[(String, String)] {
        &self.rules
    }
}

/// Whether `text` matches `pattern`, where `*` matches any run of characters.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = parts.split_first().unwrap_or((&"", &[]));
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return remaining.is_empty();
    };
    for part in middle {
        match remaining.find(part) {
            Some(at) => remaining = &remaining[at + part.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn map(entries: &str, default: Option<&str>) -> ModelMap {
        ModelMap::new(&[entries.to_string()], default.map(str::to_string)).unwrap()
    }

    #[test]
    fn exact_beats_glob_and_globs_go_in_order() {
        let m = map(
            "claude-opus-*=big, claude-opus-4-7=exact,claude-*=any",
            None,
        );
        assert_eq!(m.resolve("claude-opus-4-7").unwrap(), "exact");
        assert_eq!(m.resolve("claude-opus-5").unwrap(), "big");
        assert_eq!(m.resolve("claude-haiku-4-5").unwrap(), "any");
    }

    #[test]
    fn non_claude_names_pass_through_and_claude_names_need_a_default() {
        let m = map("", None);
        assert_eq!(
            m.resolve("accounts/fireworks/models/glm").unwrap(),
            "accounts/fireworks/models/glm"
        );
        let err = m.resolve("claude-sonnet-5").unwrap_err();
        assert_eq!(err.status.as_u16(), 404);
        assert!(err.message.contains("OMNI_DEV_GATEWAY_MODEL"));
        assert_eq!(map("", Some("d")).resolve("claude-sonnet-5").unwrap(), "d");
        assert_eq!(map("", Some("d")).resolve("").unwrap(), "d");
    }

    #[test]
    fn malformed_entries_are_errors() {
        assert!(ModelMap::new(&["nope".into()], None).is_err());
        assert!(ModelMap::new(&["a=".into()], None).is_err());
        assert!(ModelMap::new(&["=b".into()], None).is_err());
        assert!(ModelMap::new(&["a=b,,".into()], Some(String::new()))
            .unwrap()
            .default_model()
            .is_none());
    }

    #[test]
    fn listed_is_exact_keys_then_default_deduplicated() {
        let m = map("claude-x=a,claude-*=b,up=c", Some("up"));
        assert_eq!(m.listed(), vec!["claude-x", "up"]);
        assert_eq!(m.rules().len(), 3);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_matches("a*", "abc"));
        assert!(glob_matches("*c", "abc"));
        assert!(glob_matches("a*c", "abc"));
        assert!(glob_matches("a*b*c", "a-b-c"));
        assert!(glob_matches("*", ""));
        assert!(!glob_matches("a*c", "ab"));
        assert!(!glob_matches("a*b*c", "a-c"));
        assert!(!glob_matches("b*", "abc"));
        assert!(!glob_matches("abc", "abcd"));
        assert!(!glob_matches("ab*ab", "ab"));
    }
}
