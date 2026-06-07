//! Drift detection between the local `CONTENT_ENTRIES` snapshot and
//! upstream `@atlaskit/adf-schema`.
//!
//! Issue [#731]: a scheduled CI job downloads the latest upstream tarball,
//! parses `dist/json-schema/v1/full.json` into a per-parent allowed-children
//! map, and diffs it against the locally-encoded snapshot. The output is
//! consumed by `bin/adf-schema-drift` and the `.github/workflows/
//! adf-schema-drift.yml` workflow.
//!
//! The parser is intentionally narrow: it understands only the subset of
//! JSON-schema patterns the upstream artefact actually uses (`anyOf` of
//! `$ref` items, with optional alias definitions whose own subtree contains
//! more refs). Any layout change upstream surfaces as a structured error or
//! visible drift, not as a silent empty diff.
//!
//! [#731]: https://github.com/rust-works/omni-dev/issues/731

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Read;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{local_schema_map, SCHEMA_VERSION, UPSTREAM_TARBALL_SHA256};

/// npm registry endpoint that resolves the `latest` dist-tag for the package.
const NPM_LATEST_URL: &str = "https://registry.npmjs.org/@atlaskit/adf-schema/latest";

/// Optional env-var override for `NPM_LATEST_URL`.
///
/// Honoured by [`fetch_latest_drift_report`]. Used by integration tests to
/// point the binary at a `wiremock` server, and available as an operational
/// knob for teams running an npm mirror.
pub const NPM_LATEST_URL_ENV: &str = "OMNI_DEV_ADF_SCHEMA_LATEST_URL";

/// Per-parent drift: children that upstream now lists but the local snapshot
/// does not (`added_children`), and children the local snapshot lists but
/// upstream no longer does (`removed_children`).
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ParentDrift {
    /// Children listed by upstream that the local snapshot does not list.
    pub added_children: BTreeSet<String>,
    /// Children listed by the local snapshot that upstream no longer lists.
    pub removed_children: BTreeSet<String>,
}

/// Result of a drift comparison between upstream and the locally-encoded
/// schema.
///
/// `version_changed` is true if the upstream npm version differs from the
/// version embedded in [`SCHEMA_VERSION`] (after stripping the
/// `-YYYY-MM-DD` transcription-date suffix). `per_parent` lists only parents
/// that have content-model drift; parents in sync are omitted to keep the
/// rendered report tight.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DriftReport {
    /// `version` field from the latest upstream `package.json` (npm).
    pub upstream_version: String,
    /// SHA-256 of the upstream tarball bytes we downloaded.
    pub upstream_tarball_sha256: String,
    /// Local `SCHEMA_VERSION` (npm version + transcription date).
    pub local_version: String,
    /// Local `UPSTREAM_TARBALL_SHA256`.
    pub local_tarball_sha256: String,
    /// True iff `upstream_version` differs from the npm-version prefix of
    /// `local_version`.
    pub version_changed: bool,
    /// Parents present on both sides whose allowed-children sets differ.
    /// Parents fully in sync are omitted.
    pub per_parent: BTreeMap<String, ParentDrift>,
    /// Parents listed by upstream that the local snapshot does not have.
    pub added_parents: BTreeSet<String>,
    /// Parents listed by the local snapshot that upstream no longer has.
    pub removed_parents: BTreeSet<String>,
}

impl DriftReport {
    /// True iff any per-parent diff or any added/removed parent was found.
    #[must_use]
    pub fn has_content_drift(&self) -> bool {
        !self.per_parent.is_empty()
            || !self.added_parents.is_empty()
            || !self.removed_parents.is_empty()
    }

    /// True iff the upstream version differs OR any content-model drift was
    /// found. The CI workflow uses this to decide whether to open or update
    /// a tracking issue.
    #[must_use]
    pub fn has_any_drift(&self) -> bool {
        self.version_changed || self.has_content_drift()
    }

    /// Render a markdown body suitable for `gh issue create --body-file`.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# ADF schema drift report\n\n");

        out.push_str("## Version\n\n");
        out.push_str(&format!(
            "- Upstream `@atlaskit/adf-schema`: `{}`\n",
            self.upstream_version
        ));
        out.push_str(&format!(
            "- Upstream tarball SHA-256: `{}`\n",
            self.upstream_tarball_sha256
        ));
        out.push_str(&format!(
            "- Local `SCHEMA_VERSION`: `{}`\n",
            self.local_version
        ));
        out.push_str(&format!(
            "- Local `UPSTREAM_TARBALL_SHA256`: `{}`\n",
            self.local_tarball_sha256
        ));
        out.push_str(&format!(
            "- Version changed: **{}**\n\n",
            self.version_changed
        ));

        out.push_str("## Content-model drift\n\n");
        if !self.has_content_drift() {
            out.push_str("No content-model changes — version bump only.\n\n");
        } else {
            if !self.added_parents.is_empty() {
                out.push_str("### New parents (upstream only)\n\n");
                for p in &self.added_parents {
                    out.push_str(&format!("- `{p}`\n"));
                }
                out.push('\n');
            }
            if !self.removed_parents.is_empty() {
                out.push_str("### Removed parents (local only)\n\n");
                for p in &self.removed_parents {
                    out.push_str(&format!("- `{p}`\n"));
                }
                out.push('\n');
            }
            if !self.per_parent.is_empty() {
                out.push_str("### Per-parent diffs\n\n");
                for (parent, drift) in &self.per_parent {
                    out.push_str(&format!("#### `{parent}`\n\n"));
                    if !drift.added_children.is_empty() {
                        out.push_str("Added children (upstream only):\n");
                        for c in &drift.added_children {
                            out.push_str(&format!("- `{c}`\n"));
                        }
                        out.push('\n');
                    }
                    if !drift.removed_children.is_empty() {
                        out.push_str("Removed children (local only):\n");
                        for c in &drift.removed_children {
                            out.push_str(&format!("- `{c}`\n"));
                        }
                        out.push('\n');
                    }
                }
            }
        }

