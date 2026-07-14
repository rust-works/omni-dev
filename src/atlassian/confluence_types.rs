//! Confluence Cloud REST API wire types (DTOs).
//!
//! Response and request data-transfer objects for the Confluence endpoints
//! served by [`crate::atlassian::client::AtlassianClient`] and
//! [`crate::atlassian::confluence_api::ConfluenceApi`]. Split out of `client.rs`
//! and `confluence_api.rs` (see issue #1156) so those modules hold only
//! transport logic.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single hit from a Confluence CQL search (`GET /wiki/rest/api/content/search`).
///
/// See [`ConfluenceSearchResults`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceSearchResult {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Space key (e.g., "ENG").
    pub space_key: String,
}

/// Paginated wrapper around [`ConfluenceSearchResult`] hits from
/// `GET /wiki/rest/api/content/search`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceSearchResults {
    /// Matching pages.
    pub results: Vec<ConfluenceSearchResult>,
    /// Total number of matching results.
    pub total: u32,
}

/// A single user hit from `GET /wiki/rest/api/search/user`.
///
/// See [`ConfluenceUserSearchResults`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceUserSearchResult {
    /// Account ID (unique identifier). Absent for some user types such as
    /// app users or deactivated users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Display name.
    pub display_name: String,
    /// Email address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Paginated wrapper around [`ConfluenceUserSearchResult`] hits from
/// `GET /wiki/rest/api/search/user`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceUserSearchResults {
    /// Matching users.
    pub users: Vec<ConfluenceUserSearchResult>,
    /// Total number of matching results.
    pub total: u32,
}

/// A single user resolved by account ID via `GET /wiki/rest/api/user?accountId=`.
///
/// The Confluence v1 user object does not report an `active` flag, so `active`
/// is always `None` here. Failure-tolerant in the same way as
/// [`JiraUserRecord`](crate::atlassian::jira_types::JiraUserRecord). See [`ConfluenceUserGetResults`] for the batch wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceUserRecord {
    /// Account ID (always present — echoed back even when the lookup failed).
    pub account_id: String,
    /// Display name (falls back to `publicName` when `displayName` is absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Email address. Absent unless the caller has permission to see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Account type, e.g. `"atlassian"`, `"app"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_type: Option<String>,
    /// Always `None` — the Confluence v1 user endpoint does not return an
    /// active flag. Present for parity with [`JiraUserRecord`](crate::atlassian::jira_types::JiraUserRecord).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    /// Reason this ID could not be resolved (e.g. `"HTTP 404"`). Absent on
    /// success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Batch wrapper around [`ConfluenceUserRecord`] from resolving one or more
/// account IDs via `GET /wiki/rest/api/user?accountId=`.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceUserGetResults {
    /// Resolved users, one per requested account ID (in request order).
    pub users: Vec<ConfluenceUserRecord>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct ConfluenceContentSearchResponse {
    pub(crate) results: Vec<ConfluenceContentSearchEntry>,
    #[serde(default)]
    pub(crate) size: u32,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceSearchLinks>,
}

#[derive(Deserialize, Default)]
pub(crate) struct ConfluenceSearchLinks {
    pub(crate) next: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceContentSearchEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(rename = "_expandable")]
    pub(crate) expandable: Option<ConfluenceExpandable>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceExpandable {
    pub(crate) space: Option<String>,
}

