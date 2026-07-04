//! Filter construction and the `--query` mini-language.
//!
//! A [`Filter`] is the AND of every supplied flag plus every `--query`
//! expression. The query language supports `AND`/`OR`/`NOT` (and a leading `-`
//! for negation), parentheses, `field:value` structured terms, and bare fuzzy
//! tokens matched against the raw JSON line. Field matching is shared with the
//! structured flags so `--status 5xx` and `status:5xx` behave identically.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;

use crate::request_log::{LogRecord, RecordKind, Source};

/// The raw, borrowed flag values used to build a [`Filter`].
pub struct FilterInput<'a> {
    pub since: Option<&'a str>,
    pub method: Option<&'a str>,
    pub status: Option<&'a str>,
    pub service: Option<&'a str>,
    pub command: Option<&'a str>,
    pub url: Option<&'a str>,
    pub grep: Option<&'a str>,
    pub fuzzy: &'a [String],
    pub query: &'a [String],
    pub id: Option<&'a str>,
}

/// A compiled predicate over [`LogRecord`] lines.
pub struct Filter {
    since: Option<DateTime<Utc>>,
    method: Option<String>,
    status: Option<StatusMatcher>,
    service: Option<String>,
    command: Option<String>,
    url: Option<String>,
    grep: Option<Regex>,
    fuzzy: Vec<String>,
    id: Option<String>,
    queries: Vec<Expr>,
}

impl Filter {
    /// Compiles all flags and query expressions up front, surfacing parse
    /// errors (bad regex/status/duration/query) before streaming begins.
    pub fn build(input: FilterInput<'_>) -> Result<Self> {
        let since = match input.since {
            Some(s) => Some(parse_since(s)?),
            None => None,
        };
        let status = match input.status {
            Some(s) => Some(StatusMatcher::parse(s)?),
            None => None,
        };
        let grep = match input.grep {
            Some(s) => Some(Regex::new(s).with_context(|| format!("invalid --grep regex: {s}"))?),
            None => None,
        };
        let mut queries = Vec::new();
        for q in input.query {
            queries.push(parse_query(q).with_context(|| format!("invalid --query: {q}"))?);
        }
        Ok(Self {
            since,
            method: input.method.map(str::to_string),
            status,
            service: input.service.map(str::to_string),
            command: input.command.map(str::to_string),
            url: input.url.map(str::to_string),
            grep,
            fuzzy: input.fuzzy.to_vec(),
            id: input.id.map(str::to_string),
            queries,
        })
    }

    /// Whether `rec` (whose verbatim JSON line is `raw`) passes every clause.
    pub fn matches(&self, rec: &LogRecord, raw: &str) -> bool {
        let raw_lower = raw.to_ascii_lowercase();

        if let Some(cutoff) = self.since {
            match parse_timestamp(&rec.timestamp) {
                Some(ts) if ts >= cutoff => {}
                _ => return false,
            }
        }
        if let Some(m) = &self.method {
            if !opt_eq_ci(rec.method.as_deref(), m) {
                return false;
            }
        }
        if let Some(s) = &self.status {
            if !s.matches(rec.status_code) {
                return false;
            }
        }
        if let Some(s) = &self.service {
            if !opt_eq_ci(rec.service.as_deref(), s) {
                return false;
            }
        }
        if let Some(c) = &self.command {
            if !command_matches(rec, c) {
                return false;
            }
        }
        if let Some(u) = &self.url {
            if !contains_ci(rec.url.as_deref(), u) {
                return false;
            }
        }
        if let Some(re) = &self.grep {
            if !re.is_match(raw) {
                return false;
            }
        }
        for token in &self.fuzzy {
            if !raw_lower.contains(&token.to_ascii_lowercase()) {
                return false;
            }
        }
        if let Some(id) = &self.id {
            if &rec.id != id && &rec.invocation_id != id {
                return false;
            }
        }
        for q in &self.queries {
            if !q.eval(rec, &raw_lower) {
                return false;
            }
        }
        true
    }
}

