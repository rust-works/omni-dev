//! JIRA Cloud REST API wire types (DTOs).
//!
//! Response and request data-transfer objects for the JIRA endpoints served by
//! [`crate::atlassian::client::AtlassianClient`]. Split out of `client.rs` (see
//! issue #1156) so that module holds only transport logic.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// JIRA issue data returned by `GET /rest/api/3/issue/{key}`.
///
/// The [`custom_fields`](Self::custom_fields) vector is selection-gated:
/// it is empty under the default [`FieldSelection::Standard`] and only
/// populated when the request used [`FieldSelection::Named`] or
/// [`FieldSelection::All`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraIssue {
    /// Issue key (e.g., "PROJ-123").
    pub key: String,

    /// Issue summary (title).
    pub summary: String,

    /// Issue description as raw ADF JSON (may be null).
    pub description_adf: Option<serde_json::Value>,

    /// Issue status name.
    pub status: Option<String>,

    /// Issue type name.
    pub issue_type: Option<String>,

    /// Assignee display name.
    pub assignee: Option<String>,

    /// Priority name.
    pub priority: Option<String>,

    /// Labels.
    pub labels: Vec<String>,

    /// Custom fields populated on the issue. Non-empty only when the fetch
    /// was made with [`FieldSelection::Named`] or [`FieldSelection::All`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_fields: Vec<JiraCustomField>,
}

/// Selector for which fields to request when fetching a JIRA issue.
///
/// Controls both the `fields` query parameter sent to
/// `GET /rest/api/3/issue/{key}` and which fields end up populated on the
/// returned [`JiraIssue`]. In particular, [`JiraIssue::custom_fields`] is only
/// populated for [`Self::Named`] and [`Self::All`].
#[derive(Debug, Clone, Default)]
pub enum FieldSelection {
    /// Only the standard fields omni-dev tracks (summary, description,
    /// status, issuetype, assignee, priority, labels).
    #[default]
    Standard,

    /// Standard fields plus the named custom fields. Each entry may be a
    /// field ID (e.g., `customfield_19300`) or a human name (e.g.,
    /// `Acceptance Criteria`); the REST API accepts either.
    Named(Vec<String>),

    /// Every field populated on the issue, including all custom fields.
    All,
}

/// A JIRA custom field value keyed by both its stable ID and human name.
///
/// Embedded in [`JiraIssue::custom_fields`]; populated only when the issue was
/// fetched with [`FieldSelection::Named`] or [`FieldSelection::All`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraCustomField {
    /// Field ID (e.g., "customfield_19300"). Stable across renames.
    pub id: String,

    /// Human-readable field name (e.g., "Acceptance Criteria"). Falls back
    /// to `id` when the API did not return `expand=names`.
    pub name: String,

    /// Raw field value as returned by the API (ADF JSON, option object,
    /// scalar, etc.).
    pub value: serde_json::Value,
}

/// Metadata returned by `GET /rest/api/3/issue/{key}/editmeta`.
///
/// Scoped to fields on the issue's screen, so names are unambiguous for a
/// given issue even when multiple custom fields share a display name
/// globally.
#[derive(Debug, Clone, Default)]
pub struct EditMeta {
    /// Field metadata keyed by field ID (e.g., `customfield_19300`).
    pub fields: std::collections::BTreeMap<String, EditMetaField>,
}

/// A single field descriptor from the editmeta response.
#[derive(Debug, Clone)]
pub struct EditMetaField {
    /// Human-readable field name.
    pub name: String,

    /// Schema describing the field's wire type.
    pub schema: EditMetaSchema,

    /// Permitted option `value`s for option-like fields (`select`,
    /// `radiobuttons`, multi-select), taken from the meta `allowedValues`.
    /// Empty when the field does not enumerate values (free text, numbers,
    /// user pickers, cascading selects) — in which case `--set-field` values
    /// pass through for the API to validate. Used to reject an out-of-range
    /// option before the request; see
    /// [`crate::atlassian::custom_fields`].
    pub allowed_values: Vec<String>,
}

/// Schema type information for an editable field.
#[derive(Debug, Clone, Default)]
pub struct EditMetaSchema {
    /// Base type: `string`, `number`, `option`, `array`, `user`, `date`, etc.
    pub kind: String,

    /// For custom fields: the plugin type URI, e.g.
    /// `com.atlassian.jira.plugin.system.customfieldtypes:textarea`.
    pub custom: Option<String>,

    /// For `array` fields: the element type, e.g. `string` (labels),
    /// `option`, `component`, `version`.
    pub items: Option<String>,

    /// For system fields: the canonical system name, e.g. `labels`,
    /// `description`. `None` for custom fields.
    pub system: Option<String>,
}

impl EditMetaField {
    /// Returns `true` when the field carries rich-text (ADF) content on the
    /// wire: textarea custom fields, plus the system `description` and
    /// `environment` fields.
    pub fn is_adf_rich_text(&self) -> bool {
        self.schema.custom.as_deref() == Some(TEXTAREA_CUSTOM_TYPE)
            || matches!(
                self.schema.system.as_deref(),
                Some("description" | "environment")
            )
    }
}

/// Introspection of the create screen for a project + issue type, returned by
/// [`AtlassianClient::get_project_create_meta`](crate::atlassian::client::AtlassianClient::get_project_create_meta).
///
/// Unlike [`EditMeta`] (which keeps only `name` + `schema`), this carries the
/// `required` flag, allowed values, and defaults — collapsing the
/// create→HTTP&nbsp;400→`field list`→`field options` recovery loop into a single
/// pre-flight call.
#[derive(Debug, Clone, Serialize)]
pub struct CreateMeta {
    /// Project key the metadata was requested for (e.g., `PROJ`).
    pub project: String,