// ── Confluence user search API response structs ───────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceUserSearchResponse {
    pub(crate) results: Vec<ConfluenceUserSearchEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceSearchLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceUserSearchEntry {
    #[serde(default)]
    pub(crate) user: Option<ConfluenceSearchUser>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSearchUser {
    #[serde(rename = "accountId", default)]
    pub(crate) account_id: Option<String>,
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
    #[serde(default)]
    pub(crate) email: Option<String>,
    #[serde(rename = "publicName", default)]
    pub(crate) public_name: Option<String>,
}

// ── Confluence user-get API response struct ───────────────────────

/// Deserialization helper for `GET /wiki/rest/api/user?accountId=` — a bare
/// user object (not wrapped in `results`). `publicName` is the fallback when
/// `displayName` is unavailable; the v1 endpoint has no `active` flag.
#[derive(Deserialize)]
pub(crate) struct ConfluenceUserGetEntry {
    #[serde(rename = "accountId", default)]
    pub(crate) account_id: Option<String>,
    #[serde(rename = "accountType", default)]
    pub(crate) account_type: Option<String>,
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
    #[serde(rename = "publicName", default)]
    pub(crate) public_name: Option<String>,
    #[serde(default)]
    pub(crate) email: Option<String>,
}

// ── Internal API response structs ───────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluencePageResponse {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    #[serde(rename = "spaceId")]
    pub(crate) space_id: String,
    pub(crate) version: Option<ConfluenceVersion>,
    pub(crate) body: Option<ConfluenceBody>,
    #[serde(rename = "parentId")]
    pub(crate) parent_id: Option<String>,
    #[serde(default)]
    pub(crate) ancestors: Vec<ConfluenceAncestorEntry>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceAncestorEntry {
    pub(crate) id: String,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceVersion {
    pub(crate) number: u32,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceBody {
    pub(crate) atlas_doc_format: Option<ConfluenceAtlasDoc>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceAtlasDoc {
    pub(crate) value: String,
}

// ── Space lookup ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpaceResponse {
    pub(crate) key: String,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpacesResponse {
    pub(crate) results: Vec<ConfluenceSpaceEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceSpaceLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpaceLinks {
    pub(crate) next: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpaceEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) key: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(rename = "type", default)]
    pub(crate) type_: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(rename = "homepageId", default)]
    pub(crate) homepage_id: Option<String>,
}

/// A Confluence space.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceSpace {
    /// Space ID.
    pub id: String,
    /// Space key (e.g. "ENG").
    pub key: String,
    /// Display name.
    pub name: String,
    /// Space type ("global", "personal", "collaboration", "knowledge_base").
    #[serde(rename = "type")]
    pub type_: String,
    /// Status ("current" or "archived").
    pub status: String,
    /// Homepage page ID, when reported by the API.
    #[serde(rename = "homepageId", skip_serializing_if = "Option::is_none")]
    pub homepage_id: Option<String>,
}

impl From<ConfluenceSpaceEntry> for ConfluenceSpace {
    fn from(e: ConfluenceSpaceEntry) -> Self {
        Self {
            id: e.id,
            key: e.key.unwrap_or_default(),
            name: e.name.unwrap_or_default(),
            type_: e.type_.unwrap_or_default(),
            status: e.status.unwrap_or_default(),
            homepage_id: e.homepage_id,
        }
    }
}

/// A page of spaces returned by [`ConfluenceApi::list_spaces`](crate::atlassian::confluence_api::ConfluenceApi::list_spaces).
///
/// Pagination is *not* auto-drained: callers receive one page at a time and
/// pass `next_cursor` back to fetch the next page. Mirrors the
/// [`ConfluenceAttachmentPage`] shape so MCP/CLI callers can stream large
/// space inventories without buffering everything in memory.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceSpacePage {
    /// Spaces on this page.
    pub results: Vec<ConfluenceSpace>,
    /// Opaque cursor for the next page, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A summary record for a Confluence page in a space.
#[derive(Debug, Clone, Serialize)]
pub struct PageSummary {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Page status (e.g. `current`, `archived`, `draft`, `trashed`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Parent page ID, when reported by the API.
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Author account ID, when reported by the API.
    #[serde(rename = "authorId", skip_serializing_if = "Option::is_none")]
    pub author_id: Option<String>,
    /// ISO 8601 creation timestamp, when reported by the API.
    #[serde(rename = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// A page of [`PageSummary`] records returned by
/// [`ConfluenceApi::list_space_pages`](crate::atlassian::confluence_api::ConfluenceApi::list_space_pages).
///
/// Pagination is *not* auto-drained: callers receive one page at a time and
/// pass `next_cursor` back to fetch the next page. Spaces can contain
/// thousands of pages, so we avoid buffering the whole inventory in memory.
#[derive(Debug, Clone, Serialize)]
pub struct PageSummaryPage {
    /// Pages on this response.
    pub results: Vec<PageSummary>,
    /// Opaque cursor for the next page, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── Children response ──────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceChildrenResponse {
    pub(crate) results: Vec<ConfluenceChildEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceChildrenLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceChildEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceChildrenLinks {
    pub(crate) next: Option<String>,
}

// V2 space-pages response (for `depth=root`).
#[derive(Deserialize)]
pub(crate) struct ConfluenceSpacePagesResponse {
    pub(crate) results: Vec<ConfluenceSpacePageEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceChildrenLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpacePageEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(rename = "parentId", default)]
    pub(crate) parent_id: Option<String>,
}

// V2 space-pages response carrying author/createdAt for `list_space_pages`.
// Kept separate from `ConfluenceSpacePagesResponse` to avoid widening that
// type's contract (used by `get_space_root_pages`).
#[derive(Deserialize)]
pub(crate) struct ConfluenceSpacePagesSummaryResponse {
    pub(crate) results: Vec<ConfluenceSpacePageSummaryEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceChildrenLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceSpacePageSummaryEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(rename = "parentId", default)]
    pub(crate) parent_id: Option<String>,
    #[serde(rename = "authorId", default)]
    pub(crate) author_id: Option<String>,
    #[serde(rename = "createdAt", default)]
    pub(crate) created_at: Option<String>,
}

/// A child page returned from the children API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChildPage {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Page status (e.g. "current", "draft"). Empty if not provided by the API.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Parent page ID, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Space key, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_key: Option<String>,
}

// ── Comment types ─────────────────────────────────────────────────

/// Distinguishes the two kinds of Confluence page comments.
///
/// Confluence v2 exposes footer comments (page-level discussion) and inline
/// comments (anchored to a text selection) on separate endpoints. Tracking the
/// kind on each [`ConfluenceComment`] lets a merged listing identify which
/// endpoint each entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommentKind {
    /// A page-level footer comment.
    Footer,
    /// A comment anchored to a text selection in the page body.
    Inline,
}

