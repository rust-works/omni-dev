//! Provider-neutral issue/change-request types (#1779).
//!
//! A minimal, **GitHub-only** slice of the provider abstraction proposed in
//! `#1573 §3.1`. [`GitProvider`] carries only the [`GitProvider::GitHub`]
//! variant today; `#1573` adds `GitLab { host }` alongside it. Everything
//! downstream of a fetch — question building, routing, and (later) decision
//! verification — consumes only [`IssueDoc`] and friends, so a future GitLab
//! fetcher slots in without touching that code.

use serde::{Deserialize, Serialize};

/// Which forge an [`IssueDoc`] (or [`ItemRef`]) came from.
///
/// Only [`GitHub`](Self::GitHub) exists today. `#1573` adds a `GitLab { host
/// }` variant for self-hosted instances; nothing here should assume `GitHub`
/// is the only case forever, but nothing needs to guard against a second
/// variant yet either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitProvider {
    /// github.com. A GitHub Enterprise host is not supported yet: issue URLs
    /// must be on github.com, and `gh` queries its default host.
    GitHub,
}

/// Whether a referenced item is an issue or a change request (GitHub pull
/// request / GitLab merge request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// A tracking issue.
    Issue,
    /// A pull request (GitHub) or merge request (GitLab), named neutrally
    /// per the issue's "use neutral wording from the start" guidance —
    /// rewording later means re-validating every prompt that mentions it.
    ChangeRequest,
}

/// Whether an item is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    /// Open.
    Open,
    /// Closed (merged or not).
    Closed,
}

/// A human-authored comment on an [`IssueDoc`].
///
/// Bot and system-event noise is filtered out by the fetch layer before an
/// `IssueDoc` is built — this type only ever holds a human comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// The commenter's display/login name.
    pub author: String,
    /// The comment body (raw markdown, as authored).
    pub body: String,
    /// The comment's GitHub database id, used by `verify-decision`'s
    /// `--comment ID` to select one. `None` in hand-built test fixtures.
    #[serde(default)]
    pub id: Option<u64>,
}

/// A reference to another issue or change request, e.g. one cited by a
/// decision comment, or one that closed an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    /// The provider this reference resolves against.
    pub provider: GitProvider,
    /// Opaque full project path (`"owner/repo"` on GitHub; GitLab paths
    /// nest, per `#1573 §2.6`).
    pub project: String,
    /// Whether this reference is an issue or a change request.
    pub kind: ItemKind,
    /// The issue number / GitLab `iid`.
    pub number: u64,
}

impl std::fmt::Display for ItemRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.project, self.number)
    }
}

/// A fetched issue (or, in principle, change request) with everything a Jev
/// question needs to judge it: title, body, human comments, and what closed
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDoc {
    /// Which forge this came from.
    pub provider: GitProvider,
    /// Opaque full project path (`"owner/repo"` on GitHub).
    pub project: String,
    /// The issue number.
    pub number: u64,
    /// Always [`ItemKind::Issue`] today — `#1779` only ever routes issues,
    /// never change requests, but the field stays generic for reuse by a
    /// later `verify-decision` cited-source fetch.
    pub kind: ItemKind,
    /// The issue title.
    pub title: String,
    /// Whether the issue is open or closed.
    pub state: ItemState,
    /// The issue body (raw markdown, as authored).
    pub body: String,
    /// Human comments, in creation order. Bot and system-event comments are
    /// already filtered out.
    pub comments: Vec<Comment>,
    /// Change requests that closed this issue, if any.
    pub closed_by: Vec<ItemRef>,
    /// The issue's web URL.
    pub url: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn item_ref_display_is_project_hash_number() {
        let item_ref = ItemRef {
            provider: GitProvider::GitHub,
            project: "rust-works/omni-dev".to_string(),
            kind: ItemKind::Issue,
            number: 1779,
        };
        assert_eq!(item_ref.to_string(), "rust-works/omni-dev#1779");
    }

    #[test]
    fn git_provider_serialises_lowercase() {
        assert_eq!(
            serde_json::to_value(GitProvider::GitHub).unwrap(),
            serde_json::json!("github")
        );
    }

    #[test]
    fn item_kind_serialises_lowercase() {
        assert_eq!(
            serde_json::to_value(ItemKind::ChangeRequest).unwrap(),
            serde_json::json!("changerequest")
        );
    }

    #[test]
    fn item_state_serialises_lowercase() {
        assert_eq!(
            serde_json::to_value(ItemState::Open).unwrap(),
            serde_json::json!("open")
        );
        assert_eq!(
            serde_json::to_value(ItemState::Closed).unwrap(),
            serde_json::json!("closed")
        );
    }
}
