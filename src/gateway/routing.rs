//! Named upstream configuration and bounded, fail-closed model discovery.
//!
//! Custom exact routes precede ordered `*` globs. Discovery never performs
//! inference and never retries a failed inference on another provider.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::config::{normalize_base_url, GatewayConfig};
use super::error::GatewayError;
use super::flavor::{Flavor, UpstreamProtocol};
use super::models::{glob_matches, ModelMap};
use crate::utils::env::{non_empty_var, EnvSource};
use crate::utils::secret::Secret;
use crate::utils::secret_env::{secret_var, SECRET_ENV_VARS};

/// JSON array of named upstream definitions, mutually exclusive with legacy settings.
pub const UPSTREAMS_VAR: &str = "OMNI_DEV_GATEWAY_UPSTREAMS";
/// JSON array of `{pattern, upstream, model?}` custom model routes.
pub const ROUTES_VAR: &str = "OMNI_DEV_GATEWAY_ROUTES";
const SUCCESS_TTL: Duration = Duration::from_secs(60);
const FAILURE_TTL: Duration = Duration::from_secs(5);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PAGES: usize = 16;
const MAX_PAGE_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 16_384;

/// A configured upstream. Keys are resolved separately through the secret resolver.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Unique local name (not a URL).
    pub name: String,
    /// Inference base URL.
    pub url: String,
    /// Wire protocol.
    pub protocol: UpstreamProtocol,
    /// Translation quirks.
    pub flavor: Flavor,
    pub(crate) api_key: Option<Secret>,
    /// Catalog base URL; defaults to the inference URL.
    pub catalog_url: String,
    /// Catalog paths. Empty disables discovery for this upstream.
    pub catalog_paths: Vec<String>,
    pub(crate) effort_levels: Option<String>,
    pub(crate) think_tags: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpstream {
    name: String,
    url: String,
    #[serde(default = "chat")]
    protocol: String,
    #[serde(default = "generic")]
    flavor: String,
    api_key_env: Option<String>,
    catalog_url: Option<String>,
    catalog_paths: Option<Vec<String>>,
    effort_levels: Option<String>,
    #[serde(default = "yes")]
    think_tags: bool,
}
fn chat() -> String {
    "chat".into()
}
fn generic() -> String {
    "generic".into()
}
fn yes() -> bool {
    true
}

/// A route may rename a model, or preserve the requested model when `model` is absent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRoute {
    /// Exact name or `*` glob.
    pub pattern: String,
    /// Name from the upstream array.
    pub upstream: String,
    /// Optional literal upstream model (not a glob substitution).
    pub model: Option<String>,
}

/// Optional routing mode, loaded identically by the standalone server and daemon.
#[derive(Debug, Clone, Default)]
pub struct RoutingConfig {
    /// Configured upstreams, at most 32.
    pub upstreams: Vec<Upstream>,
    /// Custom routes, in glob precedence order (exact routes always win).
    pub routes: Vec<ModelRoute>,
}