    /// Issue type the metadata was requested for (e.g., `Task`).
    pub issue_type: String,

    /// Fields on the create screen, sorted required-first then by name.
    pub fields: Vec<CreateMetaField>,
}

/// A single field on the create screen.
#[derive(Debug, Clone, Serialize)]
pub struct CreateMetaField {
    /// Field ID (e.g., `summary` or `customfield_10001`).
    pub field_id: String,

    /// Human-readable field name.
    pub name: String,

    /// Whether the field must be supplied to create the issue.
    pub required: bool,

    /// Base schema type: `string`, `option`, `array`, `user`, `date`, etc.
    pub schema_type: String,

    /// For `array` fields: the element type (`schema.items`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<String>,

    /// For custom fields: the plugin type URI, e.g.
    /// `com.atlassian.jira.plugin.system.customfieldtypes:select`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,

    /// Allowed values for option/select/cascading-select fields (empty for
    /// free-form fields).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_values: Vec<CreateMetaAllowedValue>,

    /// The field's default value, if the create screen defines one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<serde_json::Value>,
}

/// One allowed value for a field, normalized across the shapes JIRA returns
/// (option `value`, version/component/priority `name`, cascading `children`).
#[derive(Debug, Clone, Serialize)]
pub struct CreateMetaAllowedValue {
    /// Option ID, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Display value: the option's `value`, falling back to its `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    /// Nested options for cascading-select fields.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

/// A JIRA user, returned by `GET /rest/api/3/myself` and embedded in
/// [`JiraWatcherList::watchers`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraUser {
    /// User display name.
    #[serde(rename = "displayName")]
    pub display_name: String,

    /// User email address.
    #[serde(rename = "emailAddress")]
    pub email_address: Option<String>,

    /// Account ID.
    #[serde(rename = "accountId")]
    pub account_id: String,
}

/// Result from `GET /rest/api/3/issue/{key}/watchers`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraWatcherList {
    /// Watchers on the issue.
    pub watchers: Vec<JiraUser>,

    /// Total number of watchers.
    pub watch_count: u32,
}

/// Response from `POST /rest/api/3/issue`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraCreatedIssue {
    /// Issue key (e.g., "PROJ-124").
    pub key: String,
    /// Issue numeric ID.
    pub id: String,
    /// API self URL.
    pub self_url: String,
}

/// Paginated result from a JQL search (`POST /rest/api/3/search/jql`).
///
/// `total` is the aggregate hit count across all pages; `issues` holds the
/// hits returned by the helper (auto-paginated up to the caller's limit).
#[derive(Debug, Clone, Serialize)]
pub struct JiraSearchResult {
    /// Matching issues.
    pub issues: Vec<JiraIssue>,

    /// Total number of matching issues (may exceed `issues.len()` if paginated).
    pub total: u32,
}

/// A single user hit from `GET /rest/api/3/user/search`.
///
/// See [`JiraUserSearchResults`] for the wrapper. JIRA's
/// `/rest/api/3/user/search` endpoint may omit `emailAddress` and
/// `displayName` for tenants where the operating account lacks the
/// privacy-controlled fields permission, so both are optional. `accountId`
/// is the canonical identifier and is always present for atlassian-account
/// users.
#[derive(Debug, Clone, Serialize)]
pub struct JiraUserSearchResult {
    /// Account ID (unique identifier).
    pub account_id: String,
    /// Display name. May be absent on GDPR-redacted tenants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Email address. Often absent due to GDPR / privacy settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,
    /// Whether the account is currently active.
    pub active: bool,
    /// Account type, e.g. `"atlassian"`, `"app"`, `"customer"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_type: Option<String>,
}

/// Wrapper around [`JiraUserSearchResult`] hits from
/// `GET /rest/api/3/user/search`.
///
/// Unlike the JQL search wrappers, the user-search endpoint does not return a
/// total across all pages; `count` is therefore `users.len()`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraUserSearchResults {
    /// Matching users.
    pub users: Vec<JiraUserSearchResult>,
    /// Number of users returned (the JIRA API does not report a true total
    /// across all pages; `count` is `users.len()`).
    pub count: u32,
}

/// A single user resolved by account ID via `GET /rest/api/3/user?accountId=`.
///
/// Unlike the search result, this is the *reverse* direction (ID → record) and
/// is failure-tolerant: on a per-ID lookup failure (unknown / anonymised
/// account, or a non-auth error) all fields except `account_id` are `None` and
/// `error` carries the reason, so a batch lookup never aborts for one bad ID.
/// Deactivated accounts still come back as a real record with `active: false`.
///
/// See [`JiraUserGetResults`] for the batch wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct JiraUserRecord {
    /// Account ID (always present — echoed back even when the lookup failed).
    pub account_id: String,
    /// Display name. Absent on GDPR-redacted tenants or failed lookups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Email address. Often absent due to GDPR / privacy settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_address: Option<String>,
    /// Whether the account is currently active. `None` when the lookup failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    /// Account type, e.g. `"atlassian"`, `"app"`, `"customer"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_type: Option<String>,
    /// Reason this ID could not be resolved (e.g. `"HTTP 404"`). Absent on
    /// success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Batch wrapper around [`JiraUserRecord`] from resolving one or more account
/// IDs via `GET /rest/api/3/user?accountId=`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraUserGetResults {
    /// Resolved users, one per requested account ID (in request order).
    pub users: Vec<JiraUserRecord>,
}