        out.push_str("---\n");
        out.push_str(
            "_Generated by the `adf-schema-drift` job. To refresh the snapshot, \
             update `CONTENT_ENTRIES`, `SCHEMA_VERSION`, and `UPSTREAM_TARBALL_SHA256` in \
             `src/atlassian/adf_schema/mod.rs`._\n",
        );
        out
    }

    /// Render the same report as a JSON object, for machine-readable CI use.
    #[must_use]
    pub fn render_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Fetch the latest upstream tarball, parse its `full.json`, and compute a
/// drift report against the locally-encoded schema.
///
/// Honours the [`NPM_LATEST_URL_ENV`] environment variable as an override
/// for the registry URL — useful for integration tests and for teams behind
/// an npm mirror.
pub async fn fetch_latest_drift_report() -> Result<DriftReport> {
    let url = std::env::var(NPM_LATEST_URL_ENV).unwrap_or_else(|_| NPM_LATEST_URL.to_string());
    fetch_drift_report_from_url(&url).await
}

/// Variant of [`fetch_latest_drift_report`] that takes a configurable
/// `latest`-dist-tag URL. Tests use this with a `wiremock` server; production
/// always uses [`NPM_LATEST_URL`].
async fn fetch_drift_report_from_url(latest_url: &str) -> Result<DriftReport> {
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "omni-dev-adf-schema-drift/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("building HTTP client")?;

    let meta: Value = client
        .get(latest_url)
        .send()
        .await
        .context("fetching npm registry latest dist-tag")?
        .error_for_status()
        .context("npm registry returned a non-2xx status for latest dist-tag")?
        .json()
        .await
        .context("parsing npm latest dist-tag JSON")?;

    let upstream_version = meta
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("npm latest dist-tag JSON has no `version` field"))?
        .to_string();
    let tarball_url = meta
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("npm latest dist-tag JSON has no `dist.tarball` field"))?
        .to_string();

    let tarball_bytes = client
        .get(&tarball_url)
        .send()
        .await
        .with_context(|| format!("fetching tarball {tarball_url}"))?
        .error_for_status()
        .with_context(|| format!("npm tarball {tarball_url} returned a non-2xx status"))?
        .bytes()
        .await
        .context("reading tarball bytes")?;

    let upstream_sha = hex_encode(&Sha256::digest(&tarball_bytes));
    let full_json = extract_full_json_from_tarball(&tarball_bytes)
        .context("extracting dist/json-schema/v1/full.json from tarball")?;

    diff_against_upstream_json_schema(&full_json, &upstream_version, &upstream_sha)
}

/// Parse the upstream `full.json` and diff against the local snapshot.
pub fn diff_against_upstream_json_schema(
    full: &Value,
    upstream_version: &str,
    upstream_sha256: &str,
) -> Result<DriftReport> {
    let upstream = parse_upstream_full_json(full)?;
    let local = local_schema_map();

    let local_version_npm = strip_transcription_date(SCHEMA_VERSION);
    let version_changed = upstream_version != local_version_npm;

    let upstream_parents: BTreeSet<&str> = upstream.keys().map(String::as_str).collect();
    let local_parents: BTreeSet<&str> = local.keys().copied().collect();

    let added_parents: BTreeSet<String> = upstream_parents
        .difference(&local_parents)
        .map(|s| (*s).to_string())
        .collect();
    let removed_parents: BTreeSet<String> = local_parents
        .difference(&upstream_parents)
        .map(|s| (*s).to_string())
        .collect();

    let mut per_parent: BTreeMap<String, ParentDrift> = BTreeMap::new();
    for parent in upstream_parents.intersection(&local_parents).copied() {
        let upstream_children: &BTreeSet<String> = upstream
            .get(parent)
            .ok_or_else(|| anyhow!("internal: parent `{parent}` missing from upstream map"))?;
        let local_children: &BTreeSet<&'static str> = local
            .get(parent)
            .ok_or_else(|| anyhow!("internal: parent `{parent}` missing from local map"))?;
        let added_children: BTreeSet<String> = upstream_children
            .iter()
            .filter(|c| !local_children.contains(c.as_str()))
            .cloned()
            .collect();
        let removed_children: BTreeSet<String> = local_children
            .iter()
            .filter(|c| !upstream_children.contains(**c))
            .map(|s| (*s).to_string())
            .collect();
        if !added_children.is_empty() || !removed_children.is_empty() {
            per_parent.insert(
                parent.to_string(),
                ParentDrift {
                    added_children,
                    removed_children,
                },
            );
        }
    }

    Ok(DriftReport {
        upstream_version: upstream_version.to_string(),
        upstream_tarball_sha256: upstream_sha256.to_string(),
        local_version: SCHEMA_VERSION.to_string(),
        local_tarball_sha256: UPSTREAM_TARBALL_SHA256.to_string(),
        version_changed,
        per_parent,
        added_parents,
        removed_parents,
    })
}

// -----------------------------------------------------------------------------
// JSON-schema parsing
// -----------------------------------------------------------------------------