impl RoutingConfig {
    pub(crate) fn from_env(env: &impl EnvSource) -> Result<Self> {
        let raw: Vec<RawUpstream> = non_empty_var(env, UPSTREAMS_VAR)
            .map(|s| serde_json::from_str(&s).context("invalid named upstream JSON"))
            .transpose()?
            .unwrap_or_default();
        if raw.len() > 32 {
            bail!("at most 32 named upstreams are supported");
        }
        let routes: Vec<ModelRoute> = non_empty_var(env, ROUTES_VAR)
            .map(|s| serde_json::from_str(&s).context("invalid model route JSON"))
            .transpose()?
            .unwrap_or_default();
        let mut upstreams = Vec::new();
        let mut names = HashSet::new();
        for u in raw {
            if u.name.is_empty()
                || !u
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !names.insert(u.name.clone())
            {
                bail!("upstream names must be unique nonempty ASCII identifiers");
            }
            let protocol = UpstreamProtocol::parse(&u.protocol)?;
            let flavor = Flavor::parse(&u.flavor)?;
            let url = normalize_base_url(&u.url)?;
            let catalog_url = u
                .catalog_url
                .as_deref()
                .map(normalize_base_url)
                .transpose()?
                .unwrap_or_else(|| url.clone());
            let catalog_paths = u.catalog_paths.unwrap_or_else(|| {
                if protocol == UpstreamProtocol::Bedrock {
                    Vec::new()
                } else {
                    vec!["/v1/models".into()]
                }
            });
            if catalog_paths.len() > 4 {
                bail!("at most four catalog paths per upstream are supported");
            }
            for path in &catalog_paths {
                let base = reqwest::Url::parse(&catalog_url)?;
                let joined = base.join(path)?;
                if !path.starts_with('/')
                    || path.starts_with("//")
                    || path.contains('\\')
                    || joined.origin() != base.origin()
                    || joined.fragment().is_some()
                {
                    bail!("catalog paths must be absolute paths on their configured origin");
                }
            }
            let api_key = if let Some(key) = u.api_key_env {
                if !SECRET_ENV_VARS.contains(&key.as_str()) || key == super::config::TOKEN_VAR {
                    bail!("api_key_env must reference a registered upstream secret variable, never OMNI_DEV_GATEWAY_TOKEN");
                }
                Some(
                    secret_var(env, &key)
                        .with_context(|| format!("could not resolve upstream secret `{key}`"))?
                        .context("named upstream secret variable is unset")?,
                )
            } else {
                None
            };
            if let Some(levels) = &u.effort_levels {
                super::config::parse_effort_levels(levels)?;
            }
            upstreams.push(Upstream {
                name: u.name,
                url,
                protocol,
                flavor,
                api_key,
                catalog_url,
                catalog_paths,
                effort_levels: u.effort_levels,
                think_tags: u.think_tags,
            });
        }
        let mut patterns = HashSet::new();
        for route in &routes {
            if route.pattern.is_empty()
                || route.model.as_ref().is_some_and(String::is_empty)
                || !names.contains(&route.upstream)
                || !patterns.insert(&route.pattern)
            {
                bail!("routes require unique nonempty patterns, a configured upstream and a nonempty optional model");
            }
        }
        Ok(Self { upstreams, routes })
    }

    fn custom(&self, requested: &str) -> Option<(usize, String)> {
        let route = self
            .routes
            .iter()
            .find(|r| r.pattern == requested)
            .or_else(|| {
                self.routes
                    .iter()
                    .find(|r| glob_matches(&r.pattern, requested))
            })?;
        let index = self
            .upstreams
            .iter()
            .position(|u| u.name == route.upstream)?;
        Some((
            index,
            route.model.clone().unwrap_or_else(|| requested.to_string()),
        ))
    }

    /// Builds a per-request translation configuration, retaining shared listener policy.
    pub(crate) fn selected(
        &self,
        original: &GatewayConfig,
        index: usize,
        requested: &str,
        model: &str,
    ) -> GatewayConfig {
        let u = &self.upstreams[index];
        let mut config = original.clone();
        config.upstream_url.clone_from(&u.url);
        config.protocol = u.protocol;
        config.flavor = u.flavor;
        config.api_key.clone_from(&u.api_key);
        config.think_tags = u.think_tags;
        config.effort_levels = u
            .effort_levels
            .as_deref()
            .and_then(|s| super::config::parse_effort_levels(s).ok())
            .unwrap_or_else(|| u.flavor.effort_levels().to_vec());
        // This map is internal and must not parse model identifiers as map syntax.
        config.models = ModelMap::single(requested, model);
        config
    }
}

type Owners = BTreeMap<String, Vec<usize>>;
struct Cached {
    at: Instant,
    value: Result<Owners, GatewayError>,
}

/// One cache shared by all requests, including negative results. The lock also
/// coalesces concurrent refreshes. Errors are cached briefly; stale ownership is never used.
#[derive(Default)]
pub(crate) struct Discovery(Mutex<Option<Cached>>);

impl Discovery {
    async fn catalog(
        &self,
        config: &RoutingConfig,
        http: &reqwest::Client,
    ) -> Result<Owners, GatewayError> {
        let mut cache = self.0.lock().await;
        if let Some(cached) = cache.as_ref() {
            let ttl = if cached.value.is_ok() {
                SUCCESS_TTL
            } else {
                FAILURE_TTL
            };
            if cached.at.elapsed() < ttl {
                return cached.value.clone();
            }
        }
        let value = tokio::time::timeout(REFRESH_TIMEOUT, discover(config, http))
            .await
            .unwrap_or_else(|_| Err(GatewayError::bad_gateway("model catalog refresh timed out")));
        *cache = Some(Cached {
            at: Instant::now(),
            value: value.clone(),
        });
        value
    }

