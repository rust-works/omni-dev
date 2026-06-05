//! Best-effort harvester for the signed-in user's **own** Facebook timeline.
//!
//! This encapsulates the manual three-step recipe (issue #922): harvest session
//! tokens from the `/me` shell, discover the pagination persisted-query `doc_id`
//! from a cross-origin script bundle, then replay the refetch GraphQL query,
//! feeding `end_cursor` back until the timeline is exhausted. Every request goes
//! through the running bridge via [`BridgeClient`], so it borrows the tab's
//! authenticated session without exfiltrating cookies.
//!
//! **Best-effort contract.** Facebook's `doc_id`s, relay-provider flags, page
//! structure, and response shape are undocumented and rotate frequently. This
//! code re-harvests every volatile value per run (never hardcoded) and reports a
//! staged, actionable error — naming the step and what it expected — when the
//! structure drifts, rather than panicking. The stable alternative is Facebook's
//! official "Download Your Information" export.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::client::BridgeClient;
use crate::browser::protocol::{ControlRequest, ResponseEnvelope};

/// The cross-origin host the timeline script bundles are served from.
const FBCDN_HOST: &str = "https://static.xx.fbcdn.net";
/// Friendly name of the initial (first-page) timeline query.
const INIT_FRIENDLY: &str = "ProfileCometTimelineFeedQuery";
/// Friendly name of the cursor-driven refetch (pagination) query.
const REFETCH_FRIENDLY: &str = "ProfileCometTimelineFeedRefetchQuery";
/// Per-request retry budget for GraphQL pages (504s / transient drops).
const PAGE_ATTEMPTS: u32 = 4;
/// Safety cap on refetch pages so a non-advancing cursor can never loop forever.
const MAX_PAGES: u32 = 5000;

/// Output serialisation for harvested posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One compact JSON object per line (streamed; append-friendly on resume).
    Jsonl,
    /// A single pretty-printed JSON array, written once at completion.
    Json,
}

/// Where harvested posts are written.
#[derive(Debug, Clone)]
pub enum Output {
    /// Standard output.
    Stdout,
    /// A file path (created/truncated for `json`, appended for `jsonl` resume).
    File(PathBuf),
}

/// Everything the harvest run needs, assembled by the CLI layer.
#[derive(Debug, Clone)]
pub struct HarvestConfig {
    /// Control-plane port of the running bridge.
    pub control_port: u16,
    /// Bridge session token (already resolved from file/env).
    pub token: String,
    /// Tab routing selector (connection id or origin); required for multi-tab.
    pub target: Option<String>,
    /// Where to write posts.
    pub output: Output,
    /// Output serialisation.
    pub format: Format,
    /// Stop paging once a post is older than this Unix timestamp (seconds).
    pub since: Option<i64>,
    /// Stop after emitting this many fresh posts.
    pub limit: Option<usize>,
    /// Path to a resume state file (last cursor + token snapshot).
    pub resume: Option<PathBuf>,
}

/// One harvested timeline post. Fields are best-effort and may be absent when
/// Facebook's response shape drifts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Post {
    /// Story node id (used for dedup).
    pub id: Option<String>,
    /// Post creation time, Unix seconds.
    pub creation_time: Option<i64>,
    /// Post text/message.
    pub text: Option<String>,
    /// Canonical permalink (`wwwURL`).
    pub url: Option<String>,
    /// Shared external link, when the post attaches one.
    pub shared_link: Option<String>,
}

/// Session values harvested from the `/me` shell plus the discovered refetch
/// `doc_id`. All are volatile and re-harvested every run.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    fb_dtsg: String,
    lsd: String,
    user_id: String,
    /// `doc_id` of the initial timeline query (from the `/me` preloader).
    init_doc: String,
    /// `doc_id` of the refetch query (from a cross-origin bundle).
    refetch_doc: String,
    /// Initial query `variables` (carries the relay-provider flags reused by the
    /// refetch query).
    base_vars: Map<String, Value>,
}

/// Persisted resume state. `end_cursor` is the load-bearing field; the token
/// snapshot is informational (tokens are re-harvested on resume regardless).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ResumeState {
    /// Last good `end_cursor` to continue paging from.
    end_cursor: Option<String>,
    /// Number of posts emitted so far (across runs).
    count: usize,
    /// Snapshot of the account this state belongs to.
    user_id: Option<String>,
}

impl ResumeState {
    /// Loads resume state from `path`, returning the default when the file is
    /// absent (first run with a fresh `--resume` path).
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("Failed to parse resume state at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => {
                Err(e).with_context(|| format!("Failed to read resume state at {}", path.display()))
            }
        }
    }

    /// Atomically writes resume state to `path` (write-temp-then-rename).
    fn save(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialise resume state")?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)
            .with_context(|| format!("Failed to write resume state to {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("Failed to finalise resume state at {}", path.display()))
    }
}

// ───────────────────────────── pure extraction ─────────────────────────────

/// One step of a JSON traversal path: a map key or an array index.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Object key.
    Key(&'static str),
    /// Array index.
    Idx(usize),
}

/// Walks `value` along `path`, returning the addressed node when every step
/// resolves (the Rust analogue of the reference `get(o, *path)` helper).
fn dig<'a>(value: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut cur = value;
    for step in path {
        cur = match step {
            Step::Key(k) => cur.get(k)?,
            Step::Idx(i) => cur.get(i)?,
        };
    }
    Some(cur)
}

/// Reads a node along `path` as an owned string when it is a JSON string.
fn dig_str(value: &Value, path: &[Step]) -> Option<String> {
    dig(value, path)?.as_str().map(ToOwned::to_owned)
}

/// Extracts a [`Post`] from a story `node`, mirroring the reference field map.
fn extract_post(node: &Value) -> Option<Post> {
    if !node.is_object() {
        return None;
    }
    use Step::{Idx, Key};
    let story = [Key("comet_sections"), Key("content"), Key("story")];
    let text = {
        let mut p = story.to_vec();
        p.extend([
            Key("comet_sections"),
            Key("message"),
            Key("story"),
            Key("message"),
            Key("text"),
        ]);
        dig_str(node, &p)
    };
    let url = {
        let mut p = story.to_vec();
        p.push(Key("wwwURL"));
        dig_str(node, &p)
    };
    let creation_time = dig(
        node,
        &[
            Key("comet_sections"),
            Key("context_layout"),
            Key("story"),
            Key("comet_sections"),
            Key("metadata"),
            Idx(0),
            Key("story"),
            Key("creation_time"),
        ],
    )
    .and_then(Value::as_i64);
    let shared_link = dig_str(
        node,
        &[
            Key("attachments"),
            Idx(0),
            Key("styles"),
            Key("attachment"),
            Key("story_attachment_link_renderer"),
            Key("attachment"),
            Key("web_link"),
            Key("url"),
        ],
    );
    let id = dig_str(node, &[Key("id")]);
    Some(Post {
        id,
        creation_time,
        text,
        url,
        shared_link,
    })
}

