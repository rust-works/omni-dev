//! Native Claude-on-Bedrock transport. Like the existing Bedrock AI client,
//! this uses a configured URL and bearer token, not an AWS SigV4 credential chain.
//! Native content (including provider thinking signatures) is never lowered.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::error::GatewayError;
use super::policy::PolicyReport;
use super::server::{annotate, AppState, Permit};
use super::sse::encode;

const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) async fn messages(
    state: &AppState,
    headers: &HeaderMap,
    mut body: Map<String, Value>,
) -> Result<Response, GatewayError> {
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::invalid("model: field required"))?
        .to_string();
    let model = state.config.models.resolve(&requested)?;
    if model.is_empty() || matches!(model.as_str(), "." | "..") {
        return Err(GatewayError::invalid("invalid Bedrock model identifier"));
    }
    let streaming = match body.remove("stream") {
        None => false,
        Some(Value::Bool(b)) => b,
        _ => return Err(GatewayError::invalid("stream must be a boolean")),
    };
    body.remove("model");
    body.insert("anthropic_version".into(), json!("bedrock-2023-05-31"));
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        body.insert(
            "anthropic_beta".into(),
            json!(beta
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()),
        );
    }
    let encoded = url::form_urlencoded::byte_serialize(model.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    let endpoint = if streaming {
        "invoke-with-response-stream"
    } else {
        "invoke"
    };
    let url = format!("{}/model/{encoded}/{endpoint}", state.config.upstream_url);
    let accept = if streaming {
        "application/vnd.amazon.eventstream"
    } else {
        "application/json"
    };
    let permit = state.permit()?;
    let upstream = state
        .send(
            &url,
            &Value::Object(body),
            &[
                ("accept", accept.into()),
                ("x-amzn-bedrock-accept", "application/json".into()),
            ],
        )
        .await?;
    if streaming {
        let content_type = upstream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("application/vnd.amazon.eventstream") {
            return Err(GatewayError::bad_gateway(
                "Bedrock returned a non-eventstream streaming response",
            ));
        }
        return Ok(stream_response(state, upstream, permit, requested, &model));
    }
    let mut reply: Value = upstream
        .json()
        .await
        .map_err(|_| GatewayError::bad_gateway("unreadable Bedrock reply"))?;
    if !reply.is_object() {
        return Err(GatewayError::bad_gateway("Bedrock reply is not an object"));
    }
    reply["model"] = json!(requested);
    let mut response = axum::Json(reply).into_response();
    annotate(response.headers_mut(), &PolicyReport::default(), &model);
    Ok(response)
}

fn stream_response(
    state: &AppState,
    upstream: reqwest::Response,
    permit: Permit,
    client_model: String,
    model: &str,
) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let state = state.clone();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = pump(upstream, &tx, &client_model).await {
            state.record_stream_error(&error);
            let _ = tx.send(encode(&json!({"type":"error", "error":{"type":error.kind.as_str(), "message":error.message}}))).await;
        }
    });
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), rx))
    });
    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    annotate(response.headers_mut(), &PolicyReport::default(), model);
    response
}

async fn pump(
    upstream: reqwest::Response,
    tx: &mpsc::Sender<Bytes>,
    client_model: &str,
) -> Result<(), GatewayError> {
    pump_chunks(upstream.bytes_stream(), tx, client_model).await
}

