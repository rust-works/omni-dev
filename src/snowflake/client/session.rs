//! A live Snowflake session: query execution and token lifecycle.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};

use crate::utils::secret::Secret;

use super::error::{Error, Result};
use super::row::{Column, Row};
use super::transport::{request_id, Transport};

/// How often to poll an in-progress (async) query for its result.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Tokens returned by login, consumed by [`SnowflakeSession::new`].
pub(crate) struct LoginTokens {
    /// Session token (authorizes queries; short-lived; redacted in `Debug` output).
    pub session_token: Secret,
    /// Master token (authorizes renewal; longer-lived; redacted in `Debug` output).
    pub master_token: Secret,
    /// Session-token validity in seconds.
    pub session_validity_secs: i64,
    /// Master-token validity in seconds.
    pub master_validity_secs: i64,
}

/// The mutable token state, refreshed by `renew`/`heartbeat`.
#[derive(Clone, Debug)]
struct Tokens {
    session_token: Secret,
    master_token: Secret,
    session_expires_at: DateTime<Utc>,
    master_expires_at: DateTime<Utc>,
}

/// A live, authenticated Snowflake session.
///
/// Holds the session and master tokens behind a mutex so a query (reading the
/// session token) and a heartbeat/renew can interleave. `query` runs SQL;
/// `renew` swaps in a fresh session token via the master token; `heartbeat`
/// keeps the master token alive so renewal can continue indefinitely.
pub struct SnowflakeSession {
    transport: Arc<Transport>,
    tokens: StdMutex<Tokens>,
    /// Overall deadline for one query (submit + async polling).
    query_timeout: Duration,
}

impl SnowflakeSession {
    /// Builds a session from a transport and freshly issued tokens.
    pub(crate) fn new(
        transport: Arc<Transport>,
        tokens: LoginTokens,
        query_timeout: Duration,
    ) -> Self {
        let now = Utc::now();
        Self {
            transport,
            tokens: StdMutex::new(Tokens {
                session_token: tokens.session_token,
                master_token: tokens.master_token,
                session_expires_at: now + TimeDelta::seconds(tokens.session_validity_secs),
                master_expires_at: now + TimeDelta::seconds(tokens.master_validity_secs),
            }),
            query_timeout,
        }
    }

    /// Whether the session token expires within `within` from now.
    #[must_use]
    pub fn session_expiring_within(&self, within: TimeDelta) -> bool {
        Utc::now() + within >= self.lock().session_expires_at
    }

    /// When the master token currently expires (renewal must happen before this,
    /// kept alive by [`heartbeat`](Self::heartbeat)).
    #[must_use]
    pub fn master_expires_at(&self) -> DateTime<Utc> {
        self.lock().master_expires_at
    }

    /// Runs SQL and returns the result rows.
    ///
    /// # Errors
    ///
    /// [`Error::SessionExpired`] when the session token is no longer valid (renew
    /// or re-authenticate), or a transport/server/protocol error.
    pub async fn query(&self, sql: &str) -> Result<Vec<Row>> {
        let token = self.lock().session_token.clone();
        let rid = request_id();
        let body = json!({ "sqlText": sql });
        // `post_statement` submits the query and polls the async result URL until
        // it completes, so a query slower than the server's synchronous window
        // (anything heavy) is not killed by a per-request timeout.
        let data = self
            .transport
            .post_statement(
                &[("requestId", rid.as_str())],
                &body,
                token.expose_secret(),
                self.query_timeout,
                POLL_INTERVAL,
            )
            .await?;

        let (columns, index, mut rows) = parse_result(&data)?;
        // Large results stream the tail as external blob chunks; download and
        // append each (gzip-compressed JSON arrays).
        if let Some(chunks) = data.get("chunks").and_then(Value::as_array) {
            let headers = parse_chunk_headers(&data);
            for chunk in chunks {
                if let Some(url) = chunk.get("url").and_then(Value::as_str) {
                    let bytes = self.transport.get_bytes(url, &headers).await?;
                    rows.extend(decode_chunk_rows(&bytes, &columns, &index)?);
                }
            }
        }
        Ok(rows)
    }