/// One page parsed from a stream/defer GraphQL response.
#[derive(Debug, Default)]
struct Page {
    /// Posts found on this page, in document order.
    posts: Vec<Post>,
    /// `end_cursor` from the deferred `page_info`, when present.
    end_cursor: Option<String>,
    /// `has_next_page` from the deferred `page_info`, when present.
    has_next_page: Option<bool>,
}

/// Parses a stream/defer JSONL response body into posts + pagination info.
///
/// Tolerates the three line shapes the reference handles: a leading `edges`
/// array, streamed single-edge `{node, cursor}` objects, and a deferred
/// `page_info`. Initial-query responses nest under `user`; refetch responses
/// nest under `node`.
fn parse_stream(body: &str) -> Page {
    use Step::Key;
    let mut page = Page::default();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(data) = doc.get("data") else {
            continue;
        };

        let edges = dig(
            data,
            &[Key("user"), Key("timeline_list_feed_units"), Key("edges")],
        )
        .or_else(|| {
            dig(
                data,
                &[Key("node"), Key("timeline_list_feed_units"), Key("edges")],
            )
        });
        if let Some(Value::Array(edges)) = edges {
            for edge in edges {
                if let Some(node) = edge.get("node") {
                    if let Some(post) = extract_post(node) {
                        page.posts.push(post);
                    }
                }
            }
        }

        // Streamed single edge: `data == {node, cursor}`.
        if data.get("node").is_some() && data.get("cursor").is_some() {
            if let Some(node) = data.get("node") {
                if let Some(post) = extract_post(node) {
                    page.posts.push(post);
                }
            }
        }

        let page_info = dig(data, &[Key("page_info")])
            .or_else(|| {
                dig(
                    data,
                    &[
                        Key("user"),
                        Key("timeline_list_feed_units"),
                        Key("page_info"),
                    ],
                )
            })
            .or_else(|| {
                dig(
                    data,
                    &[
                        Key("node"),
                        Key("timeline_list_feed_units"),
                        Key("page_info"),
                    ],
                )
            });
        if let Some(pi) = page_info {
            if let Some(c) = pi.get("end_cursor").and_then(Value::as_str) {
                page.end_cursor = Some(c.to_owned());
            }
            if let Some(b) = pi.get("has_next_page").and_then(Value::as_bool) {
                page.has_next_page = Some(b);
            }
        }
    }
    page
}

