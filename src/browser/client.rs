//! A thin client for a running bridge's HTTP control plane.
//!
//! Both `bridge request` and `bridge harvest ...` are clients of the same
//! control-plane endpoint (`POST /__bridge/request`): they serialise a
//! [`ControlRequest`], attach the bearer token and `X-Omni-Bridge` header, and
//! read back a [`ResponseEnvelope`]. This type centralises that auth/endpoint
//! construction so every caller exercises the same dispatch path rather than
//! opening its own socket.

use anyhow::{bail, Context, Result};

use crate::browser::auth;
use crate::browser::protocol::{ControlRequest, ResponseEnvelope};

/// A client bound to one running bridge's control plane.
pub struct BridgeClient {
    control_port: u16,
    token: String,
    http: reqwest::Client,
}

impl BridgeClient {
    /// Builds a client targeting the control plane on `control_port`, using
    /// `token` for bearer auth.
    #[must_use]
    pub fn new(control_port: u16, token: String) -> Self {
        Self {
            control_port,
            token,
            http: reqwest::Client::new(),
        }
    }

    /// The control-plane request endpoint for this client.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/__bridge/request", self.control_port)
    }

    /// Builds the authenticated `POST /__bridge/request` request for `payload`,
    /// without sending it. Callers that need the raw streamed response (e.g. the
    /// `request --stream` path) drive this builder themselves.
    pub fn request_builder(&self, payload: &ControlRequest) -> reqwest::RequestBuilder {
        self.http
            .post(self.endpoint())
            .bearer_auth(&self.token)
            .header(auth::BRIDGE_HEADER, auth::BRIDGE_HEADER_VALUE)
            .json(payload)
    }

    /// Sends a buffered request and returns the parsed response envelope.
    ///
    /// Errors when the bridge is unreachable, returns a non-success status, or
    /// returns a body that is not a [`ResponseEnvelope`].
    pub async fn send(&self, payload: &ControlRequest) -> Result<ResponseEnvelope> {
        let endpoint = self.endpoint();
        let resp = self
            .request_builder(payload)
            .send()
            .await
            .with_context(|| format!("Failed to reach bridge at {endpoint} (is it running?)"))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("Failed to read bridge response")?;
        if !status.is_success() {
            bail!("bridge returned {status}: {text}");
        }
        serde_json::from_str::<ResponseEnvelope>(&text)
            .context("bridge returned an unparseable response envelope")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// A minimal control request for driving the client.
    fn req() -> ControlRequest {
        ControlRequest {
            url: "/x".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            target: None,
            allow_origin: None,
            credentials: None,
            encoding: None,
        }
    }

    #[test]
    fn endpoint_targets_loopback_control_plane() {
        let client = BridgeClient::new(9998, "tok".to_string());
        assert_eq!(client.endpoint(), "http://127.0.0.1:9998/__bridge/request");
    }

    /// Mounts a control plane on a mock server and returns a client pointed at it.
    async fn client_for(server: &MockServer) -> BridgeClient {
        BridgeClient::new(server.address().port(), "tok".to_string())
    }

    #[tokio::test]
    async fn send_returns_the_parsed_envelope_on_success() {
        let server = MockServer::start().await;
        let envelope = ResponseEnvelope {
            id: 1,
            status: 200,
            headers: BTreeMap::new(),
            body: "hello".to_string(),
            encoding: None,
        };
        Mock::given(method("POST"))
            .and(path("/__bridge/request"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&envelope))
            .mount(&server)
            .await;

        let env = client_for(&server).await.send(&req()).await.unwrap();
        assert_eq!(env.status, 200);
        assert_eq!(env.body, "hello");
    }

    #[tokio::test]
    async fn send_errors_on_non_success_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/__bridge/request"))
            .respond_with(ResponseTemplate::new(503).set_body_string("no browser"))
            .mount(&server)
            .await;

        let err = client_for(&server).await.send(&req()).await.unwrap_err();
        assert!(err.to_string().contains("503"), "got: {err}");
    }

    #[tokio::test]
    async fn send_errors_on_unparseable_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/__bridge/request"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not an envelope"))
            .mount(&server)
            .await;

        let err = client_for(&server).await.send(&req()).await.unwrap_err();
        assert!(err.to_string().contains("unparseable"), "got: {err}");
    }

    #[tokio::test]
    async fn send_errors_when_bridge_unreachable() {
        // Port 0 never has a listener, so the transport fails fast.
        let err = BridgeClient::new(0, "tok".to_string())
            .send(&req())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to reach bridge"),
            "got: {err}"
        );
    }
}