/// Parses a relative duration like `30m`, `2h`, `1d`, `1w`, `45s` into the
/// absolute cutoff `now - duration`. Shared by `--since` (log search) and
/// `--older-than` (`omni-dev log prune`).
pub(crate) fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .with_context(|| format!("invalid duration: {s} (expected e.g. 30m, 2h, 1d)"))?,
    );
    let n: i64 = num
        .parse()
        .with_context(|| format!("invalid duration: {s} (expected e.g. 30m, 2h, 1d)"))?;
    let dur = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        other => bail!("invalid duration unit: {other} (use s, m, h, d, or w)"),
    };
    Ok(Utc::now() - dur)
}

/// Parses an RFC3339 timestamp into UTC, or `None` if absent/unparseable.
fn parse_timestamp(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A status filter: a set of exact codes and/or `Nxx` classes.
struct StatusMatcher {
    exact: Vec<u16>,
    classes: Vec<u8>,
}

impl StatusMatcher {
    /// Parses `"200"`, `"5xx"`, or a comma list like `"4xx,5xx"`.
    fn parse(spec: &str) -> Result<Self> {
        let mut exact = Vec::new();
        let mut classes = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let lower = part.to_ascii_lowercase();
            if let Some(first) = lower.strip_suffix("xx") {
                let digit: u8 = first
                    .parse()
                    .with_context(|| format!("invalid status class: {part}"))?;
                if !(1..=5).contains(&digit) {
                    bail!("invalid status class: {part}");
                }
                classes.push(digit);
            } else {
                exact.push(
                    part.parse()
                        .with_context(|| format!("invalid status code: {part}"))?,
                );
            }
        }
        if exact.is_empty() && classes.is_empty() {
            bail!("empty --status filter");
        }
        Ok(Self { exact, classes })
    }

    /// Whether a (possibly absent) status code matches this filter.
    fn matches(&self, code: Option<u16>) -> bool {
        let Some(code) = code else {
            return false;
        };
        self.exact.contains(&code) || self.classes.contains(&((code / 100) as u8))
    }
}

/// Whether the record's command path matches `prefix` on whole path segments
/// (so `jira` matches `["jira","read"]` but `jir` does not).
fn command_matches(rec: &LogRecord, prefix: &str) -> bool {
    let joined = rec.command.join(" ");
    let prefix = prefix.trim();
    joined == prefix || joined.starts_with(&format!("{prefix} "))
}

/// Case-insensitive equality against an optional field.
fn opt_eq_ci(field: Option<&str>, value: &str) -> bool {
    field.is_some_and(|f| f.eq_ignore_ascii_case(value))
}

/// Case-insensitive substring against an optional field.
fn contains_ci(field: Option<&str>, value: &str) -> bool {
    field.is_some_and(|f| f.to_ascii_lowercase().contains(&value.to_ascii_lowercase()))
}

/// Lowercase string form of a [`RecordKind`].
fn kind_str(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Invocation => "invocation",
        RecordKind::Http => "http",
        RecordKind::Unknown => "unknown",
    }
}

/// Lowercase string form of a [`Source`].
fn source_str(source: Source) -> &'static str {
    match source {
        Source::Cli => "cli",
        Source::Mcp => "mcp",
        Source::Daemon => "daemon",
        Source::Unknown => "unknown",
    }
}

/// Evaluates a `field:value` term against a record (shared by the query AST).
fn field_matches(rec: &LogRecord, field: &str, value: &str) -> bool {
    match field.to_ascii_lowercase().as_str() {
        "kind" => kind_str(rec.kind).eq_ignore_ascii_case(value),
        "source" => rec
            .source
            .is_some_and(|s| source_str(s).eq_ignore_ascii_case(value)),
        "service" => opt_eq_ci(rec.service.as_deref(), value),
        "method" => opt_eq_ci(rec.method.as_deref(), value),
        "status" => StatusMatcher::parse(value).is_ok_and(|m| m.matches(rec.status_code)),
        "command" | "cmd" => command_matches(rec, value),
        "url" => contains_ci(rec.url.as_deref(), value),
        "id" => rec.id == value || rec.invocation_id == value,
        "invocation_id" | "inv" => rec.invocation_id == value,
        "mcp_tool" | "tool" => opt_eq_ci(rec.mcp_tool.as_deref(), value),
        "via_daemon" => rec.via_daemon == matches!(value, "1" | "true" | "yes"),
        "error" | "err" => match rec.error.as_deref() {
            Some(e) if value.is_empty() || value == "true" => !e.is_empty() || value == "true",
            Some(e) => e.to_ascii_lowercase().contains(&value.to_ascii_lowercase()),
            None => false,
        },
        _ => false,
    }
}

