//! Integration tests for the `adf-schema-drift` binary.
//!
//! Spawns the compiled binary as a subprocess with `OMNI_DEV_ADF_SCHEMA_LATEST_URL`
//! pointing at a `wiremock` server, exercising end-to-end the fetch flow,
//! the `process_report` orchestration in `main`, and the `$GITHUB_OUTPUT`
//! signalling.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::Command;

use omni_dev::atlassian::adf_schema::drift::NPM_LATEST_URL_ENV;
use serde_json::json;

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

/// Build a minimal upstream-shaped JSON schema with one parent + one leaf.
/// Drift is "version_changed" (we feed an obviously-different version) but
/// no content drift if the local map is empty for this parent. We accept
/// content drift in this test — what we're verifying is the binary's
/// orchestration, not the schema diff specifics.
fn minimal_full_json() -> serde_json::Value {
    json!({
        "definitions": {
            "paragraph_node": {
                "properties": {
                    "type": {"const": "paragraph"},
                    "content": {
                        "type": "array",
                        "items": {"$ref": "#/definitions/text_node"}
                    }
                }
            },
            "text_node": {"properties": {"type": {"const": "text"}}}
        }
    })
}

#[tokio::test]
async fn binary_fetches_via_env_url_and_writes_outputs() {
    let server = wiremock::MockServer::start().await;
    let full = minimal_full_json();
    let tarball = build_synthetic_tarball(&[(
        "package/dist/json-schema/v1/full.json",
        serde_json::to_vec(&full).unwrap().as_slice(),
    )]);
    let tarball_url = format!("{}/-/adf-schema-fixture.tgz", server.uri());

    wiremock::Mock::given(wiremock::matchers::path("/latest"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "version": "99.0.0",
            "dist": {"tarball": tarball_url}
        })))
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::path("/-/adf-schema-fixture.tgz"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;

    let workdir = tempfile::tempdir().unwrap();
    let github_output = workdir.path().join("github_output.txt");
    // Pre-create so the binary's `append` mode finds an existing file.
    std::fs::File::create(&github_output)
        .unwrap()
        .write_all(b"")
        .unwrap();

    let exe = env!("CARGO_BIN_EXE_adf-schema-drift");
    let output = Command::new(exe)
        .args([
            "--format",
            "both",
            "--output-dir",
            workdir.path().to_str().unwrap(),
        ])
        .env(NPM_LATEST_URL_ENV, format!("{}/latest", server.uri()))
        .env("GITHUB_OUTPUT", &github_output)
        .output()
        .expect("spawn binary");

    assert!(
        output.status.success(),
        "binary failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Version 99.0.0 != local SCHEMA_VERSION → drift=true.
    assert!(stdout.contains("drift=true"), "stdout={stdout}");
    assert!(stdout.contains("version_changed=true"), "stdout={stdout}");

    assert!(workdir.path().join("drift-report.md").exists());
    assert!(workdir.path().join("drift-report.json").exists());

    let go = std::fs::read_to_string(&github_output).unwrap();
    assert!(go.contains("drift=true"));
    assert!(go.contains("version_changed=true"));
}

#[tokio::test]
async fn binary_exits_nonzero_on_npm_5xx() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/latest"))
        .respond_with(wiremock::ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let workdir = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_adf-schema-drift");
    let output = Command::new(exe)
        .args(["--output-dir", workdir.path().to_str().unwrap()])
        .env(NPM_LATEST_URL_ENV, format!("{}/latest", server.uri()))
        .env_remove("GITHUB_OUTPUT")
        .output()
        .expect("spawn binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("non-2xx") || stderr.contains("503"),
        "stderr={stderr}"
    );
}

#[test]
fn binary_help_succeeds() {
    let exe = env!("CARGO_BIN_EXE_adf-schema-drift");
    let output = Command::new(exe).arg("--help").output().expect("spawn");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--format"));
    assert!(stdout.contains("--output-dir"));
}