/// Parse `full.json`'s `definitions` into a per-parent allowed-children map.
///
/// The shape we accept:
///
/// - A "bare-type" definition has `properties.type.const` (or `enum`)
///   directly readable, e.g. `paragraph_node` → bare type `paragraph`.
/// - A "marks-overlay" definition uses `allOf [base_ref, marks_extension]`
///   to add a marks shape to an existing node (e.g.
///   `formatted_text_inline_node` overlays marks on `text_node`). It
///   inherits its base's bare type via a fixed-point pass.
/// - Allowed children are the bare types reachable from any
///   `properties.content` subtree (including ones nested under `allOf` to
///   handle defs like `mediaSingle_caption_node`), with alias definitions
///   flattened transitively.
///
/// Exposed `pub` so the `adf-schema-codegen` binary (issue #732) can parse
/// the vendored `assets/adf-schema/full.json` without copy-pasting this
/// logic.
pub fn parse_upstream_full_json(full: &Value) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let definitions = full
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("upstream JSON has no `definitions` object"))?;

    // Pass 1: direct bare types (anything with `properties.type.const|enum`
    // readable without crossing a `$ref`).
    let mut def_to_bare: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (name, def) in definitions {
        def_to_bare.insert(name.clone(), find_bare_type(def));
    }

    // Pass 2: fixed-point inheritance. `allOf [{$ref: X}, ...]` inherits
    // X's bare type, when X has one. Repeat until no change so chains of
    // length >1 (`A inherits from B inherits from C`) converge.
    loop {
        let mut changed = false;
        for (name, def) in definitions {
            if def_to_bare
                .get(name)
                .is_some_and(std::option::Option::is_some)
            {
                continue;
            }
            if let Some(inherited) = inherited_bare_type_via_allof(def, &def_to_bare) {
                def_to_bare.insert(name.clone(), Some(inherited));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut result: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, def) in definitions {
        let Some(Some(bare)) = def_to_bare.get(name) else {
            continue;
        };
        let children = definition_content_children(name, def, definitions, &def_to_bare);
        if !children.is_empty() {
            result.entry(bare.clone()).or_default().extend(children);
        }
    }

    Ok(result)
}

/// Look for an inherited bare type via an `allOf` whose items include a
/// `$ref` to a definition whose own bare type is known.
///
/// Only `allOf` is followed: it represents "I am all of these"
/// (`base + extension`), so the base's identity is the natural inheritance
/// path. `anyOf` / `oneOf` are unions of options, NOT inheritance — collapsing
/// them to a single bare type would silently drop the other options' types.
fn inherited_bare_type_via_allof(
    def: &Value,
    def_to_bare: &BTreeMap<String, Option<String>>,
) -> Option<String> {
    let Value::Object(obj) = def else { return None };
    let Some(Value::Array(arr)) = obj.get("allOf") else {
        return None;
    };
    for item in arr {
        let Some(s) = item.get("$ref").and_then(Value::as_str) else {
            continue;
        };
        let Some(target) = s.strip_prefix("#/definitions/") else {
            continue;
        };
        if let Some(Some(bare)) = def_to_bare.get(target) {
            return Some(bare.clone());
        }
    }
    None
}

/// Find the bare node type for a definition, if it has one.
///
/// Looks at `properties.type.const` and `properties.type.enum[0]`, recursing
/// through `allOf` / `anyOf` / `oneOf` arrays.
fn find_bare_type(def: &Value) -> Option<String> {
    fn walk(v: &Value) -> Option<String> {
        let Value::Object(obj) = v else { return None };
        if let Some(Value::Object(props)) = obj.get("properties") {
            if let Some(Value::Object(t)) = props.get("type") {
                if let Some(Value::String(s)) = t.get("const") {
                    return Some(s.clone());
                }
                if let Some(Value::Array(arr)) = t.get("enum") {
                    if let Some(Value::String(s)) = arr.first() {
                        return Some(s.clone());
                    }
                }
            }
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(Value::Array(arr)) = obj.get(key) {
                for x in arr {
                    if let Some(s) = walk(x) {
                        return Some(s);
                    }
                }
            }
        }
        None
    }
    walk(def)
}

/// Collect bare-type children reachable from any `properties.content`
/// subtree of `def`, including ones nested under `allOf` / `oneOf` / `anyOf`.
fn definition_content_children(
    def_name: &str,
    def: &Value,
    definitions: &Map<String, Value>,
    def_to_bare: &BTreeMap<String, Option<String>>,
) -> BTreeSet<String> {
    let mut subtrees: Vec<&Value> = Vec::new();
    find_content_subtrees(def, &mut subtrees);

    let mut refs = Vec::new();
    for subtree in subtrees {
        collect_refs(subtree, &mut refs);
    }

    let mut out = BTreeSet::new();
    for r in refs {
        let mut visited = HashSet::new();
        visited.insert(def_name.to_string());
        resolve_ref_to_bare_types(&r, definitions, def_to_bare, &mut visited, &mut out);
    }
    out
}

/// Walk a definition tree gathering every `properties.content` subtree.
///
/// Descends through `allOf` / `oneOf` / `anyOf` siblings (so a marks-overlay
/// def whose content lives inside an `allOf` extension is still picked up)
/// but does NOT descend into the values of `properties.*` itself, which
/// keeps marks/attrs subtrees out of the content-ref search.
fn find_content_subtrees<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    let Value::Object(obj) = value else { return };
    if let Some(Value::Object(props)) = obj.get("properties") {
        if let Some(content) = props.get("content") {
            out.push(content);
        }
    }
    for key in ["allOf", "oneOf", "anyOf"] {
        if let Some(Value::Array(arr)) = obj.get(key) {
            for item in arr {
                find_content_subtrees(item, out);
            }
        }
    }
}

/// Resolve a `$ref` target to its bare type(s), flattening alias chains.
///
/// Bare-type targets always emit their bare type, including for legitimate
/// self-references (e.g. `taskList` listing `taskList` as an allowed child) —
/// reaching the same bare-type def twice is not recursion, just convergence.
/// Only alias-only chains need the `visited` set to prevent infinite loops.
fn resolve_ref_to_bare_types(
    target_def_name: &str,
    definitions: &Map<String, Value>,
    def_to_bare: &BTreeMap<String, Option<String>>,
    visited: &mut HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    if let Some(Some(bare)) = def_to_bare.get(target_def_name) {
        out.insert(bare.clone());
        return;
    }
    if !visited.insert(target_def_name.to_string()) {
        return;
    }
    let Some(target_def) = definitions.get(target_def_name) else {
        return;
    };
    let mut refs = Vec::new();
    collect_refs(target_def, &mut refs);
    for r in refs {
        resolve_ref_to_bare_types(&r, definitions, def_to_bare, visited, out);
    }
}