// --- `--query` mini-language ---

/// A parsed query expression tree.
enum Expr {
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
    /// `field:value` structured term.
    Field(String, String),
    /// Bare fuzzy token, matched against the lowercased raw line.
    Term(String),
}

impl Expr {
    /// Evaluates against a record and its lowercased raw JSON line.
    fn eval(&self, rec: &LogRecord, raw_lower: &str) -> bool {
        match self {
            Self::And(a, b) => a.eval(rec, raw_lower) && b.eval(rec, raw_lower),
            Self::Or(a, b) => a.eval(rec, raw_lower) || b.eval(rec, raw_lower),
            Self::Not(a) => !a.eval(rec, raw_lower),
            Self::Field(f, v) => field_matches(rec, f, v),
            Self::Term(t) => raw_lower.contains(&t.to_ascii_lowercase()),
        }
    }
}

/// A query token stream cursor for the recursive-descent parser.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Word(String),
}

/// Splits a query string into tokens, honoring parentheses, `"quoted values"`,
/// and a leading `-` as negation.
fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '-' => {
                chars.next();
                // A lone '-' is a stray; '-foo' negates the following word.
                tokens.push(Token::Not);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' {
                        break;
                    }
                    if c == '"' {
                        chars.next();
                        for qc in chars.by_ref() {
                            if qc == '"' {
                                break;
                            }
                            word.push(qc);
                        }
                        continue;
                    }
                    word.push(c);
                    chars.next();
                }
                match word.to_ascii_uppercase().as_str() {
                    "AND" => tokens.push(Token::And),
                    "OR" => tokens.push(Token::Or),
                    "NOT" => tokens.push(Token::Not),
                    _ => tokens.push(Token::Word(word)),
                }
            }
        }
    }
    Ok(tokens)
}

/// Parses a `--query` expression into an [`Expr`] tree.
fn parse_query(input: &str) -> Result<Expr> {
    let tokens = tokenize(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        bail!("unexpected trailing tokens in query");
    }
    Ok(expr)
}