/// Returns the substring of `haystack` immediately following the first balanced
/// `{...}` object that starts at or after `from`, along with that object text.
/// Used to slice an embedded JSON object out of a script/HTML blob.
fn balanced_object(haystack: &str, from: usize) -> Option<&str> {
    let bytes = haystack.as_bytes();
    let start = haystack[from..].find('{')? + from;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&haystack[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// First capture group of `pattern` in `text`, compiled fresh (patterns are
/// constructed from trusted literals).
fn first_capture(pattern: &str, text: &str) -> Result<Option<String>> {
    let re = Regex::new(pattern).with_context(|| format!("invalid regex: {pattern}"))?;
    Ok(re
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned()))
}

/// Harvests session tokens and the initial query's `doc_id` + `variables` from
/// the `/me` HTML shell. (Step 1 of the recipe.)
fn parse_session_from_me(html: &str) -> Result<PartialSession> {
    let step = "step 1 (harvest tokens)";
    let fb_dtsg = first_capture(r#""DTSGInitialData",\[\],\{"token":"([^"]+)""#, html)?
        .with_context(|| format!("{step}: `fb_dtsg` (DTSGInitialData token) not found in /me — Facebook page structure may have changed"))?;
    let lsd = first_capture(r#""LSD",\[\],\{"token":"([^"]+)""#, html)?
        .with_context(|| format!("{step}: `lsd` (LSD token) not found in /me"))?;
    let user_id = first_capture(r#""USER_ID":"(\d+)""#, html)?
        .with_context(|| format!("{step}: `USER_ID` not found in /me"))?;

    // The initial timeline query's variables (with relay-provider flags) and its
    // doc_id live near the `ProfileCometTimelineFeedQuery` preloader entry.
    let anchor = html
        .find(INIT_FRIENDLY)
        .with_context(|| format!("{step}: `{INIT_FRIENDLY}` preloader block not found in /me"))?;
    let vars_at = html[anchor..]
        .find("\"variables\"")
        .map(|rel| anchor + rel)
        .with_context(|| {
            format!("{step}: `variables` block for `{INIT_FRIENDLY}` not found in /me")
        })?;
    let vars_text = balanced_object(html, vars_at).with_context(|| {
        format!("{step}: could not extract the `{INIT_FRIENDLY}` variables object from /me")
    })?;
    let base_vars: Map<String, Value> = serde_json::from_str(vars_text).with_context(|| {
        format!("{step}: `{INIT_FRIENDLY}` variables were not valid JSON (structure drift)")
    })?;

    // queryID (== doc_id for persisted queries); fall back to an explicit doc_id.
    let window = &html[anchor..(anchor + 4000).min(html.len())];
    let init_doc = first_capture(r#""queryID":"(\d+)""#, window)?
        .or(first_capture(r#""doc_id":"(\d+)""#, window)?)
        .with_context(|| {
            format!("{step}: initial query `doc_id`/`queryID` not found near `{INIT_FRIENDLY}`")
        })?;

    Ok(PartialSession {
        fb_dtsg,
        lsd,
        user_id,
        init_doc,
        base_vars,
    })
}

/// Session fields harvestable from `/me` alone (everything but `refetch_doc`).
#[derive(Debug)]
struct PartialSession {
    fb_dtsg: String,
    lsd: String,
    user_id: String,
    init_doc: String,
    base_vars: Map<String, Value>,
}

/// Extracts the `static.xx.fbcdn.net` script-bundle URLs referenced by the
/// `/me` HTML, de-duplicated in first-seen order.
fn bundle_urls(html: &str) -> Result<Vec<String>> {
    let re = Regex::new(r#"https://static\.xx\.fbcdn\.net/[^"'\\ )]+?\.js[^"'\\ )]*"#)
        .context("invalid bundle-url regex")?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(html) {
        let url = m.as_str().to_owned();
        if seen.insert(url.clone()) {
            out.push(url);
        }
    }
    Ok(out)
}

/// Greps a script bundle for the refetch persisted-operation `doc_id`.
fn refetch_doc_in_bundle(js: &str) -> Result<Option<String>> {
    let marker = format!("{REFETCH_FRIENDLY}_facebookRelayOperation");
    let Some(at) = js.find(&marker) else {
        return Ok(None);
    };
    let window = &js[at..(at + 2000).min(js.len())];
    first_capture(r#"exports\s*=\s*"(\d+)""#, window)
}

// ──────────────────────────── networked steps ──────────────────────────────

/// Decodes a response envelope body, honouring base64 transfer encoding.
fn decode_body(env: &ResponseEnvelope) -> Result<String> {
    match env.encoding.as_deref() {
        Some("base64") => {
            let bytes = BASE64
                .decode(env.body.as_bytes())
                .context("bridge sent an invalid base64 body")?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => Ok(env.body.clone()),
    }
}

/// The live harvest, holding the bridge client, config, and run state.
struct Harvester {
    client: BridgeClient,
    config: HarvestConfig,
    seen: HashSet<String>,
    count: usize,
    /// Posts buffered for the `json` format (unused for `jsonl`).
    buffered: Vec<Post>,
    /// Open append handle for the `jsonl` format (unused for `json`).
    sink: Option<Box<dyn std::io::Write + Send + Sync>>,
}

impl Harvester {
    /// Sends a buffered control request through the bridge and returns the
    /// decoded resource body, failing on a non-2xx *resource* status.
    async fn fetch(&self, req: &ControlRequest, what: &str) -> Result<String> {
        let env = self.client.send(req).await?;
        if !(200..300).contains(&env.status) {
            bail!("{what}: bridge fetched HTTP {} (expected 2xx)", env.status);
        }
        decode_body(&env)
    }

    /// Builds a same-origin GET against the connected tab.
    fn get(&self, url: &str, accept: &str) -> ControlRequest {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("Accept".to_string(), accept.to_string());
        ControlRequest {
            url: url.to_string(),
            method: "GET".to_string(),
            headers,
            body: None,
            stream: false,
            target: self.config.target.clone(),
            allow_origin: None,
            credentials: None,
        }
    }

    /// Step 2: fetch script bundles cross-origin (`--allow-origin` +
    /// `--credentials omit`) and grep for the refetch `doc_id`.
    async fn discover_refetch_doc(&self, html: &str) -> Result<String> {
        let step = "step 2 (discover doc_id)";
        let urls = bundle_urls(html)?;
        if urls.is_empty() {
            bail!("{step}: no {FBCDN_HOST} script bundles referenced by /me");
        }
        let mut tried = 0usize;
        for url in &urls {
            let req = ControlRequest {
                url: url.clone(),
                method: "GET".to_string(),
                headers: std::collections::BTreeMap::new(),
                body: None,
                stream: false,
                target: self.config.target.clone(),
                allow_origin: Some(FBCDN_HOST.to_string()),
                credentials: Some("omit".to_string()),
            };
            tried += 1;
            // A single failing bundle must not abort discovery.
            let Ok(js) = self.fetch(&req, step).await else {
                continue;
            };
            if let Some(doc) = refetch_doc_in_bundle(&js)? {
                tracing::info!("discovered refetch doc_id in bundle {tried}/{}", urls.len());
                return Ok(doc);
            }
        }
        bail!(
            "{step}: `{REFETCH_FRIENDLY}` doc_id not found in any of {tried} script bundle(s) — Facebook may have changed its bundle layout"
        )
    }

    /// Builds the form-encoded GraphQL POST body for one page.
    fn graphql_body(session: &Session, friendly: &str, doc_id: &str, variables: &Value) -> String {
        let vars = serde_json::to_string(variables).unwrap_or_else(|_| "{}".to_string());
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("av", &session.user_id)
            .append_pair("__a", "1")
            .append_pair("fb_dtsg", &session.fb_dtsg)
            .append_pair("lsd", &session.lsd)
            .append_pair("fb_api_caller_class", "RelayModern")
            .append_pair("fb_api_req_friendly_name", friendly)
            .append_pair("variables", &vars)
            .append_pair("server_timestamps", "true")
            .append_pair("doc_id", doc_id)
            .finish()
    }

    /// Posts one GraphQL page (with retries) and parses the response.
    async fn run_page(
        &self,
        session: &Session,
        friendly: &str,
        doc_id: &str,
        variables: &Value,
    ) -> Result<Page> {
        let body = Self::graphql_body(session, friendly, doc_id, variables);
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        headers.insert("x-fb-friendly-name".to_string(), friendly.to_string());
        headers.insert("x-fb-lsd".to_string(), session.lsd.clone());
        let req = ControlRequest {
            url: "/api/graphql/".to_string(),
            method: "POST".to_string(),
            headers,
            body: Some(body),
            stream: false,
            target: self.config.target.clone(),
            allow_origin: None,
            credentials: None,
        };

        let mut last_err = None;
        for attempt in 1..=PAGE_ATTEMPTS {
            match self.fetch(&req, "step 3 (paginate)").await {
                Ok(text) => return Ok(parse_stream(&text)),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < PAGE_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(750 * u64::from(attempt))).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("step 3 (paginate): request failed")))
            .with_context(|| {
                format!("step 3 (paginate): `{friendly}` failed after {PAGE_ATTEMPTS} attempts")
            })
    }

    /// Emits a page's fresh posts, applying dedup, `--since`, and `--limit`.
    /// Returns `true` when a stop condition (limit reached, or a post older than
    /// `--since`) was hit.
    fn absorb(&mut self, page: &Page) -> Result<bool> {
        let mut stop = false;
        for post in &page.posts {
            let Some(id) = post.id.clone() else {
                continue;
            };
            if !self.seen.insert(id) {
                continue;
            }
            if let (Some(since), Some(ct)) = (self.config.since, post.creation_time) {
                if ct < since {
                    stop = true;
                    continue; // skip posts older than the cutoff
                }
            }
            self.emit(post)?;
            self.count += 1;
            if self.config.limit.is_some_and(|n| self.count >= n) {
                return Ok(true);
            }
        }
        Ok(stop)
    }

    /// Writes one post in the configured format (streamed for `jsonl`, buffered
    /// for `json`).
    fn emit(&mut self, post: &Post) -> Result<()> {
        match self.config.format {
            Format::Jsonl => {
                let line = serde_json::to_string(post).context("Failed to serialise post")?;
                let sink = self
                    .sink
                    .as_mut()
                    .context("internal error: jsonl sink not opened")?;
                writeln!(sink, "{line}").context("Failed to write post")?;
                sink.flush().ok();
            }
            Format::Json => self.buffered.push(post.clone()),
        }
        Ok(())
    }
}

/// Runs the full Facebook timeline harvest described by `config`.
pub async fn run(config: HarvestConfig) -> Result<()> {
    let client = BridgeClient::new(config.control_port, config.token.clone());

    // Resume state: load the prior cursor/count if a resume path was given.
    let mut resume = match &config.resume {
        Some(path) => ResumeState::load(path)?,
        None => ResumeState::default(),
    };

    let mut harvester = Harvester {
        client,
        config: config.clone(),
        seen: HashSet::new(),
        count: 0,
        buffered: Vec::new(),
        sink: None,
    };

    // Seed dedup/count/buffer from any prior output so resume neither
    // re-emits nor (for `json`) drops earlier posts.
    harvester.preload_prior()?;
    harvester.open_sink(resume.end_cursor.is_some())?;

    // Step 1 + doc_id: always re-harvested, even on resume.
    tracing::info!("step 1: harvesting session tokens from /me");
    let me_html = harvester
        .fetch(
            &harvester.get("/me", "text/html"),
            "step 1 (harvest tokens)",
        )
        .await?;
    let partial = parse_session_from_me(&me_html)?;
    tracing::info!("step 2: discovering pagination doc_id from script bundles");
    let refetch_doc = harvester.discover_refetch_doc(&me_html).await?;
    let session = Session {
        fb_dtsg: partial.fb_dtsg,
        lsd: partial.lsd,
        user_id: partial.user_id,
        init_doc: partial.init_doc,
        refetch_doc,
        base_vars: partial.base_vars,
    };
    resume.user_id = Some(session.user_id.clone());

    // Step 3: paginate. On a fresh run, fetch the initial page first to obtain
    // the first cursor; on resume, jump straight to the saved cursor.
    let (mut cursor, mut has_next) = if let Some(c) = resume.end_cursor.clone() {
        tracing::info!("resuming from saved cursor");
        (Some(c), true)
    } else {
        let mut vars = Value::Object(session.base_vars.clone());
        set_var(&mut vars, "count", Value::from(5));
        set_var(&mut vars, "cursor", Value::Null);
        let page = harvester
            .run_page(&session, INIT_FRIENDLY, &session.init_doc, &vars)
            .await?;
        let stop = harvester.absorb(&page)?;
        tracing::info!(
            "step 3: initial page +{} (total {})",
            page.posts.len(),
            harvester.count
        );
        if stop {
            return harvester.finish(&config, &mut resume, page.end_cursor);
        }
        (page.end_cursor, page.has_next_page.unwrap_or(true))
    };

    let mut pages = 0u32;
    while has_next && pages < MAX_PAGES {
        let Some(cur) = cursor.clone() else { break };
        pages += 1;

        // Refetch variables: provider flags, minus userID, plus id/count/cursor.
        let mut vars = Value::Object(session.base_vars.clone());
        if let Value::Object(map) = &mut vars {
            map.remove("userID");
        }
        set_var(&mut vars, "id", Value::from(session.user_id.clone()));
        set_var(&mut vars, "count", Value::from(10));
        set_var(&mut vars, "cursor", Value::from(cur.clone()));

        let page = harvester
            .run_page(&session, REFETCH_FRIENDLY, &session.refetch_doc, &vars)
            .await?;
        let stop = harvester.absorb(&page)?;
        tracing::info!(
            "page {pages} (total {}) has_next={:?}",
            harvester.count,
            page.has_next_page
        );

        // Persist progress for resumability after each successful page.
        resume.end_cursor = page.end_cursor.clone();
        resume.count = harvester.count;
        if let Some(path) = &config.resume {
            resume.save(path)?;
        }

        if stop {
            break;
        }
        match &page.end_cursor {
            Some(next) if next != &cur => cursor = Some(next.clone()),
            _ => {
                tracing::info!("cursor did not advance; stopping");
                break;
            }
        }
        has_next = page.has_next_page.unwrap_or(false);
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    harvester.finish(&config, &mut resume, cursor)
}

impl Harvester {
    /// Pre-loads dedup ids and (for `json`) prior posts from an existing output
    /// file, so a resumed run continues cleanly.
    fn preload_prior(&mut self) -> Result<()> {
        let Output::File(path) = &self.config.output else {
            return Ok(());
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(()); // absent file → nothing to preload
        };
        match self.config.format {
            Format::Jsonl => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(post) = serde_json::from_str::<Post>(line) {
                        if let Some(id) = post.id {
                            self.seen.insert(id);
                        }
                    }
                }
            }
            Format::Json => {
                if let Ok(posts) = serde_json::from_str::<Vec<Post>>(&text) {
                    for post in posts {
                        if let Some(id) = &post.id {
                            self.seen.insert(id.clone());
                        }
                        self.buffered.push(post);
                    }
                }
            }
        }
        self.count = self.seen.len();
        Ok(())
    }

    /// Opens the streaming sink for `jsonl`. `append` keeps prior lines on resume.
    fn open_sink(&mut self, append: bool) -> Result<()> {
        if self.config.format != Format::Jsonl {
            return Ok(());
        }
        self.sink = Some(match &self.config.output {
            Output::Stdout => Box::new(std::io::stdout()),
            Output::File(path) => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(append)
                    .write(true)
                    .truncate(!append)
                    .open(path)
                    .with_context(|| format!("Failed to open output file {}", path.display()))?;
                Box::new(file)
            }
        });
        Ok(())
    }

    /// Finalises the run: writes the `json` array (if applicable), persists the
    /// final resume cursor, and prints a summary.
    fn finish(
        &self,
        config: &HarvestConfig,
        resume: &mut ResumeState,
        cursor: Option<String>,
    ) -> Result<()> {
        if self.config.format == Format::Json {
            let json = serde_json::to_string_pretty(&self.buffered)
                .context("Failed to serialise posts as a JSON array")?;
            match &self.config.output {
                Output::Stdout => println!("{json}"),
                Output::File(path) => std::fs::write(path, json)
                    .with_context(|| format!("Failed to write output file {}", path.display()))?,
            }
        }
        resume.end_cursor = cursor;
        resume.count = self.count;
        if let Some(path) = &config.resume {
            resume.save(path)?;
        }
        tracing::info!("done: {} posts", self.count);
        Ok(())
    }
}

/// Sets `key` to `value` on a JSON object, ignoring non-object values.
fn set_var(vars: &mut Value, key: &str, value: Value) {
    if let Value::Object(map) = vars {
        map.insert(key.to_string(), value);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A story node shaped like the real timeline response, for extraction.
    fn sample_node(id: &str, ct: i64) -> Value {
        json!({
            "id": id,
            "comet_sections": {
                "content": {"story": {
                    "wwwURL": "https://www.facebook.com/story",
                    "comet_sections": {"message": {"story": {"message": {"text": "hello world"}}}}
                }},
                "context_layout": {"story": {"comet_sections": {"metadata": [
                    {"story": {"creation_time": ct}}
                ]}}}
            },
            "attachments": [{"styles": {"attachment": {
                "story_attachment_link_renderer": {"attachment": {"web_link": {
                    "url": "https://example.com/shared"
                }}}
            }}}]
        })
    }

    #[test]
    fn extract_post_reads_the_full_field_map() {
        let post = extract_post(&sample_node("S1", 1_700_000_000)).unwrap();
        assert_eq!(post.id.as_deref(), Some("S1"));
        assert_eq!(post.creation_time, Some(1_700_000_000));
        assert_eq!(post.text.as_deref(), Some("hello world"));
        assert_eq!(post.url.as_deref(), Some("https://www.facebook.com/story"));
        assert_eq!(
            post.shared_link.as_deref(),
            Some("https://example.com/shared")
        );
    }

    #[test]
    fn extract_post_tolerates_missing_fields() {
        let post = extract_post(&json!({"id": "S2"})).unwrap();
        assert_eq!(post.id.as_deref(), Some("S2"));
        assert!(post.text.is_none());
        assert!(post.creation_time.is_none());
        assert!(extract_post(&json!("not-an-object")).is_none());
    }

    #[test]
    fn parse_stream_handles_refetch_node_nesting_and_deferred_page_info() {
        let edges = json!({"data": {"node": {"timeline_list_feed_units": {
            "edges": [{"node": sample_node("A", 100)}, {"node": sample_node("B", 90)}]
        }}}});
        let page_info = json!({"data": {"node": {"timeline_list_feed_units": {
            "page_info": {"end_cursor": "CURSOR2", "has_next_page": true}
        }}}});
        let body = format!("{edges}\n{page_info}\n");
        let page = parse_stream(&body);
        assert_eq!(page.posts.len(), 2);
        assert_eq!(page.posts[0].id.as_deref(), Some("A"));
        assert_eq!(page.end_cursor.as_deref(), Some("CURSOR2"));
        assert_eq!(page.has_next_page, Some(true));
    }

    #[test]
    fn parse_stream_handles_initial_user_nesting_and_streamed_single_edge() {
        let edges = json!({"data": {"user": {"timeline_list_feed_units": {
            "edges": [{"node": sample_node("A", 100)}]
        }}}});
        let streamed = json!({"data": {"node": sample_node("C", 80), "cursor": "x"}});
        let body = format!("{edges}\n{streamed}\ngarbage line\n");
        let page = parse_stream(&body);
        let ids: Vec<_> = page.posts.iter().filter_map(|p| p.id.clone()).collect();
        assert_eq!(ids, vec!["A", "C"]);
    }

    #[test]
    fn balanced_object_slices_nested_braces_and_strings() {
        let text = r#"prefix "variables":{"a":{"b":"}"},"c":1} suffix"#;
        let at = text.find("\"variables\"").unwrap();
        assert_eq!(
            balanced_object(text, at).unwrap(),
            r#"{"a":{"b":"}"},"c":1}"#
        );
    }

    #[test]
    fn parse_session_from_me_extracts_tokens_and_initial_query() {
        let html = concat!(
            r#"junk "DTSGInitialData",[],{"token":"DTSG_TOK"} more "#,
            r#""LSD",[],{"token":"LSD_TOK"} and "USER_ID":"55501" then "#,
            r#"ProfileCometTimelineFeedQuery preload "variables":{"userID":"55501","count":3,"__pv":true} "#,
            r#""queryID":"111222333" tail"#
        );
        let s = parse_session_from_me(html).unwrap();
        assert_eq!(s.fb_dtsg, "DTSG_TOK");
        assert_eq!(s.lsd, "LSD_TOK");
        assert_eq!(s.user_id, "55501");
        assert_eq!(s.init_doc, "111222333");
        assert_eq!(
            s.base_vars.get("userID").and_then(Value::as_str),
            Some("55501")
        );
        assert_eq!(s.base_vars.get("__pv").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_session_from_me_errors_name_the_missing_piece() {
        let err = parse_session_from_me("nothing useful here").unwrap_err();
        assert!(err.to_string().contains("fb_dtsg"), "got: {err}");
    }

    #[test]
    fn bundle_urls_dedups_in_order() {
        let html = concat!(
            r#"<script src="https://static.xx.fbcdn.net/rsrc.php/v3/a/one.js?_nc=1"></script>"#,
            r#"<script src="https://static.xx.fbcdn.net/rsrc.php/v3/b/two.js"></script>"#,
            r#"<script src="https://static.xx.fbcdn.net/rsrc.php/v3/a/one.js?_nc=1"></script>"#,
        );
        let urls = bundle_urls(html).unwrap();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("one.js?_nc=1"));
        assert!(urls[1].ends_with("two.js"));
    }

    #[test]
    fn refetch_doc_in_bundle_greps_the_exports_id() {
        let js = r#"__d("ProfileCometTimelineFeedRefetchQuery_facebookRelayOperation",[],(function(a){a.exports="27008916165384435"}),null);"#;
        assert_eq!(
            refetch_doc_in_bundle(js).unwrap().as_deref(),
            Some("27008916165384435")
        );
        assert!(refetch_doc_in_bundle("unrelated bundle").unwrap().is_none());
    }

    #[test]
    fn resume_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = ResumeState {
            end_cursor: Some("CUR".into()),
            count: 42,
            user_id: Some("999".into()),
        };
        state.save(&path).unwrap();
        let back = ResumeState::load(&path).unwrap();
        assert_eq!(back.end_cursor.as_deref(), Some("CUR"));
        assert_eq!(back.count, 42);
        // A missing file loads as the default rather than erroring.
        let absent = ResumeState::load(&dir.path().join("nope.json")).unwrap();
        assert!(absent.end_cursor.is_none());
    }

    /// Builds a network-free harvester (port 0 never connects) for absorb tests.
    fn test_harvester(format: Format, since: Option<i64>, limit: Option<usize>) -> Harvester {
        Harvester {
            client: BridgeClient::new(0, "t".into()),
            config: HarvestConfig {
                control_port: 0,
                token: "t".into(),
                target: None,
                output: Output::Stdout,
                format,
                since,
                limit,
                resume: None,
            },
            seen: HashSet::new(),
            count: 0,
            buffered: Vec::new(),
            sink: None,
        }
    }

    fn page_of(ids_times: &[(&str, i64)]) -> Page {
        Page {
            posts: ids_times
                .iter()
                .map(|(id, ct)| extract_post(&sample_node(id, *ct)).unwrap())
                .collect(),
            end_cursor: None,
            has_next_page: Some(true),
        }
    }

    #[test]
    fn absorb_dedups_by_id() {
        let mut h = test_harvester(Format::Json, None, None);
        let stop = h
            .absorb(&page_of(&[("A", 100), ("A", 100), ("B", 90)]))
            .unwrap();
        assert!(!stop);
        assert_eq!(h.buffered.len(), 2);
        assert_eq!(h.count, 2);
    }

    #[test]
    fn absorb_stops_at_limit() {
        let mut h = test_harvester(Format::Json, None, Some(2));
        let stop = h
            .absorb(&page_of(&[("A", 100), ("B", 90), ("C", 80)]))
            .unwrap();
        assert!(stop);
        assert_eq!(h.buffered.len(), 2);
    }

    #[test]
    fn absorb_stops_when_older_than_since() {
        // Newest-first; cutoff 95 keeps A(100), drops B(90), signals stop.
        let mut h = test_harvester(Format::Json, Some(95), None);
        let stop = h.absorb(&page_of(&[("A", 100), ("B", 90)])).unwrap();
        assert!(stop);
        assert_eq!(h.buffered.len(), 1);
        assert_eq!(h.buffered[0].id.as_deref(), Some("A"));
    }

    #[test]
    fn graphql_body_carries_required_fields() {
        let session = Session {
            fb_dtsg: "D".into(),
            lsd: "L".into(),
            user_id: "7".into(),
            init_doc: "1".into(),
            refetch_doc: "2".into(),
            base_vars: Map::new(),
        };
        let body = Harvester::graphql_body(
            &session,
            "FriendlyName",
            "2",
            &json!({"id": "7", "count": 10}),
        );
        assert!(body.contains("doc_id=2"));
        assert!(body.contains("fb_dtsg=D"));
        assert!(body.contains("fb_api_req_friendly_name=FriendlyName"));
        assert!(body.contains("av=7"));
    }

    #[test]
    fn set_var_inserts_into_object_only() {
        let mut v = json!({});
        set_var(&mut v, "count", json!(10));
        assert_eq!(v.get("count"), Some(&json!(10)));
        let mut not_obj = json!(5);
        set_var(&mut not_obj, "count", json!(10)); // no-op, no panic
        assert_eq!(not_obj, json!(5));
    }

    #[test]
    fn decode_body_handles_plain_and_base64() {
        let plain = ResponseEnvelope {
            id: 1,
            status: 200,
            headers: std::collections::BTreeMap::new(),
            body: "hi".into(),
            encoding: None,
        };
        assert_eq!(decode_body(&plain).unwrap(), "hi");

        let b64 = ResponseEnvelope {
            body: BASE64.encode("bytes"),
            encoding: Some("base64".into()),
            ..plain.clone()
        };
        assert_eq!(decode_body(&b64).unwrap(), "bytes");

        let bad = ResponseEnvelope {
            body: "!!!not-base64!!!".into(),
            encoding: Some("base64".into()),
            ..plain
        };
        assert!(decode_body(&bad).is_err());
    }

    #[test]
    fn get_builds_a_same_origin_request_with_target() {
        let mut h = test_harvester(Format::Jsonl, None, None);
        h.config.target = Some("3".into());
        let req = h.get("/me", "text/html");
        assert_eq!(req.url, "/me");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.headers.get("Accept").map(String::as_str),
            Some("text/html")
        );
        assert_eq!(req.target.as_deref(), Some("3"));
        assert!(req.allow_origin.is_none());
        assert!(req.credentials.is_none());
    }

    #[test]
    fn emit_jsonl_appends_lines_to_the_output_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posts.jsonl");
        let mut h = test_harvester(Format::Jsonl, None, None);
        h.config.output = Output::File(path.clone());
        h.open_sink(false).unwrap();
        h.emit(&extract_post(&sample_node("A", 100)).unwrap())
            .unwrap();
        h.emit(&extract_post(&sample_node("B", 90)).unwrap())
            .unwrap();
        drop(h.sink.take()); // flush + close
        let lines: Vec<_> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"id\":\"A\""));
    }

    #[test]
    fn preload_prior_seeds_seen_from_existing_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posts.jsonl");
        std::fs::write(
            &path,
            "{\"id\":\"A\",\"creation_time\":1,\"text\":null,\"url\":null,\"shared_link\":null}\n\
             {\"id\":\"B\",\"creation_time\":2,\"text\":null,\"url\":null,\"shared_link\":null}\n",
        )
        .unwrap();
        let mut h = test_harvester(Format::Jsonl, None, None);
        h.config.output = Output::File(path);
        h.preload_prior().unwrap();
        assert_eq!(h.count, 2);
        assert!(h.seen.contains("A") && h.seen.contains("B"));
    }

    #[test]
    fn preload_prior_seeds_buffer_from_existing_json_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("posts.json");
        let posts = vec![extract_post(&sample_node("A", 1)).unwrap()];
        std::fs::write(&path, serde_json::to_string(&posts).unwrap()).unwrap();
        let mut h = test_harvester(Format::Json, None, None);
        h.config.output = Output::File(path);
        h.preload_prior().unwrap();
        assert_eq!(h.buffered.len(), 1);
        assert!(h.seen.contains("A"));
    }

    #[test]
    fn finish_writes_json_array_and_persists_resume() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.json");
        let state = dir.path().join("run.state");
        let mut h = test_harvester(Format::Json, None, None);
        h.config.output = Output::File(out.clone());
        h.config.resume = Some(state.clone());
        h.buffered.push(extract_post(&sample_node("A", 1)).unwrap());
        h.count = 1;
        let mut resume = ResumeState::default();
        h.finish(&h.config.clone(), &mut resume, Some("CUR".into()))
            .unwrap();

        let written: Vec<Post> =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(written.len(), 1);
        let saved = ResumeState::load(&state).unwrap();
        assert_eq!(saved.end_cursor.as_deref(), Some("CUR"));
        assert_eq!(saved.count, 1);
    }

    // ── End-to-end `run` against a mocked control plane ──────────────────────

    /// HTML `/me` shell carrying tokens, the initial query, and a bundle URL.
    fn fake_me_html() -> String {
        concat!(
            r#""DTSGInitialData",[],{"token":"DTSG_TOK"} "LSD",[],{"token":"LSD_TOK"} "#,
            r#""USER_ID":"55501" ProfileCometTimelineFeedQuery "#,
            r#""variables":{"userID":"55501","count":3} "queryID":"111" "#,
            r#"<script src="https://static.xx.fbcdn.net/rsrc.php/v3/a/one.js"></script>"#
        )
        .to_string()
    }

    /// A bundle exposing the refetch persisted-operation id.
    fn fake_bundle_js() -> String {
        r#"__d("ProfileCometTimelineFeedRefetchQuery_facebookRelayOperation",[],(function(a){a.exports="222333"}),null);"#.to_string()
    }

    /// A single-post stream/defer page. With `page_info`, it carries a terminal
    /// cursor; without it (drift / a malformed defer line), the cursor never
    /// advances.
    fn fake_graphql_page(id: &str, with_page_info: bool) -> String {
        let edges = json!({"data": {"node": {"timeline_list_feed_units": {
            "edges": [{"node": sample_node(id, 100)}]
        }}}});
        if !with_page_info {
            return format!("{edges}\n");
        }
        let page_info = json!({"data": {"node": {"timeline_list_feed_units": {
            "page_info": {"end_cursor": "C1", "has_next_page": false}
        }}}});
        format!("{edges}\n{page_info}\n")
    }

    /// A control plane that answers each `ControlRequest` by inspecting its URL:
    /// `/me` → HTML shell, an fbcdn URL → bundle, anything else → GraphQL page.
    /// Fields let individual tests inject drift (a non-2xx resource status, a
    /// bundle missing the refetch marker).
    struct ControlPlane {
        post_id: String,
        /// Resource status returned for the `/me` fetch (200 unless overridden).
        me_status: u16,
        /// Whether the served bundle carries the refetch persisted-op marker.
        bundle_has_marker: bool,
        /// Whether GraphQL pages carry a `page_info` (and thus advance the cursor).
        page_has_info: bool,
    }

    impl ControlPlane {
        fn happy(post_id: &str) -> Self {
            Self {
                post_id: post_id.to_string(),
                me_status: 200,
                bundle_has_marker: true,
                page_has_info: true,
            }
        }
    }

    impl wiremock::Respond for ControlPlane {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let cr: ControlRequest = serde_json::from_slice(&request.body).unwrap();
            let (status, body) = if cr.url == "/me" {
                (self.me_status, fake_me_html())
            } else if cr.url.starts_with("https://static.xx.fbcdn.net") {
                let js = if self.bundle_has_marker {
                    fake_bundle_js()
                } else {
                    "no refetch marker here".to_string()
                };
                (200, js)
            } else {
                (200, fake_graphql_page(&self.post_id, self.page_has_info))
            };
            let env = ResponseEnvelope {
                id: 1,
                status,
                headers: std::collections::BTreeMap::new(),
                body,
                encoding: None,
            };
            wiremock::ResponseTemplate::new(200).set_body_json(&env)
        }
    }

    async fn mount(plane: ControlPlane) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/__bridge/request"))
            .respond_with(plane)
            .mount(&server)
            .await;
        server
    }

    async fn mount_control_plane(post_id: &str) -> wiremock::MockServer {
        mount(ControlPlane::happy(post_id)).await
    }

    /// A `HarvestConfig` writing jsonl to `out` against `port`.
    fn run_config(port: u16, out: PathBuf, resume: Option<PathBuf>) -> HarvestConfig {
        HarvestConfig {
            control_port: port,
            token: "tok".into(),
            target: None,
            output: Output::File(out),
            format: Format::Jsonl,
            since: None,
            limit: None,
            resume,
        }
    }

    #[tokio::test]
    async fn run_fresh_harvests_tokens_discovers_doc_and_writes_posts() {
        let server = mount_control_plane("FRESH").await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.jsonl");
        run(run_config(server.address().port(), out.clone(), None))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("\"id\":\"FRESH\""), "got: {text}");
    }

    #[tokio::test]
    async fn run_resume_skips_initial_and_enters_refetch_loop() {
        let server = mount_control_plane("RESUMED").await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.jsonl");
        let state = dir.path().join("run.state");
        // A prior cursor sends `run` straight into the refetch loop.
        ResumeState {
            end_cursor: Some("C0".into()),
            count: 0,
            user_id: Some("55501".into()),
        }
        .save(&state)
        .unwrap();

        run(run_config(
            server.address().port(),
            out.clone(),
            Some(state.clone()),
        ))
        .await
        .unwrap();
        assert!(std::fs::read_to_string(&out)
            .unwrap()
            .contains("\"id\":\"RESUMED\""));
        // The loop advanced and persisted the new cursor.
        assert_eq!(
            ResumeState::load(&state).unwrap().end_cursor.as_deref(),
            Some("C1")
        );
    }

    #[tokio::test]
    async fn run_fails_with_staged_error_when_me_fetch_is_non_2xx() {
        let server = mount(ControlPlane {
            me_status: 404,
            ..ControlPlane::happy("X")
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let err = run(run_config(
            server.address().port(),
            dir.path().join("p.jsonl"),
            None,
        ))
        .await
        .unwrap_err();
        // The staged error names the step and the unexpected resource status.
        assert!(err.to_string().contains("step 1"), "got: {err}");
        assert!(err.to_string().contains("404"), "got: {err}");
    }

    #[tokio::test]
    async fn run_fails_when_no_bundle_carries_the_refetch_doc_id() {
        let server = mount(ControlPlane {
            bundle_has_marker: false,
            ..ControlPlane::happy("X")
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let err = run(run_config(
            server.address().port(),
            dir.path().join("p.jsonl"),
            None,
        ))
        .await
        .unwrap_err();
        assert!(err.to_string().contains("step 2"), "got: {err}");
        assert!(
            err.to_string()
                .contains("ProfileCometTimelineFeedRefetchQuery"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_stream_skips_blank_and_dataless_lines() {
        let body = "\n   \n{\"no_data\":1}\n{\"data\":{}}\n";
        let page = parse_stream(body);
        assert!(page.posts.is_empty());
        assert!(page.end_cursor.is_none());
    }

    #[test]
    fn balanced_object_handles_escaped_quotes_and_rejects_unbalanced() {
        // An escaped quote inside the string must not end brace tracking early.
        let with_escape = r#"x:{"k":"a\"b}c"}y"#;
        assert_eq!(
            balanced_object(with_escape, 0).unwrap(),
            r#"{"k":"a\"b}c"}"#
        );
        // No opening brace, and an unbalanced object, both yield None.
        assert!(balanced_object("no braces here", 0).is_none());
        assert!(balanced_object("{\"k\":1", 0).is_none());
    }

    #[test]
    fn parse_session_errors_name_each_missing_field() {
        let base = concat!(
            r#""DTSGInitialData",[],{"token":"D"} "LSD",[],{"token":"L"} "#,
            r#""USER_ID":"5" ProfileCometTimelineFeedQuery "#,
            r#""variables":{"userID":"5"} "queryID":"111""#
        );
        // Drop one required piece at a time and assert the error points at it.
        for (needle, expect) in [
            (r#""LSD",[],{"token":"L"} "#, "lsd"),
            (r#""USER_ID":"5" "#, "USER_ID"),
            ("ProfileCometTimelineFeedQuery ", "preloader block"),
            (r#""variables":{"userID":"5"} "#, "variables"),
            (r#" "queryID":"111""#, "queryID"),
        ] {
            let broken = base.replace(needle, "");
            let err = parse_session_from_me(&broken).unwrap_err().to_string();
            assert!(err.contains(expect), "removing {needle:?} → {err}");
        }
    }

    #[test]
    fn parse_session_errors_when_variables_object_is_malformed() {
        let tokens = r#""DTSGInitialData",[],{"token":"D"} "LSD",[],{"token":"L"} "USER_ID":"5" "#;
        // An unbalanced `variables` object can't be sliced out.
        let unbalanced = format!(
            "{tokens} ProfileCometTimelineFeedQuery \"variables\":{{\"userID\":\"5\" \"queryID\":\"1\""
        );
        let err = parse_session_from_me(&unbalanced).unwrap_err().to_string();
        assert!(err.contains("variables"), "got: {err}");

        // A balanced but non-JSON `variables` object fails to deserialise.
        let invalid = format!(
            "{tokens} ProfileCometTimelineFeedQuery \"variables\":{{not valid json}} \"queryID\":\"1\""
        );
        let err = parse_session_from_me(&invalid).unwrap_err().to_string();
        assert!(err.contains("variables"), "got: {err}");
    }

    #[test]
    fn resume_state_load_surfaces_non_notfound_read_errors() {
        // A directory path is not "not found" — read fails for another reason,
        // exercising the contextual error arm rather than the default.
        let dir = tempfile::tempdir().unwrap();
        assert!(ResumeState::load(dir.path()).is_err());
    }

    #[tokio::test]
    async fn discover_refetch_doc_bails_when_me_references_no_bundles() {
        let h = test_harvester(Format::Jsonl, None, None);
        let err = h
            .discover_refetch_doc("<html>no fbcdn script tags here</html>")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no "), "got: {err}");
        assert!(err.to_string().contains("step 2"), "got: {err}");
    }

    /// A jsonl `HarvestConfig` with an explicit `limit` and optional resume.
    fn run_config_limited(
        port: u16,
        out: PathBuf,
        resume: Option<PathBuf>,
        limit: usize,
    ) -> HarvestConfig {
        HarvestConfig {
            limit: Some(limit),
            ..run_config(port, out, resume)
        }
    }

    #[tokio::test]
    async fn run_fresh_stops_on_initial_page_when_limit_is_reached() {
        // limit=1 with a one-post initial page trips the stop on the initial
        // page itself, exercising the early `finish` return.
        let server = mount_control_plane("FRESH").await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.jsonl");
        run(run_config_limited(
            server.address().port(),
            out.clone(),
            None,
            1,
        ))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn run_resume_loop_stops_when_limit_is_reached() {
        let server = mount_control_plane("RESUMED").await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.jsonl");
        let state = dir.path().join("run.state");
        ResumeState {
            end_cursor: Some("C0".into()),
            count: 0,
            user_id: None,
        }
        .save(&state)
        .unwrap();
        run(run_config_limited(
            server.address().port(),
            out.clone(),
            Some(state),
            1,
        ))
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn run_resume_stops_when_cursor_does_not_advance() {
        // A page without `page_info` yields no new cursor, so the loop stops.
        let server = mount(ControlPlane {
            page_has_info: false,
            ..ControlPlane::happy("STUCK")
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("posts.jsonl");
        let state = dir.path().join("run.state");
        ResumeState {
            end_cursor: Some("C0".into()),
            count: 0,
            user_id: None,
        }
        .save(&state)
        .unwrap();
        run(run_config(
            server.address().port(),
            out.clone(),
            Some(state),
        ))
        .await
        .unwrap();
        // The single page's post was still emitted before the stop.
        assert!(std::fs::read_to_string(&out)
            .unwrap()
            .contains("\"id\":\"STUCK\""));
    }

    #[tokio::test]
    async fn run_fresh_json_format_writes_array_to_stdout() {
        // format=json + stdout output exercises the json branch of `finish`.
        let server = mount_control_plane("JSONOUT").await;
        let config = HarvestConfig {
            format: Format::Json,
            output: Output::Stdout,
            ..run_config(server.address().port(), PathBuf::new(), None)
        };
        run(config).await.unwrap();
    }
}