/// A JIRA issue comment returned by `GET /rest/api/3/issue/{key}/comment`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraComment {
    /// Comment ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Comment body as raw ADF JSON.
    pub body_adf: Option<serde_json::Value>,
    /// ISO 8601 creation timestamp.
    pub created: String,
    /// ISO 8601 last-update timestamp, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

/// Visibility restriction kind sent in the body of
/// `POST /rest/api/3/issue/{key}/comment` (and the edit endpoint).
///
/// Note: this is a write-path type — it is *sent* to JIRA when scoping a
/// comment, not parsed from comment-read responses.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JiraVisibilityType {
    /// Restrict to members of a JIRA group.
    Group,
    /// Restrict to holders of a project role.
    Role,
}

/// Visibility restriction applied when posting or editing a JIRA comment.
///
/// Serialised as the `visibility` object on the request body of
/// `POST /rest/api/3/issue/{key}/comment` (and the edit endpoint).
#[derive(Debug, Clone)]
pub struct JiraVisibility {
    /// Whether the restriction targets a group or a project role.
    pub ty: JiraVisibilityType,
    /// Group name or project role name (sent as `identifier`).
    pub value: String,
}

impl Serialize for JiraVisibility {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("JiraVisibility", 2)?;
        s.serialize_field("type", &self.ty)?;
        s.serialize_field("identifier", &self.value)?;
        s.end()
    }
}

/// A JIRA project hit from `GET /rest/api/3/project/search`.
///
/// See [`JiraProjectList`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct JiraProject {
    /// Project ID.
    pub id: String,
    /// Project key (e.g., "PROJ").
    pub key: String,
    /// Project name.
    pub name: String,
    /// Project type key (e.g., "software", "business").
    pub project_type: Option<String>,
    /// Project lead display name.
    pub lead: Option<String>,
}

/// Paginated wrapper around [`JiraProject`] hits from
/// `GET /rest/api/3/project/search`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraProjectList {
    /// Projects returned.
    pub projects: Vec<JiraProject>,
    /// Total number of projects.
    pub total: u32,
}

/// Plugin type URI for the rich-text "textarea" custom field. Used to
/// distinguish ADF-required custom fields from scalar ones.
pub(crate) const TEXTAREA_CUSTOM_TYPE: &str =
    "com.atlassian.jira.plugin.system.customfieldtypes:textarea";

/// A JIRA field definition returned by `GET /rest/api/3/field`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraField {
    /// Field ID (e.g., "summary", "customfield_10001").
    pub id: String,
    /// Human-readable field name.
    pub name: String,
    /// Whether this is a custom field.
    pub custom: bool,
    /// Schema type. Mostly the raw `schema.type` from the API (`"string"`,
    /// `"array"`, `"option"`, ...). For rich-text custom fields this is
    /// `"richtext"`, mapped from `schema.custom` so callers can detect
    /// ADF-required fields without inspecting the plugin URI.
    pub schema_type: Option<String>,
    /// Raw `schema.custom` plugin URI for custom fields, e.g.
    /// `com.atlassian.jira.plugin.system.customfieldtypes:textarea`. Absent
    /// for system fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_custom: Option<String>,
}

/// An option value for a single-select / multi-select JIRA custom field,
/// returned by `GET /rest/api/3/field/{id}/context/{ctxId}/option`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraFieldOption {
    /// Option ID.
    pub id: String,
    /// Option display value.
    pub value: String,
}

/// A JIRA agile board hit from `GET /rest/agile/1.0/board`.
///
/// See [`AgileBoardList`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct AgileBoard {
    /// Board ID.
    pub id: u64,
    /// Board name.
    pub name: String,
    /// Board type (e.g., "scrum", "kanban").
    pub board_type: String,
    /// Project key associated with the board, if available.
    pub project_key: Option<String>,
}

/// Paginated wrapper around [`AgileBoard`] hits from
/// `GET /rest/agile/1.0/board`.
#[derive(Debug, Clone, Serialize)]
pub struct AgileBoardList {
    /// Boards returned.
    pub boards: Vec<AgileBoard>,
    /// Total number of boards.
    pub total: u32,
}

/// A JIRA agile sprint hit from `GET /rest/agile/1.0/board/{boardId}/sprint`.
///
/// See [`AgileSprintList`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct AgileSprint {
    /// Sprint ID.
    pub id: u64,
    /// Sprint name.
    pub name: String,
    /// Sprint state (e.g., "active", "future", "closed").
    pub state: String,
    /// Sprint start date (ISO 8601).
    pub start_date: Option<String>,
    /// Sprint end date (ISO 8601).
    pub end_date: Option<String>,
    /// Sprint goal.
    pub goal: Option<String>,
}

/// Paginated wrapper around [`AgileSprint`] hits from
/// `GET /rest/agile/1.0/board/{boardId}/sprint`.
#[derive(Debug, Clone, Serialize)]
pub struct AgileSprintList {
    /// Sprints returned.
    pub sprints: Vec<AgileSprint>,
    /// Total number of sprints.
    pub total: u32,
}

/// A JIRA project version (release version), returned by
/// `GET /rest/api/3/project/{projectIdOrKey}/version` and created via
/// `POST /rest/api/3/version`.
///
/// See [`JiraProjectVersionList`] for the paginated wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct JiraProjectVersion {
    /// Version ID.
    pub id: String,
    /// Version name (e.g., "1.0.0").
    pub name: String,
    /// Version description.
    pub description: Option<String>,
    /// Owning project key.
    pub project_key: String,
    /// Whether the version is released.
    pub released: bool,
    /// Whether the version is archived.
    pub archived: bool,
    /// Release date (ISO 8601, `YYYY-MM-DD`).
    pub release_date: Option<String>,
    /// Start date (ISO 8601, `YYYY-MM-DD`).
    pub start_date: Option<String>,
}