async fn pump_chunks(
    chunks: impl futures::Stream<Item = Result<Bytes, reqwest::Error>>,
    tx: &mpsc::Sender<Bytes>,
    client_model: &str,
) -> Result<(), GatewayError> {
    let mut stream = std::pin::pin!(chunks);
    let mut decoder = EventStream::default();
    let mut terminal = None;
    loop {
        let Ok(next) =
            tokio::time::timeout(std::time::Duration::from_secs(15), stream.next()).await
        else {
            if tx.send(encode(&json!({"type":"ping"}))).await.is_err() {
                return Ok(());
            }
            continue;
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| GatewayError::bad_gateway("Bedrock event stream broke"))?;
        for mut event in decoder.push(&chunk)? {
            if terminal.is_some() {
                return Err(GatewayError::bad_gateway(
                    "Bedrock sent events after message_stop",
                ));
            }
            if event["type"] == "message_start" {
                let message = event
                    .get_mut("message")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| {
                        GatewayError::bad_gateway("Bedrock message_start has no message object")
                    })?;
                message.insert("model".into(), json!(client_model));
            }
            if event["type"] == "message_stop" {
                // EOF and any trailing frames still need validation. Emitting
                // success now could make a subsequent error invisible to a
                // client that finalizes immediately on this terminal event.
                terminal = Some(event);
                continue;
            }
            if tx.send(encode(&event)).await.is_err() {
                return Ok(());
            }
        }
    }
    decoder.finish()?;
    let terminal = terminal
        .ok_or_else(|| GatewayError::bad_gateway("Bedrock stream ended before message_stop"))?;
    let _ = tx.send(encode(&terminal)).await;
    Ok(())
}

/// Incremental AWS event-stream framing, including both CRCs and typed headers.
#[derive(Default)]
pub(crate) struct EventStream {
    buffer: Vec<u8>,
}

impl EventStream {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, GatewayError> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buffer.len() < 12 {
                break;
            }
            let total = number(&self.buffer[0..4]) as usize;
            let headers = number(&self.buffer[4..8]) as usize;
            if !(16..=MAX_FRAME).contains(&total)
                || headers > total - 16
                || headers > 128 * 1024
                || crc32(&self.buffer[..8]) != number(&self.buffer[8..12])
            {
                return Err(GatewayError::bad_gateway(
                    "invalid Bedrock event-stream prelude",
                ));
            }
            if self.buffer.len() < total {
                break;
            }
            let frame = &self.buffer[..total];
            if crc32(&frame[..total - 4]) != number(&frame[total - 4..]) {
                return Err(GatewayError::bad_gateway(
                    "Bedrock event-stream CRC mismatch",
                ));
            }
            let metadata = parse_headers(&frame[12..12 + headers])?;
            let payload = &frame[12 + headers..total - 4];
            if metadata.get(":message-type").map(String::as_str) != Some("event")
                || metadata.get(":event-type").map(String::as_str) != Some("chunk")
            {
                let kind = metadata
                    .get(":exception-type")
                    .or_else(|| metadata.get(":event-type"))
                    .map_or("unknown", String::as_str);
                let status = match kind {
                    "throttlingException" => 429,
                    "modelTimeoutException" => 504,
                    "validationException" => 400,
                    "serviceUnavailableException" => 503,
                    _ => 502,
                };
                return Err(GatewayError::from_upstream(
                    status,
                    &format!("Bedrock stream error: {kind}"),
                    None,
                ));
            }
            let wrapper: Value = serde_json::from_slice(payload)
                .map_err(|_| GatewayError::bad_gateway("Bedrock event payload is not JSON"))?;
            let event = if let Some(encoded) = wrapper.get("bytes").and_then(Value::as_str) {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| GatewayError::bad_gateway("invalid Bedrock event base64"))?;
                serde_json::from_slice::<Value>(&decoded)
                    .map_err(|_| GatewayError::bad_gateway("decoded Bedrock event is not JSON"))?
            } else {
                wrapper
            };
            if event["type"] == "error" {
                return Err(GatewayError::bad_gateway("Bedrock emitted an error event"));
            }
            if event.get("type").and_then(Value::as_str).is_none() {
                return Err(GatewayError::bad_gateway("Bedrock event has no type"));
            }
            out.push(event);
            self.buffer.drain(..total);
        }
        Ok(out)
    }

    pub(crate) fn finish(&self) -> Result<(), GatewayError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(GatewayError::bad_gateway(
                "truncated Bedrock event-stream frame",
            ))
        }
    }
}

