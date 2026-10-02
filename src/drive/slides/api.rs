//! Slides endpoints. Full unmasked reads preserve nested groups and notes.
use super::client::SlidesClient;
use super::types::Presentation;
use super::write_types::{
    BatchUpdatePresentationRequest, BatchUpdatePresentationResponse, SlidesRequest,
};
use crate::drive::api_client::GoogleApiClient;
use crate::drive::error::DriveError;
use crate::drive::files_api::{append_write_scope_hint, WriteCapability};
use anyhow::{Context, Result};
use url::Url;

const MAX_PRESENTATION_BYTES: usize = 64 * 1024 * 1024;

/// Slides API facade over a shared OAuth session.
#[derive(Debug)]
pub struct SlidesApi<'a> {
    client: &'a SlidesClient,
}
impl<'a> SlidesApi<'a> {
    /// Wraps an existing client.
    pub const fn new(client: &'a SlidesClient) -> Self {
        Self { client }
    }

    /// Fetches every slide, template and notes page without a fields mask.
    pub async fn get_presentation(&self, id: &str) -> Result<Presentation> {
        let url = build_presentation_get_url(self.client.base_url(), id)?;
        let response = self.client.transport().get_json(url.as_str()).await?;
        if !response.status().is_success() {
            return Err(GoogleApiClient::response_to_error("Slides", response)
                .await
                .into());
        }
        let bytes = bounded_body(response, MAX_PRESENTATION_BYTES).await?;
        serde_json::from_slice(&bytes).context("Failed to parse Slides presentation")
    }

    /// One request with a mandatory revision assertion, behind the Drive gate.
    /// An unreadable 2xx reply still means the mutation was applied (issue #2021).
    pub(in crate::drive) async fn batch_update(
        &self,
        id: &str,
        request: SlidesRequest,
        revision: &str,
    ) -> Result<Result<BatchUpdatePresentationResponse, String>> {
        anyhow::ensure!(
            !revision.is_empty(),
            "Slides requires a nonempty revision id"
        );
        let mut url = GoogleApiClient::api_url(self.client.base_url(), "/v1/presentations")?;
        GoogleApiClient::push_path_segments(&mut url, &[&format!("{id}:batchUpdate")])?;
        let body = BatchUpdatePresentationRequest::new(request, revision);
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_success_response(response, "Failed to parse Slides batchUpdate response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }
}

fn build_presentation_get_url(base: &str, id: &str) -> Result<Url> {
    let mut url =
        GoogleApiClient::api_url(base, "/v1/presentations").context("Invalid Slides base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[id])?;
    Ok(url)
}

// Enforce the actual decoded-body bound even with chunked transfer encoding.
async fn bounded_body(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    anyhow::ensure!(
        response.content_length().is_none_or(|n| n <= cap as u64),
        "Slides presentation exceeds the {cap} byte cap; use `omni-dev drive read <presentation-id> --content` for a text export"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Failed to read Slides presentation")?
    {
        anyhow::ensure!(
            chunk.len() <= cap.saturating_sub(bytes.len()),
            "Slides presentation exceeds the {cap} byte cap; use `omni-dev drive read <presentation-id> --content` for a text export"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Conservative diagnostic only: an unfamiliar revision error remains Failed.
/// The 400 status is documented; this message marker is not live-verified.
pub(in crate::drive) fn is_stale_revision(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<DriveError>(), Some(DriveError::ApiRequestFailed { status: 400, body, .. }) if body.to_ascii_lowercase().contains("does not match the latest revision"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn get_url_is_encoded_and_unmasked() {
        let url = build_presentation_get_url("https://slides.googleapis.com", "a/b?c").unwrap();
        assert_eq!(url.path(), "/v1/presentations/a%2Fb%3Fc");
        assert_eq!(url.query(), None);
    }
    #[tokio::test]
    async fn oversized_body_is_refused() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("123456789"))
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();
        assert!(bounded_body(response, 8)
            .await
            .unwrap_err()
            .to_string()
            .contains("byte cap"));
    }
    #[tokio::test]
    async fn chunked_body_cannot_bypass_the_decoded_size_bound() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n12345\r\n5\r\n67890\r\n0\r\n\r\n").await.unwrap();
        });
        let response = reqwest::get(format!("http://{addr}")).await.unwrap();
        assert_eq!(response.content_length(), None);
        assert!(bounded_body(response, 8)
            .await
            .unwrap_err()
            .to_string()
            .contains("byte cap"));
        server.await.unwrap();
    }

    #[test]
    fn omitted_zero_reply_count_is_zero() {
        let response: BatchUpdatePresentationResponse =
            serde_json::from_str(r#"{"replies":[{"replaceAllText":{}}]}"#).unwrap();
        assert_eq!(response.occurrences_changed_for_replace(), 0);
    }
}