/// Paginated wrapper around [`JiraProjectVersion`] hits from
/// `GET /rest/api/3/project/{projectIdOrKey}/version`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraProjectVersionList {
    /// Versions returned.
    pub versions: Vec<JiraProjectVersion>,
    /// Total number of versions.
    pub total: u32,
}

/// A JIRA issue changelog entry, returned in the `changelog.histories` array
/// of `GET /rest/api/3/issue/{key}?expand=changelog`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraChangelogEntry {
    /// Entry ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// ISO 8601 timestamp.
    pub created: String,
    /// Changed items.
    pub items: Vec<JiraChangelogItem>,
}

/// A single field change embedded in a [`JiraChangelogEntry`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraChangelogItem {
    /// Field name that changed.
    pub field: String,
    /// Previous value (display string).
    pub from_string: Option<String>,
    /// New value (display string).
    pub to_string: Option<String>,
}

/// A JIRA issue link type returned by `GET /rest/api/3/issueLinkType`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraLinkType {
    /// Link type ID.
    pub id: String,
    /// Link type name (e.g., "Blocks", "Clones").
    pub name: String,
    /// Inward description (e.g., "is blocked by").
    pub inward: String,
    /// Outward description (e.g., "blocks").
    pub outward: String,
}

/// A link on a JIRA issue, as it appears in the `issuelinks` field of
/// `GET /rest/api/3/issue/{key}`. Created via `POST /rest/api/3/issueLink` and
/// removed via `DELETE /rest/api/3/issueLink/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraIssueLink {
    /// Link ID (used for removal).
    pub id: String,
    /// Link type name.
    pub link_type: String,
    /// Direction: "inward" or "outward".
    pub direction: String,
    /// The linked issue key.
    pub linked_issue_key: String,
    /// The linked issue summary.
    pub linked_issue_summary: String,
}

/// A remote (external URL) issue link on a JIRA issue.
///
/// Returned by `GET /rest/api/3/issue/{issueIdOrKey}/remotelink`. These
/// point out to non-JIRA resources (Confluence pages, Bitbucket PRs,
/// external trackers).
#[derive(Debug, Clone, Serialize)]
pub struct JiraRemoteIssueLink {
    /// Remote link ID assigned by JIRA.
    pub id: String,
    /// Application-defined global identifier, when the linking application
    /// supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_id: Option<String>,
    /// Free-form description of how the issue relates to the remote object
    /// (e.g., "mentioned in", "causes").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relationship: Option<String>,
    /// The remote object the link points at.
    pub object: JiraRemoteIssueLinkObject,
}

/// The remote object an entry of [`JiraRemoteIssueLink`] points at.
#[derive(Debug, Clone, Serialize)]
pub struct JiraRemoteIssueLinkObject {
    /// Remote URL.
    pub url: String,
    /// Display title for the remote object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Short summary text for the remote object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Icon associated with the remote object (often labels the kind of
    /// external target, e.g. "Confluence Page").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<JiraRemoteIssueLinkIcon>,
}

/// Icon metadata for a [`JiraRemoteIssueLinkObject`]. Mirrors the upstream
/// `object.icon` shape, with JIRA's `url16x16` flattened to `url`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraRemoteIssueLinkIcon {
    /// Icon URL (from JIRA's `url16x16`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Icon title — typically the label of the external target kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A JIRA issue attachment, embedded in the `attachment` field of
/// `GET /rest/api/3/issue/{key}`.
///
/// Uploaded via `POST /rest/api/3/issue/{key}/attachments` and removed via
/// `DELETE /rest/api/3/attachment/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraAttachment {
    /// Attachment ID.
    pub id: String,
    /// File name.
    pub filename: String,
    /// MIME type (e.g., "image/png", "application/pdf").
    pub mime_type: String,
    /// File size in bytes.
    pub size: u64,
    /// Download URL.
    pub content_url: String,
}

impl From<JiraAttachmentEntry> for JiraAttachment {
    fn from(entry: JiraAttachmentEntry) -> Self {
        Self {
            id: entry.id,
            filename: entry.filename,
            mime_type: entry.mime_type,
            size: entry.size,
            content_url: entry.content,
        }
    }
}

/// A JIRA workflow transition returned by
/// `GET /rest/api/3/issue/{key}/transitions`. Executed via
/// `POST /rest/api/3/issue/{key}/transitions`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraTransition {
    /// Transition ID.
    pub id: String,
    /// Transition name (e.g., "In Progress", "Done").
    pub name: String,
    /// Status the transition moves the issue into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_status: Option<JiraTransitionToStatus>,
    /// Whether executing the transition triggers a screen requiring extra fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_screen: Option<bool>,
}

/// Destination status of a JIRA workflow transition, embedded in
/// [`JiraTransition::to_status`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraTransitionToStatus {
    /// Status ID.
    pub id: String,
    /// Status name (e.g., "In Progress", "Done").
    pub name: String,
    /// Status category key (e.g., "new", "indeterminate", "done").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// A pull request entry from Jira's DevStatus detail endpoint