impl CommentKind {
    /// Returns the URL segment Confluence v2 uses for this kind
    /// (`"footer-comments"` or `"inline-comments"`).
    #[must_use]
    pub fn endpoint_segment(self) -> &'static str {
        match self {
            Self::Footer => "footer-comments",
            Self::Inline => "inline-comments",
        }
    }
}

impl std::fmt::Display for CommentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Footer => f.write_str("footer"),
            Self::Inline => f.write_str("inline"),
        }
    }
}

/// Anchor metadata required when creating an inline comment.
///
/// Confluence's `inline-comment-properties` payload identifies which text
/// selection on the page the comment attaches to. `match_index` is 0-based;
/// `match_count` is the total number of occurrences of `text` on the page.
#[derive(Debug, Clone)]
pub struct InlineAnchor {
    /// The selected text the comment anchors to.
    pub text: String,
    /// 0-based index of which occurrence on the page this comment anchors to.
    pub match_index: usize,
    /// Total number of occurrences of `text` on the page.
    pub match_count: usize,
}

/// A comment on a Confluence page.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceComment {
    /// Comment ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Whether this is a footer or inline comment.
    pub kind: CommentKind,
    /// Comment body as raw ADF JSON.
    pub body_adf: Option<serde_json::Value>,
    /// ISO 8601 creation timestamp.
    pub created: String,
    /// For inline comments: the `id` of the `annotation` mark this comment is
    /// anchored to inside the page ADF (Confluence's `inlineMarkerRef`). `None`
    /// for footer comments. Used to locate — and re-anchor — the highlighted run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_marker_ref: Option<String>,
    /// For inline comments: the plaintext the reviewer originally highlighted
    /// (Confluence's `inlineOriginalSelection`). This is durable — it does not
    /// drift when the surrounding page text is edited — so it is the ground
    /// truth for inline-comment drift auditing. `None` for footer comments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_original_selection: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentsResponse {
    pub(crate) results: Vec<ConfluenceCommentEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceCommentsLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentsLinks {
    pub(crate) next: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentEntry {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) version: Option<ConfluenceCommentVersion>,
    #[serde(default)]
    pub(crate) body: Option<ConfluenceCommentBody>,
    /// Inline-comment anchor metadata. Present on the v2 `inline-comments`
    /// endpoint regardless of `body-format`; absent for footer comments.
    #[serde(default)]
    pub(crate) properties: Option<ConfluenceInlineCommentProperties>,
}