/// Recursively collect every `#/definitions/<name>` reference under `node`.
///
/// Skips subtrees keyed `marks` or `attrs`: those describe mark/attribute
/// schemas, not content, and their refs (e.g. to `link_mark` whose bare type
/// would resolve to `"link"`) would otherwise leak in as false-positive
/// content children when walking an alias definition.
fn collect_refs(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get("$ref") {
                if let Some(name) = r.strip_prefix("#/definitions/") {
                    out.push(name.to_string());
                }
            }
            for (key, v) in obj {
                if key == "marks" || key == "attrs" {
                    continue;
                }
                collect_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, out);
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------------
// Tarball extraction
// -----------------------------------------------------------------------------

fn extract_full_json_from_tarball(bytes: &[u8]) -> Result<Value> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("opening tarball entries")? {
        let mut entry = entry.context("reading tarball entry header")?;
        let path_buf = entry
            .path()
            .context("decoding tarball entry path")?
            .into_owned();
        if path_buf == std::path::Path::new("package/dist/json-schema/v1/full.json") {
            let mut buf = String::new();
            entry
                .read_to_string(&mut buf)
                .context("reading full.json")?;
            return serde_json::from_str(&buf).context("parsing full.json");
        }
    }
    Err(anyhow!(
        "tarball does not contain package/dist/json-schema/v1/full.json"
    ))
}

// -----------------------------------------------------------------------------
// Version-string handling
// -----------------------------------------------------------------------------

/// Strip the trailing `-YYYY-MM-DD` transcription-date suffix from a
/// `SCHEMA_VERSION`-style string, leaving the npm version.
fn strip_transcription_date(s: &str) -> &str {
    if s.len() < 11 {
        return s;
    }
    let (head, tail) = s.split_at(s.len() - 11);
    let Some(rest) = tail.strip_prefix('-') else {
        return s;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let looks_like_date = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()));
    if looks_like_date {
        head
    } else {
        s
    }
}