/// (`GET /rest/dev-status/1.0/issue/detail?issueId={id}&applicationType=…&dataType=pullrequest`).
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevPullRequest {
    /// PR identifier (e.g., "#2174").
    pub id: String,
    /// PR title.
    pub name: String,
    /// Status (e.g., "OPEN", "MERGED", "DECLINED").
    pub status: String,
    /// URL to the pull request.
    pub url: String,
    /// Repository name (e.g., "org/repo").
    pub repository_name: String,
    /// Source branch name.
    pub source_branch: String,
    /// Destination branch name.
    pub destination_branch: String,
    /// PR author name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Reviewer names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    /// Number of comments on the PR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_count: Option<u32>,
    /// Last update timestamp (ISO 8601).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update: Option<String>,
}

/// A commit entry from Jira's DevStatus detail endpoint
/// (`dataType=repository`), embedded in [`JiraDevRepository::commits`] and
/// [`JiraDevBranch::last_commit`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevCommit {
    /// Full commit SHA.
    pub id: String,
    /// Short commit SHA.
    pub display_id: String,
    /// Commit message.
    pub message: String,
    /// Commit author name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Author timestamp (ISO 8601).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// URL to the commit.
    pub url: String,
    /// Number of files changed.
    pub file_count: u32,
    /// Whether this is a merge commit.
    pub merge: bool,
}

/// A branch entry from Jira's DevStatus detail endpoint
/// (`dataType=branch`).
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevBranch {
    /// Branch name.
    pub name: String,
    /// URL to the branch.
    pub url: String,
    /// Repository name (e.g., "org/repo").
    pub repository_name: String,
    /// URL to create a pull request from this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_pr_url: Option<String>,
    /// Most recent commit on this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_commit: Option<JiraDevCommit>,
}

/// A repository entry from Jira's DevStatus detail endpoint
/// (`dataType=repository`).
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevRepository {
    /// Repository name (e.g., "org/repo").
    pub name: String,
    /// URL to the repository.
    pub url: String,
    /// Commits linked to this issue in the repository.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<JiraDevCommit>,
}

/// Aggregated development data for a Jira issue, assembled from the DevStatus
/// detail endpoint (`GET /rest/dev-status/1.0/issue/detail`) across the PR,
/// branch, and repository data types.
///
/// See [`JiraDevStatusSummary`] for the high-level count-only summary.
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevStatus {
    /// Linked pull requests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<JiraDevPullRequest>,
    /// Linked branches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<JiraDevBranch>,
    /// Linked repositories.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<JiraDevRepository>,
}

/// Per-category count from Jira's DevStatus summary endpoint
/// (`GET /rest/dev-status/1.0/issue/summary?issueId={id}`). Embedded in
/// [`JiraDevStatusSummary`].
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevStatusCount {
    /// Number of items.
    pub count: u32,
    /// Providers that have data for this category.
    pub providers: Vec<JiraDevProvider>,
}

/// A development-info provider that has data for a JIRA issue, as reported by
/// the DevStatus summary endpoint's `byInstanceType` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JiraDevProvider {
    /// Instance-type identifier — the value the detail endpoint expects as
    /// `applicationType` (e.g. "stash", "bitbucket", "GitHub"). This is the
    /// round-trip key; do not substitute the display name here.
    pub instance_type: String,
    /// Human-readable display name (e.g. "Bitbucket Server", "GitHub").
    pub name: String,
}

/// High-level dev-status summary from
/// `GET /rest/dev-status/1.0/issue/summary?issueId={id}`. Count-only — use
/// [`JiraDevStatus`] when the individual PRs / branches / repos are needed.
#[derive(Debug, Clone, Serialize)]
pub struct JiraDevStatusSummary {
    /// Pull request summary.
    pub pullrequest: JiraDevStatusCount,
    /// Branch summary.
    pub branch: JiraDevStatusCount,
    /// Repository summary.
    pub repository: JiraDevStatusCount,
}

/// A JIRA issue worklog entry returned by
/// `GET /rest/api/3/issue/{key}/worklog`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraWorklog {
    /// Worklog ID.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Time spent in human-readable format (e.g., "2h 30m").
    pub time_spent: String,
    /// Time spent in seconds.
    pub time_spent_seconds: u64,
    /// ISO 8601 timestamp when the work was started.
    pub started: String,
    /// Comment text (plain text, extracted from ADF).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Paginated wrapper around [`JiraWorklog`] entries from
/// `GET /rest/api/3/issue/{key}/worklog`.
#[derive(Debug, Clone, Serialize)]
pub struct JiraWorklogList {
    /// Worklog entries.
    pub worklogs: Vec<JiraWorklog>,
    /// Total number of worklogs.
    pub total: u32,
}

// ── Internal API response structs ───────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct JiraIssueResponse {
    pub(crate) key: String,
    pub(crate) fields: JiraIssueFields,
}

/// Flexible deserialization target for `GET /rest/api/3/issue/{key}` that
/// retains every field value as raw JSON so custom fields can be extracted.
#[derive(Deserialize)]
pub(crate) struct JiraIssueEnvelope {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) fields: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub(crate) names: std::collections::BTreeMap<String, String>,
}

impl JiraIssueEnvelope {
    pub(crate) fn into_issue(self, selection: &FieldSelection) -> JiraIssue {
        let Self {
            key,
            mut fields,
            names,
        } = self;

        let description_adf = fields.remove("description").filter(|v| !v.is_null());
        let summary = fields
            .remove("summary")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let status = extract_named_field(fields.remove("status"));
        let issue_type = extract_named_field(fields.remove("issuetype"));
        let assignee = extract_display_name(fields.remove("assignee"));
        let priority = extract_named_field(fields.remove("priority"));
        let labels = fields
            .remove("labels")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();

        let collect_customs = !matches!(selection, FieldSelection::Standard);
        let custom_fields = if collect_customs {
            fields
                .into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(id, value)| {
                    let name = names.get(&id).cloned().unwrap_or_else(|| id.clone());
                    JiraCustomField { id, name, value }
                })
                .collect()
        } else {
            Vec::new()
        };