    /// Renews the session token using the master token, extending the session.
    ///
    /// # Errors
    ///
    /// [`Error::SessionExpired`] when the master token has itself expired (a full
    /// re-authentication is then required), or a transport/server error.
    pub async fn renew(&self) -> Result<()> {
        let (old_session, master) = {
            let tokens = self.lock();
            (tokens.session_token.clone(), tokens.master_token.clone())
        };
        let rid = request_id();
        let body =
            json!({ "oldSessionToken": old_session.expose_secret(), "requestType": "RENEW" });
        let data = self
            .transport
            .post(
                "session/token-request",
                &[("requestId", rid.as_str())],
                &body,
                Some(master.expose_secret()),
            )
            .await?;

        let new_session = data
            .get("sessionToken")
            .or_else(|| data.get("token"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("token-request returned no sessionToken".into()))?
            .to_string();
        let st_validity = data
            .get("validityInSecondsST")
            .or_else(|| data.get("validityInSeconds"))
            .and_then(Value::as_i64)
            .unwrap_or(3600);

        let mut tokens = self.lock();
        tokens.session_token = new_session.into();
        tokens.session_expires_at = Utc::now() + TimeDelta::seconds(st_validity);
        if let Some(master_token) = data.get("masterToken").and_then(Value::as_str) {
            tokens.master_token = master_token.into();
        }
        if let Some(mt_validity) = data.get("validityInSecondsMT").and_then(Value::as_i64) {
            tokens.master_expires_at = Utc::now() + TimeDelta::seconds(mt_validity);
        }
        Ok(())
    }

    /// Sends a keep-alive heartbeat, extending the master token server-side.
    ///
    /// # Errors
    ///
    /// A transport/server error, or [`Error::SessionExpired`] if the session
    /// token used to authorize the heartbeat has already lapsed.
    pub async fn heartbeat(&self) -> Result<()> {
        let token = self.lock().session_token.clone();
        let rid = request_id();
        self.transport
            .post(
                "session/heartbeat",
                &[("requestId", rid.as_str())],
                &json!({}),
                Some(token.expose_secret()),
            )
            .await?;
        // The server resets the master token's validity; the precise value
        // comes back on the next renew.
        Ok(())
    }

    /// Logs the session out (best-effort).
    ///
    /// # Errors
    ///
    /// A transport/server error.
    pub async fn close(&self) -> Result<()> {
        let token = self.lock().session_token.clone();
        let rid = request_id();
        self.transport
            .post(
                "session",
                &[("delete", "true"), ("requestId", rid.as_str())],
                &json!({}),
                Some(token.expose_secret()),
            )
            .await?;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, Tokens> {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Schema (columns + uppercased name index) and inline rows from a response.
type ParsedResult = (Arc<Vec<Column>>, Arc<HashMap<String, usize>>, Vec<Row>);

/// Parses a query-request `data` payload into its schema and inline rows.
/// External result chunks, if any, are downloaded separately by the caller.
fn parse_result(data: &Value) -> Result<ParsedResult> {
    if let Some(format) = data.get("queryResultFormat").and_then(Value::as_str) {
        if !format.eq_ignore_ascii_case("json") {
            return Err(Error::Unsupported(format!(
                "result format '{format}' (only JSON is supported)"
            )));
        }
    }

    let rowtype = data
        .get("rowtype")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("query response missing rowtype".into()))?;
    let columns: Arc<Vec<Column>> = Arc::new(rowtype.iter().map(parse_column).collect());

    let mut index = HashMap::new();
    for (i, col) in columns.iter().enumerate() {
        index.entry(col.name.to_ascii_uppercase()).or_insert(i);
    }
    let index = Arc::new(index);

    let rows = data
        .get("rowset")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| row_from_cells(row, &columns, &index))
                .collect()
        })
        .unwrap_or_default();
    Ok((columns, index, rows))
}

/// Builds a [`Row`] from a JSON array of cells.
fn row_from_cells(
    row: &Value,
    columns: &Arc<Vec<Column>>,
    index: &Arc<HashMap<String, usize>>,
) -> Row {
    let cells = row
        .as_array()
        .map(|cells| cells.iter().map(cell_to_string).collect())
        .unwrap_or_default();
    Row::new(cells, Arc::clone(columns), Arc::clone(index))
}