fn number(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn parse_headers(
    mut bytes: &[u8],
) -> Result<std::collections::BTreeMap<String, String>, GatewayError> {
    let invalid = || GatewayError::bad_gateway("invalid Bedrock event-stream headers");
    let mut out = std::collections::BTreeMap::new();
    while !bytes.is_empty() {
        let len = usize::from(bytes[0]);
        bytes = &bytes[1..];
        if bytes.len() < len + 1 {
            return Err(invalid());
        }
        let name = std::str::from_utf8(&bytes[..len])
            .map_err(|_| invalid())?
            .to_string();
        let kind = bytes[len];
        bytes = &bytes[len + 1..];
        let (skip, offset) = match kind {
            0 | 1 => (0, 0),
            2 => (1, 0),
            3 => (2, 0),
            4 => (4, 0),
            5 | 8 => (8, 0),
            9 => (16, 0),
            6 | 7 if bytes.len() >= 2 => (usize::from(u16::from_be_bytes([bytes[0], bytes[1]])), 2),
            _ => return Err(invalid()),
        };
        if bytes.len() < skip + offset {
            return Err(invalid());
        }
        if kind == 7 {
            let value =
                std::str::from_utf8(&bytes[offset..offset + skip]).map_err(|_| invalid())?;
            if out.insert(name, value.to_string()).is_some() {
                return Err(invalid());
            }
        } else if name.starts_with(':') {
            return Err(invalid());
        }
        bytes = &bytes[offset + skip..];
    }
    if !out.contains_key(":message-type") {
        return Err(invalid());
    }
    Ok(out)
}

// IEEE CRC-32 (AWS event-stream uses the same checksum as gzip).
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gateway::routing_tests::frame;

    #[tokio::test]
    async fn terminal_event_waits_for_eof_and_fragmented_tail_validation() {
        let start = json!({"type":"message_start","message":{"model":"native"}});
        let stop = json!({"type":"message_stop"});
        let ping = frame(&json!({"type":"ping"}), "chunk");
        let mut corrupt = ping.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        let truncated = ping[..ping.len() - 1].to_vec();
        for (name, tail, succeeds) in [
            ("valid", vec![], true),
            ("post-stop-event", ping, false),
            ("post-stop-crc", corrupt, false),
            ("post-stop-truncated", truncated, false),
        ] {
            // A stop arrives in its own HTTP chunk, followed by a tail
            // fragmented at every byte. This covers late errors that cannot
            // be discovered while processing the terminal event's chunk.
            let chunks = [frame(&start, "chunk"), frame(&stop, "chunk")]
                .into_iter()
                .chain(tail.into_iter().map(|byte| vec![byte]))
                .map(|chunk| Ok(Bytes::from(chunk)));
            let (tx, mut rx) = mpsc::channel(64);
            let result = pump_chunks(futures::stream::iter(chunks), &tx, "alias").await;
            assert_eq!(result.is_ok(), succeeds, "{name}");
            drop(tx);
            let mut text = String::new();
            while let Some(bytes) = rx.recv().await {
                text.push_str(std::str::from_utf8(&bytes).unwrap());
            }
            assert!(text.contains("event: message_start"), "{name}: {text}");
            assert_eq!(
                text.contains("event: message_stop"),
                succeeds,
                "{name}: {text}"
            );
            if succeeds {
                assert!(text.ends_with("data: {\"type\":\"message_stop\"}\n\n"));
            }
        }
    }

    #[tokio::test]
    async fn terminal_event_is_not_emitted_while_waiting_for_eof() {
        let (chunks_tx, chunks_rx) = mpsc::channel(4);
        let chunks = futures::stream::unfold(chunks_rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        });
        let (tx, mut rx) = mpsc::channel(4);
        let task = tokio::spawn(async move { pump_chunks(chunks, &tx, "alias").await });
        chunks_tx
            .send(Ok(Bytes::from(frame(&json!({"type":"ping"}), "chunk"))))
            .await
            .unwrap();
        chunks_tx
            .send(Ok(Bytes::from(frame(
                &json!({"type":"message_stop"}),
                "chunk",
            ))))
            .await
            .unwrap();
        assert!(std::str::from_utf8(&rx.recv().await.unwrap())
            .unwrap()
            .contains("event: ping"));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
        drop(chunks_tx);
        task.await.unwrap().unwrap();
        assert!(std::str::from_utf8(&rx.recv().await.unwrap())
            .unwrap()
            .contains("event: message_stop"));
        assert!(rx.recv().await.is_none());
    }
}