        JiraIssue {
            key,
            summary,
            description_adf,
            status,
            issue_type,
            assignee,
            priority,
            labels,
            custom_fields,
        }
    }
}

fn extract_named_field(value: Option<serde_json::Value>) -> Option<String> {
    value
        .and_then(|v| v.get("name").cloned())
        .and_then(|n| n.as_str().map(str::to_string))
}

fn extract_display_name(value: Option<serde_json::Value>) -> Option<String> {
    value
        .and_then(|v| v.get("displayName").cloned())
        .and_then(|n| n.as_str().map(str::to_string))
}

#[derive(Deserialize)]
pub(crate) struct JiraEditMetaResponse {
    #[serde(default)]
    pub(crate) fields: std::collections::BTreeMap<String, JiraEditMetaField>,
}

#[derive(Deserialize)]
pub(crate) struct JiraEditMetaField {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) schema: Option<JiraEditMetaSchemaRaw>,
    #[serde(rename = "allowedValues", default)]
    pub(crate) allowed_values: Vec<JiraAllowedValueRaw>,
}

impl JiraEditMetaField {
    /// Flattens the raw `allowedValues` to their display `value`s (falling back
    /// to `name`) for option validation. Cascading `children` are ignored —
    /// `--set-field` targets only the top-level option.
    pub(crate) fn allowed_value_strings(&self) -> Vec<String> {
        self.allowed_values
            .iter()
            .filter_map(|v| v.value.clone().or_else(|| v.name.clone()))
            .collect()
    }
}

#[derive(Deserialize)]
pub(crate) struct JiraEditMetaSchemaRaw {
    #[serde(rename = "type", default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) custom: Option<String>,
    #[serde(default)]
    pub(crate) items: Option<String>,
    #[serde(default)]
    pub(crate) system: Option<String>,
}