/// Recursive-descent parser: `or := and ("OR" and)*`,
/// `and := unary (("AND")? unary)*`, `unary := "NOT" unary | primary`,
/// `primary := "(" or ")" | term`.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Some(Token::And) => {
                    self.pos += 1;
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                // Implicit AND between adjacent terms (stop at OR/`)`/EOF).
                Some(Token::Word(_) | Token::Not | Token::LParen) => {
                    let right = self.parse_unary()?;
                    left = Expr::And(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        match self.tokens.get(self.pos) {
            Some(Token::LParen) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                match self.tokens.get(self.pos) {
                    Some(Token::RParen) => {
                        self.pos += 1;
                        Ok(inner)
                    }
                    _ => bail!("unbalanced parenthesis in query"),
                }
            }
            Some(Token::Word(word)) => {
                let word = word.clone();
                self.pos += 1;
                Ok(match word.split_once(':') {
                    Some((field, value)) if !field.is_empty() => {
                        Expr::Field(field.to_string(), value.to_string())
                    }
                    _ => Expr::Term(word),
                })
            }
            _ => bail!("expected a term in query"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn http(status: Option<u16>, service: &str, method: &str) -> LogRecord {
        LogRecord {
            kind: RecordKind::Http,
            service: Some(service.to_string()),
            method: Some(method.to_string()),
            status_code: status,
            ..LogRecord::default()
        }
    }

    #[test]
    fn status_matcher_handles_exact_class_and_list() {
        let m = StatusMatcher::parse("200").unwrap();
        assert!(m.matches(Some(200)));
        assert!(!m.matches(Some(201)));

        let m = StatusMatcher::parse("5xx").unwrap();
        assert!(m.matches(Some(503)));
        assert!(!m.matches(Some(404)));
        assert!(!m.matches(None));

        let m = StatusMatcher::parse("4xx,5xx").unwrap();
        assert!(m.matches(Some(404)));
        assert!(m.matches(Some(500)));
        assert!(!m.matches(Some(204)));
    }

    #[test]
    fn status_matcher_rejects_garbage() {
        assert!(StatusMatcher::parse("9xx").is_err());
        assert!(StatusMatcher::parse("abc").is_err());
        assert!(StatusMatcher::parse("").is_err());
    }

    #[test]
    fn since_parses_units() {
        assert!(parse_since("30m").is_ok());
        assert!(parse_since("2h").is_ok());
        assert!(parse_since("1d").is_ok());
        assert!(parse_since("1w").is_ok());
        assert!(parse_since("10x").is_err());
        assert!(parse_since("h").is_err());
    }

    #[test]
    fn command_prefix_matches() {
        let rec = LogRecord {
            command: vec!["jira".to_string(), "read".to_string()],
            ..LogRecord::default()
        };
        assert!(command_matches(&rec, "jira"));
        assert!(command_matches(&rec, "jira read"));
        assert!(!command_matches(&rec, "git"));
    }

    #[test]
    fn query_field_and_implicit_and() {
        let rec = http(Some(500), "jira", "POST");
        let expr = parse_query("status:5xx service:jira").unwrap();
        assert!(expr.eval(&rec, "{}"));
        let expr = parse_query("status:5xx service:datadog").unwrap();
        assert!(!expr.eval(&rec, "{}"));
    }

    #[test]
    fn query_or_not_and_parens() {
        let rec = http(Some(404), "jira", "GET");
        assert!(parse_query("status:5xx OR status:4xx")
            .unwrap()
            .eval(&rec, "{}"));
        assert!(parse_query("NOT status:5xx").unwrap().eval(&rec, "{}"));
        assert!(parse_query("-status:5xx").unwrap().eval(&rec, "{}"));
        assert!(parse_query("(status:4xx OR status:5xx) method:GET")
            .unwrap()
            .eval(&rec, "{}"));
        assert!(!parse_query("(status:4xx OR status:5xx) method:POST")
            .unwrap()
            .eval(&rec, "{}"));
    }

    #[test]
    fn query_bare_token_is_fuzzy_on_raw() {
        let rec = LogRecord::default();
        let expr = parse_query("deploy").unwrap();
        assert!(expr.eval(&rec, r#"{"url":"/api/deploy"}"#));
        assert!(!expr.eval(&rec, r#"{"url":"/api/status"}"#));
    }

    #[test]
    fn query_rejects_unbalanced_parens() {
        assert!(parse_query("(status:5xx").is_err());
        assert!(parse_query("status:5xx)").is_err());
    }

    #[test]
    fn filter_ands_flags_together() {
        let rec = http(Some(500), "jira", "GET");
        let pass = Filter::build(FilterInput {
            since: None,
            method: Some("GET"),
            status: Some("5xx"),
            service: Some("jira"),
            command: None,
            url: None,
            grep: None,
            fuzzy: &[],
            query: &[],
            id: None,
        })
        .unwrap();
        assert!(pass.matches(&rec, "{}"));

        let fail = Filter::build(FilterInput {
            since: None,
            method: Some("POST"),
            status: None,
            service: None,
            command: None,
            url: None,
            grep: None,
            fuzzy: &[],
            query: &[],
            id: None,
        })
        .unwrap();
        assert!(!fail.matches(&rec, "{}"));
    }

    fn empty_input<'a>() -> FilterInput<'a> {
        FilterInput {
            since: None,
            method: None,
            status: None,
            service: None,
            command: None,
            url: None,
            grep: None,
            fuzzy: &[],
            query: &[],
            id: None,
        }
    }

    fn rec_http() -> LogRecord {
        LogRecord {
            id: "rec-1".to_string(),
            invocation_id: "inv-9".to_string(),
            kind: RecordKind::Http,
            timestamp: "2026-06-22T10:00:00.000Z".to_string(),
            service: Some("jira".to_string()),
            method: Some("GET".to_string()),
            url: Some("https://acme.atlassian.net/rest/api/3/issue/X-1".to_string()),
            status_code: Some(200),
            ..LogRecord::default()
        }
    }

    #[test]
    fn build_rejects_bad_inputs() {
        let mut i = empty_input();
        i.grep = Some("(");
        assert!(Filter::build(i).is_err(), "bad regex");

        let mut i = empty_input();
        i.status = Some("9xx");
        assert!(Filter::build(i).is_err(), "bad status class");

        let mut i = empty_input();
        i.since = Some("bogus");
        assert!(Filter::build(i).is_err(), "bad since");

        let bad_query = vec!["(unclosed".to_string()];
        let mut i = empty_input();
        i.query = &bad_query;
        assert!(Filter::build(i).is_err(), "bad query");
    }

    #[test]
    fn matches_each_flag() {
        let rec = rec_http();
        let raw = serde_json::to_string(&rec).unwrap();

        let mut i = empty_input();
        i.method = Some("get");
        assert!(Filter::build(i).unwrap().matches(&rec, &raw));
        let mut i = empty_input();
        i.method = Some("post");
        assert!(!Filter::build(i).unwrap().matches(&rec, &raw));

        let mut i = empty_input();
        i.service = Some("jira");
        assert!(Filter::build(i).unwrap().matches(&rec, &raw));

        let mut i = empty_input();
        i.url = Some("issue/X-1");
        assert!(Filter::build(i).unwrap().matches(&rec, &raw));
        let mut i = empty_input();
        i.url = Some("nope");
        assert!(!Filter::build(i).unwrap().matches(&rec, &raw));

        let mut i = empty_input();
        i.grep = Some("X-\\d+");
        assert!(Filter::build(i).unwrap().matches(&rec, &raw));

        let toks = vec!["jira".to_string(), "issue".to_string()];
        let mut i = empty_input();
        i.fuzzy = &toks;
        assert!(Filter::build(i).unwrap().matches(&rec, &raw));
        let toks = vec!["absent".to_string()];
        let mut i = empty_input();
        i.fuzzy = &toks;
        assert!(!Filter::build(i).unwrap().matches(&rec, &raw));

        for id in ["rec-1", "inv-9"] {
            let mut i = empty_input();
            i.id = Some(id);
            assert!(Filter::build(i).unwrap().matches(&rec, &raw), "id {id}");
        }
        let mut i = empty_input();
        i.id = Some("other");
        assert!(!Filter::build(i).unwrap().matches(&rec, &raw));
    }

    #[test]
    fn since_filters_by_recency() {
        let raw = "{}";
        let mut past = rec_http();
        past.timestamp = "2000-01-01T00:00:00.000Z".to_string();
        let mut future = rec_http();
        future.timestamp = "2999-01-01T00:00:00.000Z".to_string();
        let mut undated = rec_http();
        undated.timestamp = String::new();

        let mut i = empty_input();
        i.since = Some("1d");
        let f = Filter::build(i).unwrap();
        assert!(!f.matches(&past, raw));
        assert!(f.matches(&future, raw));
        assert!(
            !f.matches(&undated, raw),
            "unparseable timestamp is excluded"
        );
    }

    #[test]
    fn query_covers_every_field_arm() {
        let mut rec = rec_http();
        rec.source = Some(Source::Mcp);
        rec.mcp_tool = Some("jira_read".to_string());
        rec.via_daemon = true;
        rec.error = Some("boom timeout".to_string());
        rec.command = vec!["jira".to_string(), "read".to_string()];
        let raw = serde_json::to_string(&rec).unwrap().to_ascii_lowercase();

        let cases = [
            ("kind:http", true),
            ("kind:invocation", false),
            ("source:mcp", true),
            ("source:cli", false),
            ("service:jira", true),
            ("method:GET", true),
            ("status:2xx", true),
            ("status:5xx", false),
            ("command:jira", true),
            ("cmd:\"jira read\"", true),
            ("url:issue", true),
            ("id:rec-1", true),
            ("id:inv-9", true),
            ("id:nope", false),
            ("inv:inv-9", true),
            ("invocation_id:inv-9", true),
            ("tool:jira_read", true),
            ("mcp_tool:other", false),
            ("via_daemon:true", true),
            ("via_daemon:false", false),
            ("error:timeout", true),
            ("err:absent", false),
            ("error:", true),
            ("unknownfield:x", false),
        ];
        for (q, expected) in cases {
            let parsed = parse_query(q).unwrap();
            assert_eq!(parsed.eval(&rec, &raw), expected, "query: {q}");
        }
    }

    #[test]
    fn query_parser_edge_cases() {
        let rec = LogRecord::default();
        let raw = r#"{"x":"hello world"}"#.to_ascii_lowercase();

        assert!(parse_query("\"hello world\"").unwrap().eval(&rec, &raw));
        assert!(parse_query("hello AND world").unwrap().eval(&rec, &raw));
        assert!(!parse_query("NOT hello").unwrap().eval(&rec, &raw));
        assert!(parse_query("(hello OR nope) AND world")
            .unwrap()
            .eval(&rec, &raw));

        assert!(parse_query("(hello").is_err());
        assert!(parse_query("hello )").is_err());
        assert!(parse_query("").is_err());
    }

    #[test]
    fn matches_rejects_on_each_clause() {
        let mut rec = rec_http();
        rec.command = vec!["jira".to_string(), "read".to_string()];
        let raw = serde_json::to_string(&rec).unwrap();

        // Each clause, set to a value the record does NOT satisfy, fails the match.
        let mut status = empty_input();
        status.status = Some("5xx");
        assert!(!Filter::build(status).unwrap().matches(&rec, &raw));

        let mut service = empty_input();
        service.service = Some("datadog");
        assert!(!Filter::build(service).unwrap().matches(&rec, &raw));

        let mut command = empty_input();
        command.command = Some("git");
        assert!(!Filter::build(command).unwrap().matches(&rec, &raw));
        // …and the matching command passes.
        let mut command = empty_input();
        command.command = Some("jira");
        assert!(Filter::build(command).unwrap().matches(&rec, &raw));

        let mut url = empty_input();
        url.url = Some("absent-path");
        assert!(!Filter::build(url).unwrap().matches(&rec, &raw));

        let mut grep = empty_input();
        grep.grep = Some("ZZZ-not-present");
        assert!(!Filter::build(grep).unwrap().matches(&rec, &raw));

        // A --query clause that fails also rejects the record.
        let q = vec!["service:datadog".to_string()];
        let mut query = empty_input();
        query.query = &q;
        assert!(!Filter::build(query).unwrap().matches(&rec, &raw));
    }

    #[test]
    fn since_rejects_all_digit_and_empty_number() {
        // No unit (all digits) hits the "no non-digit found" error path.
        let mut all_digits = empty_input();
        all_digits.since = Some("30");
        assert!(Filter::build(all_digits).is_err());

        // A leading non-digit yields an empty number, hitting the parse error.
        let mut empty_num = empty_input();
        empty_num.since = Some("xh");
        assert!(Filter::build(empty_num).is_err());
    }

    #[test]
    fn query_covers_kind_source_and_error_variants() {
        let raw = "{}".to_ascii_lowercase();

        for (kind, q, want) in [
            (RecordKind::Invocation, "kind:invocation", true),
            (RecordKind::Http, "kind:invocation", false),
            (RecordKind::Unknown, "kind:unknown", true),
        ] {
            let rec = LogRecord {
                kind,
                ..LogRecord::default()
            };
            assert_eq!(parse_query(q).unwrap().eval(&rec, &raw), want, "{q}");
        }

        for (source, q, want) in [
            (Source::Cli, "source:cli", true),
            (Source::Daemon, "source:daemon", true),
            (Source::Unknown, "source:unknown", true),
            (Source::Daemon, "source:cli", false),
        ] {
            let rec = LogRecord {
                source: Some(source),
                ..LogRecord::default()
            };
            assert_eq!(parse_query(q).unwrap().eval(&rec, &raw), want, "{q}");
        }

        // The error arm with no error present returns false.
        let rec = LogRecord::default();
        assert!(!parse_query("error:boom").unwrap().eval(&rec, &raw));
    }
}