/// The `properties` object on an inline-comment entry. Both fields are optional
/// so we deserialize tolerantly across footer comments (no `properties`) and any
/// future shape changes.
#[derive(Deserialize)]
pub(crate) struct ConfluenceInlineCommentProperties {
    /// The `annotation`-mark `id` this comment is anchored to in the page ADF.
    #[serde(rename = "inlineMarkerRef", default)]
    pub(crate) inline_marker_ref: Option<String>,
    /// The plaintext the reviewer highlighted when the comment was posted.
    #[serde(rename = "inlineOriginalSelection", default)]
    pub(crate) inline_original_selection: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentVersion {
    #[serde(rename = "authorId", default)]
    pub(crate) author_id: Option<String>,
    #[serde(rename = "createdAt", default)]
    pub(crate) created_at: Option<String>,
    /// Monotonic version number. Absent from list responses we ignore it on;
    /// present when a single comment is fetched to bump for an update.
    #[serde(default)]
    pub(crate) number: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentBody {
    pub(crate) atlas_doc_format: Option<ConfluenceAtlasDoc>,
}

#[derive(Serialize)]
pub(crate) struct ConfluenceAddCommentRequest {
    #[serde(rename = "pageId")]
    pub(crate) page_id: String,
    pub(crate) body: ConfluenceUpdateBody,
}

#[derive(Serialize)]
pub(crate) struct ConfluenceAddInlineCommentRequest {
    #[serde(rename = "pageId")]
    pub(crate) page_id: String,
    pub(crate) body: ConfluenceUpdateBody,
    #[serde(rename = "inlineCommentProperties")]
    pub(crate) inline_comment_properties: InlineCommentProperties,
}

#[derive(Serialize)]
pub(crate) struct InlineCommentProperties {
    #[serde(rename = "textSelection")]
    pub(crate) text_selection: String,
    #[serde(rename = "textSelectionMatchCount")]
    pub(crate) text_selection_match_count: usize,
    #[serde(rename = "textSelectionMatchIndex")]
    pub(crate) text_selection_match_index: usize,
}

/// Request body for `PUT /wiki/api/v2/{footer|inline}-comments/{id}` — used by
/// both comment edit and inline-comment resolve/reopen. `resolved` is only sent
/// for inline comments (footer comments cannot be resolved), so it is omitted
/// when `None`.
#[derive(Serialize)]
pub(crate) struct ConfluenceUpdateCommentRequest {
    pub(crate) version: ConfluenceUpdateVersion,
    pub(crate) body: ConfluenceUpdateBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resolved: Option<bool>,
}

/// Minimal read of a single comment (`GET /wiki/api/v2/{segment}/{id}`) — just
/// enough to learn its current `version.number` (required to build the next
/// version on update) and its current ADF body (needed to re-send an unchanged
/// body when only toggling the `resolved` flag). Reuses the shared
/// [`ConfluenceCommentVersion`] / [`ConfluenceCommentBody`] shapes.
#[derive(Deserialize)]
pub(crate) struct ConfluenceCommentDetail {
    #[serde(default)]
    pub(crate) version: Option<ConfluenceCommentVersion>,
    #[serde(default)]
    pub(crate) body: Option<ConfluenceCommentBody>,
}

/// Response of `GET /wiki/rest/api/user/watch/content/{id}` — whether the
/// (current or specified) user is watching the content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfluenceWatchStatus {
    /// Whether the user is watching the content.
    #[serde(default)]
    pub watching: bool,
}

// ── Labels ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceLabelsResponse {
    pub(crate) results: Vec<ConfluenceLabelEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceLabelsLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceLabelEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) prefix: String,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceLabelsLinks {
    pub(crate) next: Option<String>,
}

/// A label on a Confluence page.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceLabel {
    /// Label ID.
    pub id: String,
    /// Label name.
    pub name: String,
    /// Label prefix (e.g. "global").
    pub prefix: String,
}

#[derive(Serialize)]
pub(crate) struct ConfluenceAddLabelEntry {
    pub(crate) prefix: String,
    pub(crate) name: String,
}

// ── Versions ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceVersionsResponse {
    pub(crate) results: Vec<ConfluenceVersionEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceVersionsLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceVersionEntry {
    pub(crate) number: u32,
    #[serde(rename = "createdAt", default)]
    pub(crate) created_at: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(rename = "minorEdit", default)]
    pub(crate) minor_edit: Option<bool>,
    #[serde(rename = "authorId", default)]
    pub(crate) author_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceVersionsLinks {
    pub(crate) next: Option<String>,
}