    pub(crate) async fn resolve(
        &self,
        config: &RoutingConfig,
        http: &reqwest::Client,
        requested: &str,
    ) -> Result<(usize, String), GatewayError> {
        if let Some(route) = config.custom(requested) {
            return Ok(route);
        }
        let catalog = self.catalog(config, http).await?;
        match catalog.get(requested).map(Vec::as_slice) {
            Some([index]) => Ok((*index, requested.to_string())),
            Some(_) => Err(GatewayError::invalid(format!("model `{requested}` has ambiguous upstream ownership; configure an explicit route"))),
            None => Err(GatewayError::not_found(format!("model `{requested}` is not owned by a configured upstream; configure a route or catalog"))),
        }
    }

    pub(crate) async fn listed(
        &self,
        config: &RoutingConfig,
        http: &reqwest::Client,
    ) -> Result<Vec<String>, GatewayError> {
        let catalog = self.catalog(config, http).await?;
        let mut ids = std::collections::BTreeSet::new();
        for (id, owners) in catalog {
            if config.custom(&id).is_some() || owners.len() == 1 {
                ids.insert(id);
            } else {
                return Err(GatewayError::invalid(format!(
                    "model `{id}` has ambiguous upstream ownership; configure an explicit route"
                )));
            }
        }
        ids.extend(
            config
                .routes
                .iter()
                .filter(|r| !r.pattern.contains('*'))
                .map(|r| r.pattern.clone()),
        );
        Ok(ids.into_iter().collect())
    }
}

async fn discover(config: &RoutingConfig, http: &reqwest::Client) -> Result<Owners, GatewayError> {
    let mut owners: Owners = BTreeMap::new();
    for (index, u) in config.upstreams.iter().enumerate() {
        for path in &u.catalog_paths {
            let initial = reqwest::Url::parse(&format!("{}{path}", u.catalog_url))
                .map_err(|_| GatewayError::internal("invalid catalog path"))?;
            let mut url = initial.clone();
            let mut seen = HashSet::new();
            for page in 0..MAX_PAGES {
                if !seen.insert(url.to_string()) {
                    return Err(GatewayError::bad_gateway("model catalog pagination loop"));
                }
                let mut request = http.get(url.clone());
                if let Some(key) = &u.api_key {
                    request = if u.protocol == UpstreamProtocol::Anthropic
                        && u.flavor != Flavor::Fireworks
                    {
                        request.header("x-api-key", key.expose_secret())
                    } else {
                        request.bearer_auth(key.expose_secret())
                    };
                }
                if u.protocol == UpstreamProtocol::Anthropic {
                    request = request.header("anthropic-version", "2023-06-01");
                }
                let started = Instant::now();
                let result = request.send().await;
                crate::request_log::record_http_result(
                    super::server::LOG_SERVICE,
                    "GET",
                    url.as_str(),
                    started,
                    &result,
                );
                let response = result.map_err(|_| {
                    GatewayError::bad_gateway(format!(
                        "model catalog unavailable for upstream `{}`",
                        u.name
                    ))
                })?;
                if !response.status().is_success() {
                    return Err(GatewayError::bad_gateway(format!(
                        "model catalog for upstream `{}` returned HTTP {}",
                        u.name,
                        response.status().as_u16()
                    )));
                }
                let mut bytes = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk
                        .map_err(|_| GatewayError::bad_gateway("model catalog body failed"))?;
                    if bytes.len() + chunk.len() > MAX_PAGE_BYTES {
                        return Err(GatewayError::bad_gateway(
                            "model catalog page exceeds 1 MiB",
                        ));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| GatewayError::bad_gateway("model catalog is not JSON"))?;
                let (ids, next) = parse_page(&value)?;
                for id in ids {
                    let entry = owners.entry(id).or_default();
                    if !entry.contains(&index) {
                        entry.push(index);
                    }
                    if owners.len() > MAX_MODELS {
                        return Err(GatewayError::bad_gateway(
                            "model catalog exceeds model limit",
                        ));
                    }
                }
                let Some((key, token)) = next else {
                    break;
                };
                if page + 1 == MAX_PAGES {
                    return Err(GatewayError::bad_gateway(
                        "model catalog exceeds pagination limit",
                    ));
                }
                url = initial.clone();
                let pairs: Vec<(String, String)> = url
                    .query_pairs()
                    .filter(|(k, _)| k != key)
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                url.set_query(None);
                url.query_pairs_mut()
                    .extend_pairs(pairs)
                    .append_pair(key, &token);
            }
        }
    }
    Ok(owners)
}