impl From<Option<JiraEditMetaSchemaRaw>> for EditMetaSchema {
    fn from(raw: Option<JiraEditMetaSchemaRaw>) -> Self {
        raw.map_or_else(Self::default, |s| Self {
            kind: s.kind.unwrap_or_default(),
            custom: s.custom,
            items: s.items,
            system: s.system,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaResponse {
    #[serde(default)]
    pub(crate) projects: Vec<JiraCreateMetaProject>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaProject {
    #[serde(default)]
    pub(crate) issuetypes: Vec<JiraCreateMetaIssueType>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaIssueType {
    #[serde(default)]
    pub(crate) fields: std::collections::BTreeMap<String, JiraEditMetaField>,
}

/// Full createmeta response, parsed for the richer `create-meta` introspection
/// (keeps `required`, `allowedValues`, and `defaultValue`, which the lean
/// `JiraCreateMeta*` path above discards).
#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaFullResponse {
    #[serde(default)]
    pub(crate) projects: Vec<JiraCreateMetaFullProject>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaFullProject {
    #[serde(default)]
    pub(crate) issuetypes: Vec<JiraCreateMetaFullIssueType>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaFullIssueType {
    #[serde(default)]
    pub(crate) fields: std::collections::BTreeMap<String, JiraCreateMetaFieldRaw>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaFieldRaw {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) required: bool,
    #[serde(default)]
    pub(crate) schema: Option<JiraCreateMetaSchemaRaw>,
    #[serde(rename = "allowedValues", default)]
    pub(crate) allowed_values: Vec<JiraAllowedValueRaw>,
    #[serde(rename = "defaultValue", default)]
    pub(crate) default_value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateMetaSchemaRaw {
    #[serde(rename = "type", default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) items: Option<String>,
    #[serde(default)]
    pub(crate) custom: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraAllowedValueRaw {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) value: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) children: Vec<Self>,
}

impl JiraAllowedValueRaw {
    /// Normalizes one raw allowed value: display value is `value` falling back
    /// to `name`; children recurse.
    pub(crate) fn into_allowed_value(self) -> CreateMetaAllowedValue {
        CreateMetaAllowedValue {
            id: self.id,
            value: self.value.or(self.name),
            children: self
                .children
                .into_iter()
                .map(Self::into_allowed_value)
                .collect(),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueFields {
    pub(crate) summary: Option<String>,
    pub(crate) description: Option<serde_json::Value>,
    pub(crate) status: Option<JiraNameField>,
    pub(crate) issuetype: Option<JiraNameField>,
    pub(crate) assignee: Option<JiraAssigneeField>,
    pub(crate) priority: Option<JiraNameField>,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraNameField {
    pub(crate) name: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraAssigneeField {
    #[serde(rename = "displayName")]
    pub(crate) display_name: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct JiraSearchResponse {
    pub(crate) issues: Vec<JiraIssueResponse>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "nextPageToken", default)]
    pub(crate) next_page_token: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraTransitionsResponse {
    pub(crate) transitions: Vec<JiraTransitionEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraTransitionEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) to: Option<JiraTransitionToEntry>,
    #[serde(rename = "hasScreen", default)]
    pub(crate) has_screen: Option<bool>,
    /// Transition-screen field metadata, populated only when the request adds
    /// `expand=transitions.fields`. Same shape as the editmeta fields map, so it
    /// reuses [`JiraEditMetaField`]. Empty for the lean list path.
    #[serde(default)]
    pub(crate) fields: std::collections::BTreeMap<String, JiraEditMetaField>,
}

#[derive(Deserialize)]
pub(crate) struct JiraTransitionToEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(rename = "statusCategory", default)]
    pub(crate) status_category: Option<JiraStatusCategoryEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraStatusCategoryEntry {
    #[serde(default)]
    pub(crate) key: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCommentsResponse {
    #[serde(default)]
    pub(crate) comments: Vec<JiraCommentEntry>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "startAt", default)]
    pub(crate) start_at: u32,
    #[serde(rename = "maxResults", default)]
    #[allow(dead_code)]
    pub(crate) max_results: u32,
}

#[derive(Deserialize)]
pub(crate) struct JiraCommentEntry {
    pub(crate) id: String,
    pub(crate) author: Option<JiraCommentAuthor>,
    pub(crate) body: Option<serde_json::Value>,
    pub(crate) created: Option<String>,
    #[serde(default)]
    pub(crate) updated: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCommentAuthor {
    #[serde(rename = "displayName")]
    pub(crate) display_name: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraWorklogResponse {
    #[serde(default)]
    pub(crate) worklogs: Vec<JiraWorklogEntry>,
    #[serde(default)]
    pub(crate) total: u32,
}

#[derive(Deserialize)]
pub(crate) struct JiraWorklogEntry {
    pub(crate) id: String,
    pub(crate) author: Option<JiraCommentAuthor>,
    #[serde(rename = "timeSpent")]
    pub(crate) time_spent: Option<String>,
    #[serde(rename = "timeSpentSeconds", default)]
    pub(crate) time_spent_seconds: u64,
    pub(crate) started: Option<String>,
    pub(crate) comment: Option<serde_json::Value>,
}

// ── JIRA user search API response struct ──────────────────────────

#[derive(Deserialize)]
pub(crate) struct JiraUserSearchEntry {
    #[serde(rename = "accountId")]
    pub(crate) account_id: String,
    #[serde(rename = "displayName", default)]
    pub(crate) display_name: Option<String>,
    #[serde(rename = "emailAddress", default)]
    pub(crate) email_address: Option<String>,
    #[serde(default)]
    pub(crate) active: bool,
    #[serde(rename = "accountType", default)]
    pub(crate) account_type: Option<String>,
}

// ── Agile API response structs ─────────────────────────────────────

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct AgileBoardListResponse {
    pub(crate) values: Vec<AgileBoardEntry>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "isLast", default)]
    pub(crate) is_last: bool,
}

#[derive(Deserialize)]
pub(crate) struct AgileBoardEntry {
    pub(crate) id: u64,
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) board_type: String,
    pub(crate) location: Option<AgileBoardLocation>,
}

#[derive(Deserialize)]
pub(crate) struct AgileBoardLocation {
    #[serde(rename = "projectKey")]
    pub(crate) project_key: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct AgileIssueListResponse {
    pub(crate) issues: Vec<JiraIssueResponse>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "isLast", default)]
    pub(crate) is_last: bool,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct AgileSprintListResponse {
    pub(crate) values: Vec<AgileSprintEntry>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "isLast", default)]
    pub(crate) is_last: bool,
}

#[derive(Deserialize)]
pub(crate) struct AgileSprintEntry {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) state: String,
    #[serde(rename = "startDate")]
    pub(crate) start_date: Option<String>,
    #[serde(rename = "endDate")]
    pub(crate) end_date: Option<String>,
    pub(crate) goal: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraProjectVersionEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) released: bool,
    #[serde(default)]
    pub(crate) archived: bool,
    #[serde(rename = "releaseDate", default)]
    pub(crate) release_date: Option<String>,
    #[serde(rename = "startDate", default)]
    pub(crate) start_date: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinksResponse {
    pub(crate) fields: JiraIssueLinksFields,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinksFields {
    #[serde(default)]
    pub(crate) issuelinks: Vec<JiraIssueLinkEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinkEntry {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) link_type: JiraIssueLinkType,
    #[serde(rename = "inwardIssue")]
    pub(crate) inward_issue: Option<JiraIssueLinkIssue>,
    #[serde(rename = "outwardIssue")]
    pub(crate) outward_issue: Option<JiraIssueLinkIssue>,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinkType {
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinkIssue {
    pub(crate) key: String,
    pub(crate) fields: Option<JiraIssueLinkIssueFields>,
}

#[derive(Deserialize)]
pub(crate) struct JiraIssueLinkIssueFields {
    pub(crate) summary: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraRemoteIssueLinkEntry {
    pub(crate) id: serde_json::Value,
    #[serde(rename = "globalId", default)]
    pub(crate) global_id: Option<String>,
    #[serde(default)]
    pub(crate) relationship: Option<String>,
    pub(crate) object: JiraRemoteIssueLinkObjectEntry,
}

#[derive(Deserialize)]
pub(crate) struct JiraRemoteIssueLinkObjectEntry {
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) icon: Option<JiraRemoteIssueLinkIconEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraRemoteIssueLinkIconEntry {
    #[serde(rename = "url16x16", default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraLinkTypesResponse {
    #[serde(rename = "issueLinkTypes")]
    pub(crate) issue_link_types: Vec<JiraLinkTypeEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraLinkTypeEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) inward: String,
    pub(crate) outward: String,
}

#[derive(Deserialize)]
pub(crate) struct JiraAttachmentIssueResponse {
    pub(crate) fields: JiraAttachmentFields,
}

#[derive(Deserialize)]
pub(crate) struct JiraAttachmentFields {
    #[serde(default)]
    pub(crate) attachment: Vec<JiraAttachmentEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraAttachmentEntry {
    pub(crate) id: String,
    pub(crate) filename: String,
    #[serde(rename = "mimeType")]
    pub(crate) mime_type: String,
    pub(crate) size: u64,
    pub(crate) content: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct JiraChangelogResponse {
    pub(crate) values: Vec<JiraChangelogEntryResponse>,
    #[serde(default)]
    pub(crate) total: u32,
    #[serde(rename = "isLast", default)]
    pub(crate) is_last: bool,
}

#[derive(Deserialize)]
pub(crate) struct JiraChangelogEntryResponse {
    pub(crate) id: String,
    pub(crate) author: Option<JiraCommentAuthor>,
    pub(crate) created: Option<String>,
    #[serde(default)]
    pub(crate) items: Vec<JiraChangelogItemResponse>,
}

#[derive(Deserialize)]
pub(crate) struct JiraChangelogItemResponse {
    pub(crate) field: String,
    #[serde(rename = "fromString")]
    pub(crate) from_string: Option<String>,
    #[serde(rename = "toString")]
    pub(crate) to_string: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) custom: bool,
    pub(crate) schema: Option<JiraFieldSchema>,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldSchema {
    #[serde(rename = "type")]
    pub(crate) schema_type: Option<String>,
    pub(crate) custom: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldContextsResponse {
    pub(crate) values: Vec<JiraFieldContextEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldContextEntry {
    pub(crate) id: String,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldOptionsResponse {
    pub(crate) values: Vec<JiraFieldOptionEntry>,
}

#[derive(Deserialize)]
pub(crate) struct JiraFieldOptionEntry {
    pub(crate) id: String,
    pub(crate) value: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct JiraProjectSearchResponse {
    pub(crate) values: Vec<JiraProjectEntry>,
    pub(crate) total: u32,
    #[serde(rename = "isLast", default)]
    pub(crate) is_last: bool,
}

#[derive(Deserialize)]
pub(crate) struct JiraProjectEntry {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) name: String,
    #[serde(rename = "projectTypeKey")]
    pub(crate) project_type_key: Option<String>,
    pub(crate) lead: Option<JiraProjectLead>,
}

#[derive(Deserialize)]
pub(crate) struct JiraProjectLead {
    #[serde(rename = "displayName")]
    pub(crate) display_name: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct JiraCreateResponse {
    pub(crate) key: String,
    pub(crate) id: String,
    #[serde(rename = "self")]
    pub(crate) self_url: String,
}

// ── DevStatus API response structs ─────────────────────────────────

/// Minimal response for resolving an issue key to its numeric ID.
#[derive(Deserialize)]
pub(crate) struct JiraIssueIdResponse {
    pub(crate) id: String,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusResponse {
    #[serde(default)]
    pub(crate) detail: Vec<DevStatusDetail>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusDetail {
    #[serde(rename = "pullRequests", default)]
    pub(crate) pull_requests: Vec<DevStatusPullRequest>,
    #[serde(default)]
    pub(crate) branches: Vec<DevStatusBranch>,
    #[serde(default)]
    pub(crate) repositories: Vec<DevStatusRepositoryEntry>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusPullRequest {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(rename = "repositoryName", default)]
    pub(crate) repository_name: String,
    #[serde(default)]
    pub(crate) source: Option<DevStatusBranchRef>,
    #[serde(default)]
    pub(crate) destination: Option<DevStatusBranchRef>,
    #[serde(default)]
    pub(crate) author: Option<DevStatusAuthor>,
    #[serde(default)]
    pub(crate) reviewers: Vec<DevStatusReviewer>,
    #[serde(rename = "commentCount", default)]
    pub(crate) comment_count: Option<u32>,
    #[serde(rename = "lastUpdate", default)]
    pub(crate) last_update: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusBranchRef {
    #[serde(default)]
    pub(crate) branch: String,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusAuthor {
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusReviewer {
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusCommit {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(rename = "displayId", default)]
    pub(crate) display_id: String,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) author: Option<DevStatusAuthor>,
    #[serde(rename = "authorTimestamp", default)]
    pub(crate) author_timestamp: Option<String>,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(rename = "fileCount", default)]
    pub(crate) file_count: u32,
    #[serde(default)]
    pub(crate) merge: bool,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusBranch {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(rename = "repositoryName", default)]
    pub(crate) repository_name: String,
    #[serde(rename = "createPullRequestUrl", default)]
    pub(crate) create_pr_url: Option<String>,
    #[serde(rename = "lastCommit", default)]
    pub(crate) last_commit: Option<DevStatusCommit>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusRepositoryEntry {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) commits: Vec<DevStatusCommit>,
}

// ── DevStatus summary response structs ────────────────────────────

#[derive(Deserialize)]
pub(crate) struct DevStatusSummaryResponse {
    #[serde(default)]
    pub(crate) summary: DevStatusSummaryData,
}

#[derive(Deserialize, Default)]
pub(crate) struct DevStatusSummaryData {
    #[serde(default)]
    pub(crate) pullrequest: Option<DevStatusSummaryCategory>,
    #[serde(default)]
    pub(crate) branch: Option<DevStatusSummaryCategory>,
    #[serde(default)]
    pub(crate) repository: Option<DevStatusSummaryCategory>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusSummaryCategory {
    pub(crate) overall: Option<DevStatusSummaryOverall>,
    // Keyed by instance-type identifier (e.g. "github", "stash"); only the
    // keys are needed for provider discovery, so the values are ignored.
    #[serde(rename = "byInstanceType", default)]
    pub(crate) by_instance_type: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct DevStatusSummaryOverall {
    #[serde(default)]
    pub(crate) count: u32,
}