/// A single version entry from a Confluence page's history.
///
/// Optional fields (`created_at`, `author_id`, `message`) are returned as
/// empty strings when the API omits them — older pages can have null author
/// or timestamp data, see issue #708.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageVersion {
    /// Version number (1-based; current version at the head of the list).
    pub number: u32,
    /// ISO 8601 creation timestamp; empty if the API returned null.
    #[serde(default)]
    pub created_at: String,
    /// Account ID of the author; empty if the API returned null.
    #[serde(default)]
    pub author_id: String,
    /// Version comment / edit message; empty if the API returned null.
    #[serde(default)]
    pub message: String,
    /// Whether the edit was marked as minor.
    #[serde(default)]
    pub minor_edit: bool,
}

/// Filter applied to a version listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinceFilter {
    /// Keep versions whose `number >= n`.
    Version(u32),
    /// Keep versions whose `created_at >= iso` (lexicographic compare on
    /// ISO 8601 strings — ordering is correct as long as the timestamps
    /// are fully qualified with offsets, which Confluence's API guarantees).
    CreatedAt(String),
}

impl SinceFilter {
    /// Parses a `since` parameter. A purely numeric input is interpreted as
    /// a version number; anything containing `-` or `T` (the typical ISO 8601
    /// markers) is treated as a date.
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!("`since` must be a version number or ISO 8601 date");
        }
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            let n: u32 = trimmed
                .parse()
                .with_context(|| format!("Invalid version number \"{trimmed}\""))?;
            return Ok(Self::Version(n));
        }
        if trimmed.contains('-') || trimmed.contains('T') {
            return Ok(Self::CreatedAt(trimmed.to_string()));
        }
        anyhow::bail!(
            "`since` must be a numeric version (e.g. \"5\") or ISO 8601 date \
             (e.g. \"2026-01-01T00:00:00Z\"); got \"{trimmed}\""
        );
    }

    /// Whether `version` satisfies this filter (i.e. should be kept).
    pub(crate) fn matches(&self, version: &PageVersion) -> bool {
        match self {
            Self::Version(min) => version.number >= *min,
            Self::CreatedAt(min) => {
                if version.created_at.is_empty() {
                    // Tolerate missing timestamps: treat as too-old.
                    false
                } else {
                    version.created_at.as_str() >= min.as_str()
                }
            }
        }
    }
}

// ── Page metadata ──────────────────────────────────────────────────

/// Lightweight metadata about a Confluence page, returned by
/// [`ConfluenceApi::get_page_metadata`](crate::atlassian::confluence_api::ConfluenceApi::get_page_metadata).
#[derive(Debug, Clone, Serialize)]
pub struct PageMetadata {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Current version number, if known.
    pub current_version: Option<u32>,
}

// ── Attachments ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ConfluenceAttachmentsResponse {
    pub(crate) results: Vec<ConfluenceAttachmentEntry>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceAttachmentLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceAttachmentLinks {
    pub(crate) next: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceAttachmentEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(rename = "mediaType", default)]
    pub(crate) media_type: Option<String>,
    #[serde(rename = "fileSize", default)]
    pub(crate) file_size: Option<u64>,
    #[serde(rename = "downloadLink", default)]
    pub(crate) download_link: Option<String>,
    #[serde(default)]
    pub(crate) version: Option<ConfluenceAttachmentVersion>,
    #[serde(rename = "pageId", default)]
    pub(crate) page_id: Option<String>,
    #[serde(rename = "fileId", default)]
    pub(crate) file_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceAttachmentVersion {
    pub(crate) number: u32,
}

