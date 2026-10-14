//! Passthrough to an Anthropic-compatible upstream (`--upstream-protocol
//! anthropic`).
//!
//! No translation: the body goes upstream as the client sent it, with the
//! model mapped, the gateway token swapped for the upstream key, and the
//! request logged. For the Fireworks flavor, the fields Fireworks documents
//! as unsupported on its Anthropic surface (as of 2026-10-14) are stripped
//! and reported as ignored, since Fireworks would otherwise reject the
//! request. `count_tokens` is forwarded, and answered with the gateway's
//! estimate when the upstream has no such endpoint (Fireworks returns 404).

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Map, Value};

use super::anthropic::MessagesRequest;
use super::error::GatewayError;
use super::flavor::Flavor;
use super::policy::PolicyReport;
use super::server::AppState;

/// The Anthropic API version sent when the client gave none.
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Tool fields Fireworks' Anthropic surface rejects.
const FIREWORKS_UNSUPPORTED_TOOL_FIELDS: &[&str] = &[
    "eager_input_streaming",
    "cache_control",
    "allowed_callers",
    "input_examples",
];

/// Maps the model and strips what the upstream flavor cannot take.
pub(crate) fn prepare(
    body: &mut Map<String, Value>,
    state: &AppState,
    report: &mut PolicyReport,
) -> Result<String, GatewayError> {
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::invalid("model: field required"))?
        .to_string();
    let model = state.config.models.resolve(&requested)?;
    body.insert("model".into(), Value::String(model.clone()));
    if state.config.flavor == Flavor::Fireworks {
        if body.remove("inference_geo").is_some() {
            report.ignore("inference_geo");
        }
        if let Some(Value::Array(tools)) = body.get_mut("tools") {
            for tool in tools.iter_mut().filter_map(Value::as_object_mut) {
                for field in FIREWORKS_UNSUPPORTED_TOOL_FIELDS {
                    if tool.remove(*field).is_some() {
                        report.ignore(format!("tools.{field}"));
                    }
                }
            }
        }
    }
    Ok(model)
}

/// The Anthropic headers forwarded upstream.
fn forwarded(headers: &HeaderMap) -> Vec<(&'static str, String)> {
    let version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(DEFAULT_ANTHROPIC_VERSION)
        .to_string();
    let mut out = vec![("anthropic-version", version)];
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        out.push(("anthropic-beta", beta.to_string()));
    }
    out
}

/// `POST /v1/messages`, passed through.
pub(crate) async fn messages(
    state: &AppState,
    headers: &HeaderMap,
    mut body: Map<String, Value>,
) -> Result<Response, GatewayError> {
    let mut report = PolicyReport::default();
    let model = prepare(&mut body, state, &mut report)?;
    state.note_ignored(&report);
    let permit = state.permit()?;
    let url = format!("{}/v1/messages", state.config.upstream_url);
    let upstream = state
        .send(&url, &Value::Object(body), &forwarded(headers))
        .await?;
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::OK);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let stream = upstream.bytes_stream().map(move |chunk| {
        let _held = &permit;
        chunk
    });
    let mut response = (status, Body::from_stream(stream)).into_response();
    let response_headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response_headers.insert(axum::http::header::CONTENT_TYPE, value);
    }
    super::server::annotate(response_headers, &report, &model);
    Ok(response)
}

/// `POST /v1/messages/count_tokens`, passed through, falling back to the
/// estimate when the upstream has no such endpoint.
pub(crate) async fn count_tokens(
    state: &AppState,
    headers: &HeaderMap,
    mut body: Map<String, Value>,
) -> Result<Response, GatewayError> {
    let original = Value::Object(body.clone());
    let mut report = PolicyReport::default();
    prepare(&mut body, state, &mut report)?;
    let _permit = state.permit()?;
    let url = format!("{}/v1/messages/count_tokens", state.config.upstream_url);
    match state
        .send(&url, &Value::Object(body), &forwarded(headers))
        .await
    {
        Ok(upstream) => {
            let value: Value = upstream.json().await.map_err(|e| {
                GatewayError::bad_gateway(format!("unreadable count_tokens reply: {e}"))
            })?;
            Ok(axum::Json(value).into_response())
        }
        Err(err) if matches!(err.upstream_status, Some(404 | 405)) => {
            let request: MessagesRequest = serde_json::from_value(original)
                .map_err(|e| GatewayError::invalid(e.to_string()))?;
            Ok(super::server::estimate_response(&request))
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn forwarded_headers_default_the_version() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            forwarded(&headers),
            vec![("anthropic-version", "2023-06-01".to_string())]
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2024-01-01"));
        headers.insert("anthropic-beta", HeaderValue::from_static("x"));
        assert_eq!(
            forwarded(&headers),
            vec![
                ("anthropic-version", "2024-01-01".to_string()),
                ("anthropic-beta", "x".to_string())
            ]
        );
    }

    #[test]
    fn fireworks_strips_documented_unsupported_fields() {
        let state = super::super::server::tests::state("fireworks", "anthropic");
        let mut body = json!({"model": "claude-x", "inference_geo": "us", "tools": [
            {"name": "t", "input_schema": {}, "cache_control": {"type": "ephemeral"}, "input_examples": []}
        ]})
        .as_object()
        .unwrap()
        .clone();
        let mut report = PolicyReport::default();
        let model = prepare(&mut body, &state, &mut report).unwrap();
        assert_eq!(model, "up-default");
        assert_eq!(body["model"], "up-default");
        assert!(body.get("inference_geo").is_none());
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(report.ignored.contains("inference_geo"));
        assert!(report.ignored.contains("tools.input_examples"));

        let mut missing = Map::new();
        assert!(prepare(&mut missing, &state, &mut report).is_err());
    }
}
