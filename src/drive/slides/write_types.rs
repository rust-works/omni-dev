//! One typed replacement request, always under revision control.
use serde::{Deserialize, Serialize};

/// Revision assertion; no force/rebase alternative exists.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteControl {
    /// Revision obtained from the immediately preceding get.
    pub required_revision_id: String,
}

/// Exactly one mutation per invocation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchUpdatePresentationRequest {
    requests: [SlidesRequest; 1],
    write_control: WriteControl,
}
impl BatchUpdatePresentationRequest {
    /// Builds a single-request revision-controlled batch.
    pub fn new(request: SlidesRequest, revision: &str) -> Self {
        Self {
            requests: [request],
            write_control: WriteControl {
                required_revision_id: revision.into(),
            },
        }
    }
}

/// Only text replacement is reachable; object deletion is unrepresentable.
#[derive(Debug, Clone, Serialize)]
pub enum SlidesRequest {
    /// Literal replacement on explicit ordinary-slide page IDs.
    #[serde(rename = "replaceAllText")]
    ReplaceAllText {
        /// Literal match criteria.
        #[serde(rename = "containsText")]
        contains_text: SubstringMatchCriteria,
        /// Replacement prose.
        #[serde(rename = "replaceText")]
        replace_text: String,
        /// Always nonempty, validated by the guarded engine.
        #[serde(rename = "pageObjectIds")]
        page_object_ids: Vec<String>,
    },
}
impl SlidesRequest {
    /// Builds the only supported Slides mutation.
    pub fn replace_all_text(
        search: &str,
        replace: &str,
        match_case: bool,
        pages: &[String],
    ) -> Self {
        Self::ReplaceAllText {
            contains_text: SubstringMatchCriteria {
                text: search.into(),
                match_case,
            },
            replace_text: replace.into(),
            page_object_ids: pages.to_vec(),
        }
    }
}

/// Literal substring criteria. No regex option is exposed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubstringMatchCriteria {
    /// Text to match.
    pub text: String,
    /// Case-sensitive by default in the CLI.
    pub match_case: bool,
}

/// Replacement batch response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BatchUpdatePresentationResponse {
    /// One reply per request.
    pub replies: Vec<SlidesReply>,
}
impl BatchUpdatePresentationResponse {
    /// Resolves proto3's omitted zero count at the boundary.
    pub fn occurrences_changed_for_replace(&self) -> i64 {
        self.replies
            .first()
            .and_then(|r| r.replace_all_text.as_ref())
            .map_or(0, |r| r.occurrences_changed)
    }
}

/// Reply to one request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SlidesReply {
    /// Replacement result.
    pub replace_all_text: Option<ReplaceAllTextReply>,
}

/// Server-reported replacement count.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReplaceAllTextReply {
    /// Zero is commonly omitted in proto3 JSON.
    pub occurrences_changed: i64,
}