// ── v1 attachment-upload response ───────────────────────────────────
//
// Attachment *creation* is only available on the Confluence Cloud v1 REST
// API (`POST /wiki/rest/api/content/{id}/child/attachment`); the v2 API
// exposes no attachment-creation endpoint. The v1 response nests its
// metadata differently from the v2 list shape above, so it needs its own
// deserialization structs.
#[derive(Deserialize)]
pub(crate) struct ConfluenceV1AttachmentResponse {
    pub(crate) results: Vec<ConfluenceV1AttachmentEntry>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceV1AttachmentEntry {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) extensions: Option<ConfluenceV1AttachmentExtensions>,
    #[serde(default)]
    pub(crate) version: Option<ConfluenceAttachmentVersion>,
    #[serde(default)]
    pub(crate) container: Option<ConfluenceV1AttachmentContainer>,
    #[serde(rename = "_links", default)]
    pub(crate) links: Option<ConfluenceV1AttachmentEntryLinks>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceV1AttachmentExtensions {
    #[serde(rename = "mediaType", default)]
    pub(crate) media_type: Option<String>,
    #[serde(rename = "fileSize", default)]
    pub(crate) file_size: Option<u64>,
    #[serde(rename = "fileId", default)]
    pub(crate) file_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceV1AttachmentContainer {
    #[serde(default)]
    pub(crate) id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceV1AttachmentEntryLinks {
    #[serde(default)]
    pub(crate) download: Option<String>,
}

impl From<ConfluenceV1AttachmentEntry> for ConfluenceAttachment {
    fn from(e: ConfluenceV1AttachmentEntry) -> Self {
        let (media_type, file_size, file_id) = match e.extensions {
            Some(x) => (x.media_type, x.file_size, x.file_id),
            None => (None, None, None),
        };
        Self {
            id: e.id,
            title: e.title,
            media_type,
            file_size,
            download_url: e.links.and_then(|l| l.download),
            version: e.version.map(|v| v.number),
            page_id: e.container.and_then(|c| c.id),
            file_id,
        }
    }
}

/// An attachment on a Confluence page.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceAttachment {
    /// Attachment ID (used for delete and get).
    pub id: String,
    /// Display title (filename).
    pub title: String,
    /// MIME type, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// File size in bytes, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// Download URL path or absolute URL, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    /// Version number, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Owning page ID, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
    /// Underlying file ID, when reported by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
}

impl From<ConfluenceAttachmentEntry> for ConfluenceAttachment {
    fn from(e: ConfluenceAttachmentEntry) -> Self {
        Self {
            id: e.id,
            title: e.title,
            media_type: e.media_type,
            file_size: e.file_size,
            download_url: e.download_link,
            version: e.version.map(|v| v.number),
            page_id: e.page_id,
            file_id: e.file_id,
        }
    }
}

/// A page of attachments returned by [`ConfluenceApi::list_attachments`](crate::atlassian::confluence_api::ConfluenceApi::list_attachments).
///
/// Pagination is *not* auto-drained: callers receive one page at a time and
/// pass `next_cursor` back to fetch the next page. Other v2 list helpers in
/// this module (e.g. [`ConfluenceApi::get_labels`](crate::atlassian::confluence_api::ConfluenceApi::get_labels)) auto-drain — attachments
/// expose the cursor explicitly so MCP/CLI callers can stream very large
/// attachment lists without buffering everything in memory.
#[derive(Debug, Clone, Serialize)]
pub struct ConfluenceAttachmentPage {
    /// Attachments on this page.
    pub results: Vec<ConfluenceAttachment>,
    /// Opaque cursor for the next page, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── Create request ─────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct ConfluenceCreateRequest {
    #[serde(rename = "spaceId")]
    pub(crate) space_id: String,
    pub(crate) title: String,
    pub(crate) body: ConfluenceUpdateBody,
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,
    pub(crate) status: String,
}

#[derive(Deserialize)]
pub(crate) struct ConfluenceCreateResponse {
    pub(crate) id: String,
}

// ── Update request ──────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct ConfluenceUpdateRequest {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) title: String,
    pub(crate) body: ConfluenceUpdateBody,
    pub(crate) version: ConfluenceUpdateVersion,
}

#[derive(Serialize)]
pub(crate) struct ConfluenceUpdateBody {
    pub(crate) representation: String,
    pub(crate) value: String,
}

#[derive(Serialize)]
pub(crate) struct ConfluenceUpdateVersion {
    pub(crate) number: u32,
    pub(crate) message: Option<String>,
}

// ── Move types ─────────────────────────────────────────────────────

/// Position for [`ConfluenceApi::move_page`](crate::atlassian::confluence_api::ConfluenceApi::move_page). Same-space only —
/// cross-space moves are not supported by the v2 API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePosition {
    /// Place the page as the last child of the target (target becomes the new parent).
    Append,
    /// Place the page as a sibling immediately before the target.
    Before,
    /// Place the page as a sibling immediately after the target.
    After,
}

impl MovePosition {
    /// Returns the URL-path segment used by the Confluence move endpoint.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// Updated page metadata returned by [`ConfluenceApi::move_page`](crate::atlassian::confluence_api::ConfluenceApi::move_page).
#[derive(Debug, Clone, Serialize)]
pub struct MovedPage {
    /// Page ID.
    pub id: String,
    /// Page title.
    pub title: String,
    /// New parent page ID, if the page now has a parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Ancestor page IDs from root toward the immediate parent.
    pub ancestors: Vec<String>,
}