/// Lower-case hex encoding of a byte slice.
///
/// Replaces the `format!("{:x}", Sha256::digest(...))` idiom, which broke when
/// `sha2` 0.11 changed the digest output type to `hybrid_array::Array`, which
/// does not implement `LowerHex`.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn synthesise_full_json_from_local() -> Value {
        let local = local_schema_map();
        let parents: BTreeSet<&str> = local.keys().copied().collect();
        let mut all_types: BTreeSet<&str> = parents.clone();
        for children in local.values() {
            for c in children {
                all_types.insert(*c);
            }
        }
        let leaves: BTreeSet<&str> = all_types.difference(&parents).copied().collect();

        let mut definitions = serde_json::Map::new();
        for (parent, children) in &local {
            let any_of: Vec<Value> = children
                .iter()
                .map(|c| json!({"$ref": format!("#/definitions/{c}_node")}))
                .collect();
            definitions.insert(
                format!("{parent}_node"),
                json!({
                    "properties": {
                        "type": {"const": parent},
                        "content": {
                            "type": "array",
                            "items": {"anyOf": any_of}
                        }
                    }
                }),
            );
        }
        for leaf in &leaves {
            definitions.insert(
                format!("{leaf}_node"),
                json!({
                    "properties": {
                        "type": {"const": leaf}
                    }
                }),
            );
        }
        json!({"definitions": Value::Object(definitions)})
    }

    #[test]
    fn parses_anyof_refs_into_allowed_children_set() {
        let full = json!({
            "definitions": {
                "blockquote_node": {
                    "properties": {
                        "type": {"const": "blockquote"},
                        "content": {
                            "type": "array",
                            "items": {
                                "anyOf": [
                                    {"$ref": "#/definitions/paragraph_node"},
                                    {"$ref": "#/definitions/codeBlock_node"}
                                ]
                            }
                        }
                    }
                },
                "paragraph_node": {"properties": {"type": {"const": "paragraph"}}},
                "codeBlock_node": {"properties": {"type": {"const": "codeBlock"}}}
            }
        });
        let parsed = parse_upstream_full_json(&full).unwrap();
        let bq = parsed.get("blockquote").expect("blockquote parsed");
        let expected: BTreeSet<String> = ["codeBlock", "paragraph"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(*bq, expected);
    }

    #[test]
    fn alias_definition_is_flattened_transitively() {
        // tableCell-style: bare def points at an alias; alias resolves to bare types.
        let full = json!({
            "definitions": {
                "tableCell_node": {
                    "properties": {
                        "type": {"const": "tableCell"},
                        "content": {
                            "type": "array",
                            "items": {"$ref": "#/definitions/table_cell_content"}
                        }
                    }
                },
                "table_cell_content": {
                    "anyOf": [
                        {"$ref": "#/definitions/paragraph_node"},
                        {"$ref": "#/definitions/heading_node"}
                    ]
                },
                "paragraph_node": {"properties": {"type": {"const": "paragraph"}}},
                "heading_node": {"properties": {"type": {"const": "heading"}}}
            }
        });
        let parsed = parse_upstream_full_json(&full).unwrap();
        let cell = parsed.get("tableCell").expect("tableCell parsed");
        let expected: BTreeSet<String> = ["heading", "paragraph"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(*cell, expected);
    }

    #[test]
    fn report_is_empty_when_input_matches_local_entries() {
        // Round-trip: synthesise a JSON-schema fixture from the local table,
        // feed it through the parser, and assert no drift. This is a stronger
        // version of the existing `blockquote_allowed_children_match_upstream_
        // json_schema` test: it covers every parent in the table at once.
        let full = synthesise_full_json_from_local();
        let report = diff_against_upstream_json_schema(
            &full,
            strip_transcription_date(SCHEMA_VERSION),
            "fixture",
        )
        .unwrap();
        assert!(
            !report.has_content_drift(),
            "synthesised-from-local fixture should produce no content drift, got: {report:#?}"
        );
        assert!(report.added_parents.is_empty());
        assert!(report.removed_parents.is_empty());
        assert!(report.per_parent.is_empty());
        assert!(!report.version_changed);
    }

    #[test]
    fn report_flags_added_and_removed_children() {
        let mut full = synthesise_full_json_from_local();
        // Add a child to blockquote that the local table doesn't list.
        let bq_items = full
            .pointer_mut("/definitions/blockquote_node/properties/content/items/anyOf")
            .unwrap()
            .as_array_mut()
            .unwrap();
        bq_items.push(json!({"$ref": "#/definitions/madeUpBlock_node"}));
        full.pointer_mut("/definitions")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "madeUpBlock_node".to_string(),
                json!({"properties": {"type": {"const": "madeUpBlock"}}}),
            );
        // Remove a child from panel.
        let panel_items = full
            .pointer_mut("/definitions/panel_node/properties/content/items/anyOf")
            .unwrap()
            .as_array_mut()
            .unwrap();
        panel_items.retain(|v| {
            v.get("$ref").and_then(Value::as_str) != Some("#/definitions/paragraph_node")
        });

        let report =
            diff_against_upstream_json_schema(&full, "fixture-version", "fixture").unwrap();
        assert!(report.has_content_drift());

        let bq = report
            .per_parent
            .get("blockquote")
            .expect("blockquote drift present");
        assert_eq!(
            bq.added_children,
            std::iter::once("madeUpBlock").map(String::from).collect()
        );
        assert!(bq.removed_children.is_empty());

        let panel = report.per_parent.get("panel").expect("panel drift present");
        assert!(panel.added_children.is_empty());
        assert_eq!(
            panel.removed_children,
            std::iter::once("paragraph").map(String::from).collect()
        );
    }

    #[test]
    fn report_flags_added_and_removed_parents() {
        let mut full = synthesise_full_json_from_local();
        // Remove a parent definition (`expand`) from upstream entirely.
        full.pointer_mut("/definitions")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("expand_node");
        // Add a parent definition that the local table doesn't have.
        full.pointer_mut("/definitions")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(
                "futureBlock_node".to_string(),
                json!({
                    "properties": {
                        "type": {"const": "futureBlock"},
                        "content": {
                            "type": "array",
                            "items": {
                                "anyOf": [
                                    {"$ref": "#/definitions/paragraph_node"}
                                ]
                            }
                        }
                    }
                }),
            );

        let report = diff_against_upstream_json_schema(&full, "fixture", "fixture").unwrap();
        assert!(report.added_parents.contains("futureBlock"));
        assert!(report.removed_parents.contains("expand"));
    }

    #[test]
    fn version_changed_distinguishes_npm_version_from_transcription_date() {
        let full = synthesise_full_json_from_local();
        // Same as local: not changed.
        let r = diff_against_upstream_json_schema(
            &full,
            strip_transcription_date(SCHEMA_VERSION),
            "fixture",
        )
        .unwrap();
        assert!(!r.version_changed);

        // Different: changed.
        let r = diff_against_upstream_json_schema(&full, "999.0.0", "fixture").unwrap();
        assert!(r.version_changed);
    }

    #[test]
    fn strip_transcription_date_handles_yyyy_mm_dd_suffix() {
        assert_eq!(strip_transcription_date("52.9.5-2026-05-10"), "52.9.5");
        assert_eq!(strip_transcription_date("52.9.5"), "52.9.5");
        assert_eq!(strip_transcription_date("52.9.5-rc1"), "52.9.5-rc1");
        assert_eq!(
            strip_transcription_date("52.9.5-rc1-2026-05-10"),
            "52.9.5-rc1"
        );
        assert_eq!(strip_transcription_date(""), "");
    }

    #[test]
    fn strip_transcription_date_returns_input_when_suffix_lacks_leading_dash() {
        // 12+ chars but no `-` at the suffix-start position: returns input.
        assert_eq!(strip_transcription_date("abcdefghijkl"), "abcdefghijkl");
    }

    #[test]
    fn strip_transcription_date_returns_input_when_suffix_is_not_a_date() {
        // 14 chars; tail = "-1234-XX-YY", parts non-numeric → returns input.
        assert_eq!(strip_transcription_date("abc-1234-XX-YY"), "abc-1234-XX-YY");
    }

    #[test]
    fn render_markdown_is_terse_when_no_drift() {
        let full = synthesise_full_json_from_local();
        let report = diff_against_upstream_json_schema(
            &full,
            strip_transcription_date(SCHEMA_VERSION),
            "fixture",
        )
        .unwrap();
        let md = report.render_markdown();
        assert!(md.contains("No content-model changes"));
        assert!(!md.contains("Per-parent diffs"));
    }

    #[test]
    fn render_markdown_includes_per_parent_diffs() {
        let mut full = synthesise_full_json_from_local();
        let bq_items = full
            .pointer_mut("/definitions/blockquote_node/properties/content/items/anyOf")
            .unwrap()
            .as_array_mut()
            .unwrap();
        bq_items.push(json!({"$ref": "#/definitions/text_node"}));
        let report = diff_against_upstream_json_schema(&full, "fixture", "fixture").unwrap();
        let md = report.render_markdown();
        assert!(md.contains("Per-parent diffs"));
        assert!(md.contains("`blockquote`"));
        assert!(md.contains("text"));
    }

    #[test]
    fn render_json_is_serializable() {
        let full = synthesise_full_json_from_local();
        let report = diff_against_upstream_json_schema(&full, "fixture", "fixture").unwrap();
        let v = report.render_json();
        assert!(v.is_object());
        assert!(v.get("upstream_version").is_some());
        assert!(v.get("per_parent").is_some());
    }

    // ---- find_bare_type variants -----------------------------------------

    #[test]
    fn find_bare_type_recognises_enum_array() {
        // Some upstream defs use `"enum": ["nodeName"]` instead of
        // `"const": "nodeName"` — both must resolve to the bare type.
        let def = json!({"properties": {"type": {"enum": ["paragraph"]}}});
        assert_eq!(find_bare_type(&def).as_deref(), Some("paragraph"));
    }

    #[test]
    fn find_bare_type_walks_into_oneof_for_nested_const() {
        let def = json!({
            "oneOf": [
                {"properties": {"type": {"const": "nestedExpand"}}},
                {"properties": {"type": {"const": "ignoredVariant"}}}
            ]
        });
        // First match wins.
        assert_eq!(find_bare_type(&def).as_deref(), Some("nestedExpand"));
    }

    #[test]
    fn find_bare_type_returns_none_for_nodes_with_no_type_const() {
        let def = json!({"anyOf": [{"$ref": "#/definitions/x"}]});
        assert_eq!(find_bare_type(&def), None);
    }

    #[test]
    fn find_bare_type_returns_none_when_enum_is_empty_or_non_string() {
        // Empty enum array: the `first()` Option is None.
        let empty = json!({"properties": {"type": {"enum": []}}});
        assert_eq!(find_bare_type(&empty), None);
        // Enum with a non-string head element: the Value::String pattern fails.
        let non_string = json!({"properties": {"type": {"enum": [42]}}});
        assert_eq!(find_bare_type(&non_string), None);
    }

    #[test]
    fn find_bare_type_returns_none_on_non_object() {
        assert_eq!(find_bare_type(&json!(null)), None);
        assert_eq!(find_bare_type(&json!([])), None);
        assert_eq!(find_bare_type(&json!("string")), None);
    }

    // ---- inherited_bare_type_via_allof -----------------------------------

    #[test]
    fn inheritance_via_allof_finds_known_base() {
        let def = json!({
            "allOf": [
                {"$ref": "#/definitions/paragraph_node"},
                {"properties": {"marks": {}}}
            ]
        });
        let mut def_to_bare = BTreeMap::new();
        def_to_bare.insert("paragraph_node".to_string(), Some("paragraph".to_string()));
        assert_eq!(
            inherited_bare_type_via_allof(&def, &def_to_bare).as_deref(),
            Some("paragraph")
        );
    }

    #[test]
    fn inheritance_via_allof_returns_none_when_no_allof() {
        let def = json!({"anyOf": [{"$ref": "#/definitions/x_node"}]});
        let mut def_to_bare = BTreeMap::new();
        def_to_bare.insert("x_node".to_string(), Some("x".to_string()));
        // anyOf is intentionally NOT followed for inheritance.
        assert_eq!(inherited_bare_type_via_allof(&def, &def_to_bare), None);
    }

    #[test]
    fn inheritance_via_allof_returns_none_when_target_unknown() {
        let def = json!({"allOf": [{"$ref": "#/definitions/unknown_node"}]});
        let def_to_bare: BTreeMap<String, Option<String>> = BTreeMap::new();
        assert_eq!(inherited_bare_type_via_allof(&def, &def_to_bare), None);
    }

    #[test]
    fn inheritance_via_allof_skips_items_without_ref_or_with_external_ref() {
        // Items inside `allOf` may be plain objects (no `$ref`) — skip.
        // Items with `$ref` not pointing into `#/definitions/` — also skip.
        let def = json!({
            "allOf": [
                {"properties": {"marks": {}}},                  // no $ref
                {"$ref": "https://example.com/external.json"},   // external ref
                {"$ref": "#/definitions/paragraph_node"}         // valid; should be picked
            ]
        });
        let mut def_to_bare = BTreeMap::new();
        def_to_bare.insert("paragraph_node".to_string(), Some("paragraph".to_string()));
        assert_eq!(
            inherited_bare_type_via_allof(&def, &def_to_bare).as_deref(),
            Some("paragraph")
        );
    }

    #[test]
    fn inheritance_via_allof_returns_none_when_only_non_ref_items() {
        // No item carries a usable `$ref` — function returns None.
        let def = json!({
            "allOf": [
                {"properties": {"marks": {}}},
                {"$ref": "https://example.com/external.json"}
            ]
        });
        let def_to_bare: BTreeMap<String, Option<String>> = BTreeMap::new();
        assert_eq!(inherited_bare_type_via_allof(&def, &def_to_bare), None);
    }

    #[test]
    fn inheritance_via_allof_returns_none_for_non_object_input() {
        // Defensive `let Value::Object(obj) = def else { return None }`.
        let def_to_bare: BTreeMap<String, Option<String>> = BTreeMap::new();
        assert_eq!(
            inherited_bare_type_via_allof(&json!(null), &def_to_bare),
            None
        );
        assert_eq!(
            inherited_bare_type_via_allof(&json!([]), &def_to_bare),
            None
        );
        assert_eq!(
            inherited_bare_type_via_allof(&json!("string"), &def_to_bare),
            None
        );
    }

    #[test]
    fn inheritance_via_allof_handles_marks_overlay_pattern() {
        // Real-world pattern: formatted_text_inline_node.
        let full = json!({
            "definitions": {
                "paragraph_node": {
                    "properties": {
                        "type": {"const": "paragraph"},
                        "content": {
                            "type": "array",
                            "items": {"$ref": "#/definitions/formatted_text_inline_node"}
                        }
                    }
                },
                "text_node": {"properties": {"type": {"const": "text"}}},
                "formatted_text_inline_node": {
                    "allOf": [
                        {"$ref": "#/definitions/text_node"},
                        {
                            "properties": {
                                "marks": {
                                    "type": "array",
                                    "items": {
                                        "anyOf": [
                                            {"$ref": "#/definitions/link_mark"}
                                        ]
                                    }
                                }
                            }
                        }
                    ]
                },
                "link_mark": {"properties": {"type": {"const": "link"}}}
            }
        });
        let parsed = parse_upstream_full_json(&full).unwrap();
        // paragraph's children must be exactly {text}; the link mark must
        // NOT leak in via the marks subtree.
        let p = parsed.get("paragraph").expect("paragraph present");
        let expected: BTreeSet<String> = std::iter::once("text").map(String::from).collect();
        assert_eq!(*p, expected);
    }

    // ---- parse error paths -----------------------------------------------

    #[test]
    fn parse_returns_error_when_definitions_missing() {
        let full = json!({"foo": "bar"});
        let err = parse_upstream_full_json(&full).unwrap_err();
        assert!(err.to_string().contains("definitions"));
    }

    #[test]
    fn diff_propagates_parse_error_when_definitions_missing() {
        let err = diff_against_upstream_json_schema(&json!({}), "1.0.0", "sha").unwrap_err();
        assert!(err.to_string().contains("definitions"));
    }

    // ---- collect_refs / find_content_subtrees corner cases ---------------

    #[test]
    fn collect_refs_skips_marks_and_attrs_subtrees() {
        let v = json!({
            "properties": {
                "content": {"$ref": "#/definitions/keep_me"},
                "marks": {"$ref": "#/definitions/skip_me_mark"},
                "attrs": {"$ref": "#/definitions/skip_me_attrs"}
            }
        });
        let mut refs = Vec::new();
        collect_refs(&v, &mut refs);
        assert!(refs.contains(&"keep_me".to_string()));
        assert!(!refs.contains(&"skip_me_mark".to_string()));
        assert!(!refs.contains(&"skip_me_attrs".to_string()));
    }

    #[test]
    fn collect_refs_handles_arrays_of_refs() {
        let v = json!([
            {"$ref": "#/definitions/a"},
            {"$ref": "#/definitions/b"},
            {"$ref": "https://example.com/schema"}, // non-#/definitions, ignored
        ]);
        let mut refs = Vec::new();
        collect_refs(&v, &mut refs);
        assert_eq!(refs, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn find_content_subtrees_returns_nothing_for_non_object_input() {
        // Defensive `let Value::Object(obj) = value else { return }`.
        let null = json!(null);
        let array = json!([]);
        let string = json!("string");
        let mut subtrees = Vec::new();
        find_content_subtrees(&null, &mut subtrees);
        find_content_subtrees(&array, &mut subtrees);
        find_content_subtrees(&string, &mut subtrees);
        assert!(subtrees.is_empty());
    }

    #[test]
    fn find_content_subtrees_picks_up_content_nested_in_allof() {
        // mediaSingle_caption_node-style: bare type comes via $ref, content
        // sits inside an `allOf` extension.
        let def = json!({
            "allOf": [
                {"$ref": "#/definitions/mediaSingle_node"},
                {
                    "properties": {
                        "content": {
                            "type": "array",
                            "items": [
                                {"$ref": "#/definitions/media_node"},
                                {"$ref": "#/definitions/caption_node"}
                            ]
                        }
                    }
                }
            ]
        });
        let mut subtrees = Vec::new();
        find_content_subtrees(&def, &mut subtrees);
        assert_eq!(subtrees.len(), 1);
    }

    // ---- resolve cycle protection ----------------------------------------

    #[test]
    fn resolve_handles_alias_cycles_without_infinite_loop() {
        let mut definitions = serde_json::Map::new();
        definitions.insert(
            "alias_a".to_string(),
            json!({"anyOf": [{"$ref": "#/definitions/alias_b"}]}),
        );
        definitions.insert(
            "alias_b".to_string(),
            json!({"anyOf": [{"$ref": "#/definitions/alias_a"}]}),
        );
        let mut def_to_bare = BTreeMap::new();
        def_to_bare.insert("alias_a".to_string(), None);
        def_to_bare.insert("alias_b".to_string(), None);

        let mut visited = HashSet::new();
        let mut out = BTreeSet::new();
        // Should terminate (no infinite recursion).
        resolve_ref_to_bare_types(
            "alias_a",
            &definitions,
            &def_to_bare,
            &mut visited,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn resolve_returns_silently_when_target_not_in_definitions() {
        let definitions = serde_json::Map::new();
        let mut def_to_bare = BTreeMap::new();
        def_to_bare.insert("ghost".to_string(), None);
        let mut visited = HashSet::new();
        let mut out = BTreeSet::new();
        resolve_ref_to_bare_types("ghost", &definitions, &def_to_bare, &mut visited, &mut out);
        assert!(out.is_empty());
    }

    // ---- markdown rendering branches -------------------------------------

    fn report_with(
        added_parents: BTreeSet<String>,
        removed_parents: BTreeSet<String>,
        per_parent: BTreeMap<String, ParentDrift>,
        version_changed: bool,
    ) -> DriftReport {
        DriftReport {
            upstream_version: "9.9.9".to_string(),
            upstream_tarball_sha256: "up-sha".to_string(),
            local_version: "1.0.0-2026-01-01".to_string(),
            local_tarball_sha256: "local-sha".to_string(),
            version_changed,
            per_parent,
            added_parents,
            removed_parents,
        }
    }

    #[test]
    fn render_markdown_renders_added_parents_section() {
        let added: BTreeSet<String> = std::iter::once("futureNode").map(String::from).collect();
        let report = report_with(added, BTreeSet::new(), BTreeMap::new(), true);
        let md = report.render_markdown();
        assert!(md.contains("New parents (upstream only)"));
        assert!(md.contains("`futureNode`"));
        assert!(!md.contains("Removed parents"));
    }

    #[test]
    fn render_markdown_renders_removed_parents_section() {
        let removed: BTreeSet<String> = std::iter::once("oldNode").map(String::from).collect();
        let report = report_with(BTreeSet::new(), removed, BTreeMap::new(), false);
        let md = report.render_markdown();
        assert!(md.contains("Removed parents (local only)"));
        assert!(md.contains("`oldNode`"));
        assert!(!md.contains("New parents"));
    }

    #[test]
    fn render_markdown_renders_only_added_children_when_no_removed() {
        let mut per = BTreeMap::new();
        per.insert(
            "blockquote".to_string(),
            ParentDrift {
                added_children: std::iter::once("newChild").map(String::from).collect(),
                removed_children: BTreeSet::new(),
            },
        );
        let report = report_with(BTreeSet::new(), BTreeSet::new(), per, false);
        let md = report.render_markdown();
        assert!(md.contains("Added children"));
        assert!(!md.contains("Removed children"));
        assert!(md.contains("`newChild`"));
    }

    #[test]
    fn render_markdown_renders_only_removed_children_when_no_added() {
        let mut per = BTreeMap::new();
        per.insert(
            "panel".to_string(),
            ParentDrift {
                added_children: BTreeSet::new(),
                removed_children: std::iter::once("goneChild").map(String::from).collect(),
            },
        );
        let report = report_with(BTreeSet::new(), BTreeSet::new(), per, false);
        let md = report.render_markdown();
        assert!(!md.contains("Added children"));
        assert!(md.contains("Removed children"));
        assert!(md.contains("`goneChild`"));
    }

    // ---- has_any_drift / has_content_drift -------------------------------

    #[test]
    fn has_any_drift_true_when_only_version_changed() {
        let report = report_with(BTreeSet::new(), BTreeSet::new(), BTreeMap::new(), true);
        assert!(!report.has_content_drift());
        assert!(report.has_any_drift());
    }

    #[test]
    fn has_any_drift_true_when_only_added_parents() {
        let added: BTreeSet<String> = std::iter::once("x").map(String::from).collect();
        let report = report_with(added, BTreeSet::new(), BTreeMap::new(), false);
        assert!(report.has_content_drift());
        assert!(report.has_any_drift());
    }

    #[test]
    fn has_any_drift_true_when_only_removed_parents() {
        let removed: BTreeSet<String> = std::iter::once("x").map(String::from).collect();
        let report = report_with(BTreeSet::new(), removed, BTreeMap::new(), false);
        assert!(report.has_content_drift());
        assert!(report.has_any_drift());
    }

    // ---- tarball extraction ----------------------------------------------

    fn build_synthetic_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut gz);
            for (path, body) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(body.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, *body).unwrap();
            }
            builder.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    #[test]
    fn extract_full_json_succeeds_when_path_present() {
        let body = serde_json::to_vec(&json!({"definitions": {}})).unwrap();
        let bytes =
            build_synthetic_tarball(&[("package/dist/json-schema/v1/full.json", body.as_slice())]);
        let parsed = extract_full_json_from_tarball(&bytes).unwrap();
        assert!(parsed.get("definitions").is_some());
    }

    #[test]
    fn extract_full_json_errors_when_path_missing() {
        let bytes = build_synthetic_tarball(&[("package/README.md", b"hello")]);
        let err = extract_full_json_from_tarball(&bytes).unwrap_err();
        assert!(err.to_string().contains("does not contain"));
    }

    #[test]
    fn extract_full_json_errors_on_invalid_gzip() {
        let bytes = b"not a gzip stream";
        let err = extract_full_json_from_tarball(bytes).unwrap_err();
        // Some error from flate2/tar surfaces as the cause; we only assert
        // an error is returned.
        let _ = err;
    }

    #[test]
    fn extract_full_json_errors_when_payload_is_not_json() {
        let bytes =
            build_synthetic_tarball(&[("package/dist/json-schema/v1/full.json", b"not json{")]);
        let err = extract_full_json_from_tarball(&bytes).unwrap_err();
        assert!(err.to_string().contains("parsing full.json"));
    }

    // ---- end-to-end fetch via wiremock -----------------------------------

    #[tokio::test]
    async fn fetch_drift_report_from_url_handles_clean_upstream() {
        let server = wiremock::MockServer::start().await;
        let full = synthesise_full_json_from_local();
        let tarball = build_synthetic_tarball(&[(
            "package/dist/json-schema/v1/full.json",
            serde_json::to_vec(&full).unwrap().as_slice(),
        )]);
        let tarball_url = format!("{}/-/adf-schema-fixture.tgz", server.uri());

        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "version": strip_transcription_date(SCHEMA_VERSION),
                "dist": {"tarball": tarball_url}
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/-/adf-schema-fixture.tgz"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(tarball))
            .mount(&server)
            .await;

        let report = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap();
        assert!(!report.version_changed);
        assert!(!report.has_content_drift());
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_when_metadata_lacks_version() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({"dist": {"tarball": "x"}})),
            )
            .mount(&server)
            .await;
        let err = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`version` field"));
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_when_metadata_lacks_tarball() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(json!({"version": "1.0.0"})),
            )
            .mount(&server)
            .await;
        let err = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`dist.tarball` field"));
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_when_metadata_is_not_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("not json{"),
            )
            .mount(&server)
            .await;
        let err = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parsing npm latest dist-tag JSON"));
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_on_connection_refused() {
        // Hold the listener for the duration of the test so a parallel
        // wiremock server can't reclaim the port between bind and connect
        // (the bind-then-drop race in issue #861). The accept loop closes
        // each connection immediately, so reqwest's `send().await` fails
        // before any HTTP response can arrive — exercising the
        // `fetching npm registry` context wrapper.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move {
            if let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let url = format!("http://127.0.0.1:{port}/latest");
        let err = fetch_drift_report_from_url(&url).await.unwrap_err();
        let _ = accept_task.await;
        assert!(
            err.to_string().contains("fetching npm registry"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_on_npm_5xx() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("non-2xx status for latest dist-tag"));
    }

    #[tokio::test]
    async fn fetch_drift_report_from_url_errors_on_tarball_5xx() {
        let server = wiremock::MockServer::start().await;
        let tarball_url = format!("{}/-/x.tgz", server.uri());
        wiremock::Mock::given(wiremock::matchers::path("/latest"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "version": "1.0.0",
                "dist": {"tarball": tarball_url}
            })))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/-/x.tgz"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = fetch_drift_report_from_url(&format!("{}/latest", server.uri()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("non-2xx status"));
    }
}