type CatalogPage = (Vec<String>, Option<(&'static str, String)>);

fn parse_page(value: &Value) -> Result<CatalogPage, GatewayError> {
    let array = [
        "data",
        "models",
        "modelSummaries",
        "inferenceProfileSummaries",
    ]
    .iter()
    .find_map(|key| value.get(key).and_then(Value::as_array))
    .or_else(|| value.as_array())
    .ok_or_else(|| GatewayError::bad_gateway("unsupported model catalog shape"))?;
    let mut ids = Vec::new();
    for model in array {
        let mut found = false;
        for key in [
            "id",
            "modelId",
            "modelArn",
            "inferenceProfileId",
            "inferenceProfileArn",
        ] {
            if let Some(id) = model
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                ids.push(id.to_string());
                found = true;
            }
        }
        if value.get("models").is_some() {
            if let Some(id) = model
                .get("name")
                .or_else(|| model.get("model"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                ids.push(id.to_string());
                found = true;
            }
        }
        if let Some(id) = model.as_str().filter(|s| !s.is_empty()) {
            ids.push(id.to_string());
            found = true;
        }
        if !found {
            return Err(GatewayError::bad_gateway(
                "model catalog entry has no supported identifier",
            ));
        }
    }
    for (field, query) in [
        ("nextToken", "nextToken"),
        ("next_token", "next_token"),
        ("next_page", "page"),
        ("next_page_token", "page_token"),
    ] {
        if let Some(token) = value.get(field).filter(|v| !v.is_null()) {
            let token = token.as_str().ok_or_else(|| {
                GatewayError::bad_gateway("model catalog cursor must be a string")
            })?;
            if !token.is_empty() {
                return Ok((ids, Some((query, token.to_string()))));
            }
        }
    }
    if value.get("has_more").is_some_and(|v| !v.is_boolean()) {
        return Err(GatewayError::bad_gateway(
            "model catalog has_more must be a boolean",
        ));
    }
    if ["next", "next_url", "nextPageToken"]
        .iter()
        .any(|key| value.get(key).is_some_and(|v| !v.is_null() && v != ""))
    {
        return Err(GatewayError::bad_gateway(
            "unsupported model catalog pagination; use a compatible cursor catalog",
        ));
    }
    if value.get("has_more").and_then(Value::as_bool) == Some(true) {
        let last = value
            .get("last_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| ids.last().cloned())
            .ok_or_else(|| GatewayError::bad_gateway("model catalog has_more without a cursor"))?;
        return Ok((ids, Some(("after", last))));
    }
    Ok((ids, None))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use serde_json::json;

    #[test]
    fn catalog_shapes_and_cursors_are_explicit() {
        for (page, expected) in [
            (json!({"data":[{"id":"chat"}]}), vec!["chat"]),
            (json!({"models":[{"name":"local"}]}), vec!["local"]),
            (
                json!({"modelSummaries":[{"modelId":"native","modelArn":"arn:example:model"}]}),
                vec!["native", "arn:example:model"],
            ),
            (
                json!({"inferenceProfileSummaries":[{"inferenceProfileId":"profile","inferenceProfileArn":"arn:example:profile"}]}),
                vec!["profile", "arn:example:profile"],
            ),
            (json!(["one", "two"]), vec!["one", "two"]),
        ] {
            assert_eq!(parse_page(&page).unwrap().0, expected);
        }
        for (page, key) in [
            (json!({"data":[{"id":"last"}],"has_more":true}), "after"),
            (
                json!({"modelSummaries":[],"nextToken":"token"}),
                "nextToken",
            ),
            (json!({"data":[],"next_page_token":"token"}), "page_token"),
        ] {
            assert_eq!(parse_page(&page).unwrap().1.unwrap().0, key);
        }
        for page in [
            json!({}),
            json!({"data":[{}]}),
            json!({"data":[],"has_more":true}),
            json!({"data":[],"nextToken":2}),
            json!({"data":[],"next":"https://example.invalid/next"}),
            json!({"data":[],"has_more":"yes"}),
        ] {
            assert!(parse_page(&page).is_err(), "{page}");
        }
    }

    #[test]
    fn names_paths_credential_sources_and_legacy_mixing_are_validated() {
        for upstreams in [
            json!([{"name":"x","url":"https://example.invalid"},{"name":"x","url":"https://example.invalid"}]),
            json!([{"name":"x","url":"https://user:password@example.invalid"}]),
            json!([{"name":"x","url":"https://example.invalid#fragment"}]),
            json!([{"name":"x","url":"https://example.invalid","catalog_paths":["//other.example.invalid/models"]}]),
            json!([{"name":"x","url":"https://example.invalid","catalog_paths":["/\\other.example.invalid/models"]}]),
            json!([{"name":"x","url":"https://example.invalid","api_key_env":"OMNI_DEV_GATEWAY_TOKEN"}]),
            json!([{"name":"x","url":"https://example.invalid","api_key":"inline-secret-not-supported"}]),
            json!([{"name":"x","url":"https://example.invalid","api_key_env":"UNSET_KEY"}]),
        ] {
            let env = MapEnv::new().with(UPSTREAMS_VAR, &upstreams.to_string());
            assert!(
                GatewayConfig::from_env_with(&env, &super::super::ConfigOverrides::default())
                    .is_err()
            );
        }
        let env = MapEnv::new()
            .with(
                UPSTREAMS_VAR,
                r#"[{"name":"x","url":"http://localhost:1"}]"#,
            )
            .with(super::super::config::UPSTREAM_URL_VAR, "http://localhost:2");
        assert!(
            GatewayConfig::from_env_with(&env, &super::super::ConfigOverrides::default()).is_err()
        );
        let env = MapEnv::new()
            .with(
                UPSTREAMS_VAR,
                r#"[{"name":"x","url":"http://localhost:1"}]"#,
            )
            .with(ROUTES_VAR, r#"[{"pattern":"a","upstream":"unknown"}]"#);
        assert!(RoutingConfig::from_env(&env).is_err());
    }

    #[test]
    fn named_key_uses_the_existing_file_secret_resolver_and_redacts_debug() {
        let (_dir, path) = crate::test_support::env::secret_file("named-private-key\n");
        let env = MapEnv::new()
            .with(
                UPSTREAMS_VAR,
                r#"[{"name":"x","url":"http://localhost:1","api_key_env":"OPENAI_API_KEY"}]"#,
            )
            .with("OPENAI_API_KEY_FILE", &path);
        let config = RoutingConfig::from_env(&env).unwrap();
        assert_eq!(
            config.upstreams[0]
                .api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "named-private-key"
        );
        assert!(!format!("{config:?}").contains("named-private-key"));
        assert!(RoutingConfig::from_env(&env.with("OPENAI_API_KEY", "conflict")).is_err());
    }

    #[test]
    fn registered_provider_auth_token_is_not_confused_with_inbound_gateway_token() {
        let env = MapEnv::new()
            .with(UPSTREAMS_VAR, r#"[{"name":"native","url":"http://localhost:1","protocol":"anthropic","api_key_env":"ANTHROPIC_AUTH_TOKEN"}]"#)
            .with("ANTHROPIC_AUTH_TOKEN", "provider-only-token");
        let config = RoutingConfig::from_env(&env).unwrap();
        assert_eq!(
            config.upstreams[0]
                .api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "provider-only-token"
        );
    }

    #[tokio::test]
    async fn success_and_failure_cache_expire_without_stale_ownership() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let env = MapEnv::new().with(
            UPSTREAMS_VAR,
            &json!([{"name":"x","url":server.uri()}]).to_string(),
        );
        let config = RoutingConfig::from_env(&env).unwrap();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let discovery = Discovery::default();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"old"}]})))
            .expect(1)
            .mount(&server)
            .await;
        assert!(discovery.resolve(&config, &http, "old").await.is_ok());
        assert!(discovery.resolve(&config, &http, "old").await.is_ok());
        discovery.0.lock().await.as_mut().unwrap().at =
            Instant::now().checked_sub(SUCCESS_TTL).unwrap();
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            discovery
                .resolve(&config, &http, "old")
                .await
                .unwrap_err()
                .status
                .as_u16(),
            502
        );
        assert_eq!(
            discovery
                .resolve(&config, &http, "old")
                .await
                .unwrap_err()
                .status
                .as_u16(),
            502
        );
        discovery.0.lock().await.as_mut().unwrap().at =
            Instant::now().checked_sub(FAILURE_TTL).unwrap();
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"new"}]})))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            discovery
                .resolve(&config, &http, "old")
                .await
                .unwrap_err()
                .status
                .as_u16(),
            404
        );
        assert!(discovery.resolve(&config, &http, "new").await.is_ok());
    }
}