/// Extracts the per-chunk HTTP headers from a query response.
fn parse_chunk_headers(data: &Value) -> Vec<(String, String)> {
    data.get("chunkHeaders")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Decodes a downloaded result chunk (gzip-compressed JSON array of rows).
fn decode_chunk_rows(
    bytes: &[u8],
    columns: &Arc<Vec<Column>>,
    index: &Arc<HashMap<String, usize>>,
) -> Result<Vec<Row>> {
    let json = gunzip_if_needed(bytes)?;
    // Snowflake serves each chunk as bare, comma-separated row arrays
    // (`[r1],[r2],…`) designed to be concatenated, not a self-contained JSON
    // array — so wrap with `[` … `]` before parsing.
    let mut framed = Vec::with_capacity(json.len() + 2);
    framed.push(b'[');
    framed.extend_from_slice(&json);
    framed.push(b']');
    let rows: Vec<Value> = serde_json::from_slice(&framed)
        .map_err(|e| Error::Protocol(format!("invalid result chunk JSON: {e}")))?;
    Ok(rows
        .iter()
        .map(|row| row_from_cells(row, columns, index))
        .collect())
}

/// Gunzips `bytes` when they carry the gzip magic, else returns them unchanged.
fn gunzip_if_needed(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).map_err(Error::Io)?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Parses one `rowtype` entry into a [`Column`].
fn parse_column(value: &Value) -> Column {
    Column {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ty: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase(),
        nullable: value
            .get("nullable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        length: value.get("length").and_then(Value::as_i64),
        precision: value.get("precision").and_then(Value::as_i64),
        scale: value.get("scale").and_then(Value::as_i64),
    }
}

/// Normalizes a rowset cell to its raw string form (or `None` for null).
fn cell_to_string(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::snowflake::client::row::rows_to_payload;
    use url::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A session whose transport points at `server`, with the given session-token
    /// validity. The master token is long-lived.
    fn live_session(server: &MockServer, session_validity_secs: i64) -> SnowflakeSession {
        let base = Url::parse(&server.uri()).unwrap().join("/").unwrap();
        let transport = Arc::new(Transport::with_base_url(base, Duration::from_secs(5)).unwrap());
        SnowflakeSession::new(
            transport,
            LoginTokens {
                session_token: "sess".into(),
                master_token: "mast".into(),
                session_validity_secs,
                master_validity_secs: 14_400,
            },
            Duration::from_secs(5),
        )
    }

    #[test]
    fn token_accessors_reflect_validities() {
        // The token accessors never touch the network.
        let base = Url::parse("https://acct.example/").unwrap();
        let transport = Arc::new(Transport::with_base_url(base, Duration::from_secs(5)).unwrap());
        let session = SnowflakeSession::new(
            transport,
            LoginTokens {
                session_token: "s".into(),
                master_token: "m".into(),
                session_validity_secs: 3600,
                master_validity_secs: 14_400,
            },
            Duration::from_secs(5),
        );
        assert!(session.master_expires_at() > Utc::now());
        assert!(!session.session_expiring_within(TimeDelta::seconds(60)));
        assert!(session.session_expiring_within(TimeDelta::seconds(7200)));
    }

    #[test]
    fn tokens_debug_redacts_secrets() {
        let tokens = Tokens {
            session_token: "sekret-session-token".into(),
            master_token: "sekret-master-token".into(),
            session_expires_at: Utc::now(),
            master_expires_at: Utc::now(),
        };
        // Debug must never print the token values (#1131).
        let debug = format!("{tokens:?}");
        assert!(
            !debug.contains("sekret-session-token"),
            "leaked session token: {debug}"
        );
        assert!(
            !debug.contains("sekret-master-token"),
            "leaked master token: {debug}"
        );
        assert!(debug.contains("session_token: <redacted>"));
        assert!(debug.contains("master_token: <redacted>"));
    }

    #[tokio::test]
    async fn query_returns_inline_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": {
                    "queryResultFormat": "json",
                    "rowtype": [{ "name": "N", "type": "fixed", "precision": 38, "scale": 0 }],
                    "rowset": [["1"], ["2"]],
                }
            })))
            .mount(&server)
            .await;

        let rows = live_session(&server, 3600).query("SELECT 1").await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn query_downloads_and_appends_external_chunks() {
        let server = MockServer::start().await;
        let chunk_url = format!("{}/chunk0", server.uri());
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": {
                    "queryResultFormat": "json",
                    "rowtype": [{ "name": "N", "type": "text" }],
                    "rowset": [["a"]],
                    "chunks": [{ "url": chunk_url }],
                }
            })))
            .mount(&server)
            .await;
        // A real chunk is bare, comma-separated row arrays (no enclosing brackets).
        Mock::given(method("GET"))
            .and(path("/chunk0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(br#"["b"],["c"]"#.to_vec()))
            .mount(&server)
            .await;

        let rows = live_session(&server, 3600).query("SELECT 1").await.unwrap();
        assert_eq!(rows.len(), 3, "1 inline + 2 chunked");
    }

    #[tokio::test]
    async fn query_surfaces_session_expired() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/queries/v1/query-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": false, "code": "390112", "message": "expired", "data": {}
            })))
            .mount(&server)
            .await;

        let err = live_session(&server, 3600)
            .query("SELECT 1")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::SessionExpired));
    }

    #[tokio::test]
    async fn renew_swaps_in_a_fresh_session_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/token-request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": { "sessionToken": "new-sess", "validityInSecondsST": 3600 }
            })))
            .mount(&server)
            .await;

        // A nearly-expired session is no longer about to expire after a renew.
        let session = live_session(&server, 1);
        assert!(session.session_expiring_within(TimeDelta::seconds(120)));
        session.renew().await.unwrap();
        assert!(!session.session_expiring_within(TimeDelta::seconds(120)));
    }

    #[tokio::test]
    async fn renew_errors_when_response_lacks_a_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/token-request"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": {} })),
            )
            .mount(&server)
            .await;

        let err = live_session(&server, 3600).renew().await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn heartbeat_and_close_post_successfully() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/heartbeat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": {} })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": {} })),
            )
            .mount(&server)
            .await;

        let session = live_session(&server, 3600);
        session.heartbeat().await.unwrap();
        session.close().await.unwrap();
    }

    #[test]
    fn parse_result_builds_columns_and_inline_rows() {
        let data = json!({
            "queryResultFormat": "json",
            "rowtype": [
                { "name": "ID", "type": "fixed", "nullable": false, "precision": 38, "scale": 0 },
                { "name": "NAME", "type": "text", "nullable": true, "length": 16_777_216 },
            ],
            "rowset": [["1", "alice"], ["2", null]],
        });
        let (_columns, _index, rows) = parse_result(&data).unwrap();
        assert_eq!(rows.len(), 2);
        let payload = rows_to_payload(&rows);
        assert_eq!(payload["columns"][0]["name"], "ID");
        assert_eq!(payload["columns"][0]["type"], "fixed(38,0)");
        assert_eq!(payload["rows"][0]["ID"], 1);
        assert_eq!(payload["rows"][0]["NAME"], "alice");
        assert_eq!(payload["rows"][1]["NAME"], Value::Null);
    }

    #[test]
    fn parse_result_rejects_arrow_but_allows_chunked() {
        let arrow = json!({ "queryResultFormat": "arrow", "rowtype": [], "rowset": [] });
        assert!(matches!(parse_result(&arrow), Err(Error::Unsupported(_))));
        // Chunked responses are no longer rejected at parse — the inline rows are
        // returned and chunks are downloaded separately.
        let chunked = json!({
            "queryResultFormat": "json",
            "rowtype": [{ "name": "C", "type": "text" }],
            "rowset": [["a"]],
            "chunks": [{ "url": "https://example/chunk0" }],
        });
        let (_c, _i, rows) = parse_result(&chunked).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn parse_result_requires_rowtype() {
        assert!(matches!(
            parse_result(&json!({ "rowset": [] })),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn decode_chunk_rows_handles_gzip_and_plain_json() {
        use std::io::Write as _;
        let (columns, index, _rows) = parse_result(&json!({
            "queryResultFormat": "json",
            "rowtype": [
                { "name": "ID", "type": "fixed", "precision": 38, "scale": 0 },
                { "name": "NAME", "type": "text" },
            ],
            "rowset": [],
        }))
        .unwrap();

        // A real chunk is bare, comma-separated row arrays WITHOUT enclosing
        // brackets — the multi-row case that the framing fix must handle.
        let chunk_json = br#"["3","carol"],["4",null]"#;

        // Plain JSON.
        let rows = decode_chunk_rows(chunk_json, &columns, &index).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_json_object().get("NAME"), Some(&json!("carol")));
        assert_eq!(rows[1].to_json_object().get("NAME"), Some(&Value::Null));

        // Gzip-compressed (as chunks arrive over the wire).
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(chunk_json).unwrap();
        let gzipped = encoder.finish().unwrap();
        let rows = decode_chunk_rows(&gzipped, &columns, &index).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].to_json_object().get("ID"), Some(&json!(3)));
    }

    #[test]
    fn parse_chunk_headers_reads_string_pairs() {
        let data = json!({ "chunkHeaders": { "x-amz-server-side-encryption": "AES256" } });
        let headers = parse_chunk_headers(&data);
        assert_eq!(
            headers,
            vec![(
                "x-amz-server-side-encryption".to_string(),
                "AES256".to_string()
            )]
        );
    }
}
