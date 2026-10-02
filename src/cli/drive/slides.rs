//! Google Slides read and guarded replacement commands.
pub(crate) mod info;
pub(crate) mod read;
pub(crate) mod write;
use crate::drive::client::DriveClient;
use anyhow::Result;
use clap::{Parser, Subcommand};

/// Reads Slides objects and replaces text on ordinary slides.
#[derive(Parser)]
pub struct SlidesCommand {
    /// Slides subcommand.
    #[command(subcommand)]
    pub command: SlidesSubcommands,
}
/// Typed Slides verbs; no arbitrary batch or object deletion.
#[derive(Subcommand)]
pub enum SlidesSubcommands {
    /// Shows title, revision and ordinary slide object IDs.
    Info(info::InfoCommand),
    /// Reads shapes, table cells, groups and speaker notes with object IDs.
    Read(read::ReadCommand),
    /// Replaces literal text on ordinary slides, gated by slides-write and
    /// revision control. Requires drive.file or drive OAuth scope.
    Replace(write::ReplaceCommand),
}
impl SlidesCommand {
    /// Dispatches using the shared Drive OAuth session.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.command {
            SlidesSubcommands::Info(cmd) => cmd.execute(client).await,
            SlidesSubcommands::Read(cmd) => cmd.execute(client).await,
            SlidesSubcommands::Replace(cmd) => cmd.execute(client).await,
        }
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::slides::client::{test_support::mock_clients, SLIDES_API_URL};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Runs each verb through `SlidesCommand::execute`'s own match, not the
    /// leaf's, so every arm is reached — and against a live server, so a
    /// verb wired to the wrong host would fail the request-count check.
    #[tokio::test]
    async fn every_verb_dispatches_to_its_leaf_against_the_slides_host() {
        let guard = crate::drive::test_support::EnvGuard::take();
        // No settings: every account is default-deny, so `replace` below is
        // refused before any Slides read or write.
        let _home = guard.clear_credentials();
        let server = MockServer::start().await;
        let (drive, _) = mock_clients(&server).await;
        std::env::set_var(SLIDES_API_URL, server.uri());
        Mock::given(method("GET"))
            .and(path("/drive/v3/files/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"p1","name":"Deck","mimeType":crate::drive::types::GOOGLE_SLIDES_MIME_TYPE,"version":"1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/presentations/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"title":"Deck","revisionId":"r","slides":[{"objectId":"s1"}]}),
            ))
            .mount(&server)
            .await;

        for args in [
            vec!["slides", "info", "p1", "-o", "json"],
            vec!["slides", "read", "p1", "-o", "json"],
            vec![
                "slides",
                "replace",
                "p1",
                "--search",
                "a",
                "--replace",
                "b",
                "--dry-run",
                "-o",
                "json",
            ],
        ] {
            SlidesCommand::try_parse_from(args)
                .unwrap()
                .execute(&drive)
                .await
                .unwrap();
        }

        let slides_reads = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/v1/presentations/p1")
            .count();
        assert_eq!(slides_reads, 2, "info and read hit the Slides host");
    }
}
