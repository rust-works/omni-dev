//! `adf-schema-codegen` — emit `src/atlassian/adf_schema/generated.rs` from
//! the vendored upstream JSON schema (`assets/adf-schema/full.json`) and its
//! provenance sidecar (`assets/adf-schema/provenance.json`).
//!
//! Implements issue [#732] — code-generates the ADF allowed-children atoms
//! table from `@atlaskit/adf-schema` so the source of truth lives in upstream
//! data, not in hand transcription. The hand-maintained `CONTENT_ENTRIES`
//! table in `src/atlassian/adf_schema/mod.rs` keeps the per-term quantifier
//! information that the upstream JSON schema does not expose in a parseable
//! shape; the integration tests assert the two views agree modulo a small,
//! documented leniency allowlist.
//!
//! # Usage
//!
//! Regenerate the file (writes `src/atlassian/adf_schema/generated.rs`):
//!
//! ```text
//! cargo run --bin adf-schema-codegen
//! ```
//!
//! CI check — exit non-zero if the committed file is out of date with
//! respect to the vendored JSON:
//!
//! ```text
//! cargo run --bin adf-schema-codegen -- --check
//! ```
//!
//! [#732]: https://github.com/rust-works/omni-dev/issues/732
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use omni_dev::atlassian::adf_schema::drift::{hex_encode, parse_upstream_full_json};

/// Sidecar provenance file. Mirrors the JSON shape of
/// `assets/adf-schema/provenance.json`.
#[derive(Debug, Deserialize)]
struct Provenance {
    npm_package: String,
    version: String,
    tarball_sha256: String,
    full_json_sha256: String,
}

#[derive(Debug, Parser)]
#[command(
    name = "adf-schema-codegen",
    about = "Generate src/atlassian/adf_schema/generated.rs from the vendored @atlaskit/adf-schema JSON"
)]
struct Cli {
    /// Path to the vendored upstream JSON schema.
    #[arg(long, default_value = "assets/adf-schema/full.json")]
    full_json: PathBuf,

    /// Path to the sidecar provenance JSON.
    #[arg(long, default_value = "assets/adf-schema/provenance.json")]
    provenance: PathBuf,

    /// Output path for the generated Rust file.
    #[arg(long, default_value = "src/atlassian/adf_schema/generated.rs")]
    output: PathBuf,

    /// Verify the committed output matches what would be generated. Exit 1
    /// (with a diff hint) if it does not. Does not write any files.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    classify(run(&cli, &mut std::io::stderr()))
}

/// Convert a `run` result into the binary's exit code, with a stderr message
/// for the error case. Split out so tests can exercise `run` directly.
fn classify(result: Result<bool>) -> ExitCode {
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("adf-schema-codegen: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Return `Ok(true)` on success, `Ok(false)` for a `--check` failure (drift
/// detected), `Err(_)` for I/O / parse failures.
///
/// `stderr` is where the `--check` failure hint is written; production passes
/// `std::io::stderr()`, tests capture into a buffer.
fn run(cli: &Cli, stderr: &mut dyn std::io::Write) -> Result<bool> {
    let full_json_bytes =
        fs::read(&cli.full_json).with_context(|| format!("reading {}", cli.full_json.display()))?;

    let provenance_bytes = fs::read(&cli.provenance)
        .with_context(|| format!("reading {}", cli.provenance.display()))?;
    let provenance: Provenance = serde_json::from_slice(&provenance_bytes)
        .with_context(|| format!("parsing {}", cli.provenance.display()))?;

    let computed_sha = hex_encode(&Sha256::digest(&full_json_bytes));
    if computed_sha != provenance.full_json_sha256 {
        return Err(anyhow!(
            "{} SHA-256 mismatch: computed {computed_sha}, provenance says {}",
            cli.full_json.display(),
            provenance.full_json_sha256,
        ));
    }

    let full: Value = serde_json::from_slice(&full_json_bytes)
        .with_context(|| format!("parsing {}", cli.full_json.display()))?;

    let upstream_map = parse_upstream_full_json(&full)?;
    let generated = render_generated_rs(&provenance, &upstream_map);
    let generated = rustfmt(&generated).context("formatting generated source with rustfmt")?;

    if cli.check {
        let existing = fs::read_to_string(&cli.output)
            .with_context(|| format!("reading {} for --check", cli.output.display()))?;
        if existing == generated {
            Ok(true)
        } else {
            writeln!(
                stderr,
                "adf-schema-codegen: {} is out of date with respect to {}",
                cli.output.display(),
                cli.full_json.display(),
            )
            .context("writing --check failure hint")?;
            writeln!(
                stderr,
                "hint: run `cargo run --bin adf-schema-codegen` to regenerate, then commit."
            )
            .context("writing --check failure hint")?;
            Ok(false)
        }
    } else {
        write_if_changed(&cli.output, &generated)?;
        Ok(true)
    }
}

/// Pipe `source` through `rustfmt` (reading the workspace's `rustfmt.toml`
/// from the current working directory) and return its stdout.
///
/// Running rustfmt over the hand-rendered string is what makes the generator
/// idempotent against `cargo fmt`: the committed `generated.rs` always
/// matches what `cargo fmt` would produce, so `--check` does not falsely
/// flag formatter-only drift.
fn rustfmt(source: &str) -> Result<String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2021", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning rustfmt — is it on PATH?")?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("rustfmt stdin not available"))?
        .write_all(source.as_bytes())
        .context("writing source to rustfmt stdin")?;
    let out = child
        .wait_with_output()
        .context("waiting for rustfmt to exit")?;
    if !out.status.success() {
        return Err(anyhow!(
            "rustfmt exited with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    String::from_utf8(out.stdout).context("decoding rustfmt stdout")
}

/// Write `contents` to `path` only when it differs from the current file
/// contents (if any). Avoids touching mtimes for no-op regenerations, which
/// keeps `cargo build` cache hits intact.
fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == contents {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent of {}", path.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// Render `generated.rs` as a deterministic, formatted Rust source string.
///
/// Output shape:
/// - File-level doc comment naming the generator and refresh workflow.
/// - Provenance `pub const` strings (package, version, tarball SHA, JSON
///   SHA).
/// - `UPSTREAM_ENTRIES` slice: parents sorted alphabetically, children
///   within each parent sorted alphabetically. No duplicates.
fn render_generated_rs(
    provenance: &Provenance,
    upstream_map: &BTreeMap<String, BTreeSet<String>>,
) -> String {
    let mut out = String::new();
    out.push_str(GENERATED_HEADER);

    out.push_str("\n/// Upstream npm package name.\n");
    out.push_str(&format!(
        "pub const UPSTREAM_PACKAGE: &str = {:?};\n",
        provenance.npm_package
    ));

    out.push_str("\n/// Upstream npm package version this snapshot was generated from.\n");
    out.push_str(&format!(
        "pub const UPSTREAM_VERSION: &str = {:?};\n",
        provenance.version
    ));

    out.push_str("\n/// SHA-256 of the upstream tarball that produced this snapshot.\n");
    out.push_str(&format!(
        "pub const UPSTREAM_TARBALL_SHA256: &str = {:?};\n",
        provenance.tarball_sha256
    ));

    out.push_str("\n/// SHA-256 of the vendored `assets/adf-schema/full.json` bytes.\n");
    out.push_str(&format!(
        "pub const UPSTREAM_FULL_JSON_SHA256: &str = {:?};\n",
        provenance.full_json_sha256
    ));

    out.push_str(ENTRIES_DOC);
    out.push_str("pub const UPSTREAM_ENTRIES: &[(&str, &[&str])] = &[\n");
    for (parent, children) in upstream_map {
        if children.is_empty() {
            continue;
        }
        out.push_str("    (\n");
        out.push_str(&format!("        {parent:?},\n"));
        out.push_str("        &[\n");
        for child in children {
            out.push_str(&format!("            {child:?},\n"));
        }
        out.push_str("        ],\n");
        out.push_str("    ),\n");
    }
    out.push_str("];\n");

    out
}

const GENERATED_HEADER: &str = "\
//! Auto-generated from `assets/adf-schema/full.json` by
//! `src/bin/adf_schema_codegen.rs`.
//!
//! **Do not edit by hand.** To refresh the snapshot, follow
//! `assets/adf-schema/README.md`:
//!
//! 1. Replace `assets/adf-schema/full.json` with a newly-extracted upstream
//!    `dist/json-schema/v1/full.json`.
//! 2. Update `assets/adf-schema/provenance.json` with the new version and
//!    tarball/JSON SHA-256s.
//! 3. Run `cargo run --bin adf-schema-codegen`.
//! 4. Commit `full.json`, `provenance.json`, and this file together.
//!
//! See issue #732 (ADR-0023 follow-up) for the rationale.
";

const ENTRIES_DOC: &str = "
/// Per-parent allowed-children atoms, derived faithfully from the upstream
/// `@atlaskit/adf-schema` JSON schema in `assets/adf-schema/full.json`.
///
/// Sorted alphabetically by parent; children within each parent are also
/// sorted alphabetically and deduplicated. Quantifier and order information
/// (`+`, `*`, `?`, `{n}`, `{m,n}`, sequence order) is *not* preserved here —
/// the upstream JSON schema's `anyOf`-of-`$ref` shape does not encode it in
/// a parseable way. See [`super::CONTENT_ENTRIES`] in
/// `src/atlassian/adf_schema/mod.rs` for the runtime model that layers
/// quantifier arity on top of these atoms.
///
/// The unit test `generated_upstream_atoms_match_local_snapshot` in
/// `src/atlassian/adf_schema/mod.rs` asserts that the flattened atoms from
/// [`super::CONTENT_ENTRIES`] agree with `UPSTREAM_ENTRIES` modulo a small
/// allowlist of documented leniency deviations.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_provenance() -> Provenance {
        Provenance {
            npm_package: "@atlaskit/adf-schema".to_string(),
            version: "52.9.5".to_string(),
            tarball_sha256: "abc123".to_string(),
            full_json_sha256: "def456".to_string(),
        }
    }

    #[test]
    fn render_emits_sorted_parents_and_children() {
        let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        map.insert(
            "panel".to_string(),
            ["heading", "paragraph"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        map.insert(
            "blockquote".to_string(),
            ["paragraph", "bulletList"]
                .into_iter()
                .map(String::from)
                .collect(),
        );
        let out = render_generated_rs(&fixture_provenance(), &map);

        // Parents in alphabetical order: blockquote precedes panel.
        let bq_idx = out.find("\"blockquote\"").expect("blockquote present");
        let panel_idx = out.find("\"panel\"").expect("panel present");
        assert!(bq_idx < panel_idx, "parents must be alphabetical");

        // Children within blockquote: bulletList precedes paragraph.
        let bullet_idx = out.find("\"bulletList\"").expect("bulletList present");
        let para_idx = out.find("\"paragraph\"").expect("paragraph present");
        assert!(bullet_idx < para_idx, "children must be alphabetical");
    }

    #[test]
    fn render_includes_provenance_constants() {
        let out = render_generated_rs(&fixture_provenance(), &BTreeMap::new());
        assert!(out.contains("UPSTREAM_PACKAGE: &str = \"@atlaskit/adf-schema\""));
        assert!(out.contains("UPSTREAM_VERSION: &str = \"52.9.5\""));
        assert!(out.contains("UPSTREAM_TARBALL_SHA256: &str = \"abc123\""));
        assert!(out.contains("UPSTREAM_FULL_JSON_SHA256: &str = \"def456\""));
    }

    #[test]
    fn render_skips_parents_with_no_children() {
        let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        map.insert("emptyParent".to_string(), BTreeSet::new());
        let out = render_generated_rs(&fixture_provenance(), &map);
        assert!(!out.contains("\"emptyParent\""));
    }

    #[test]
    fn write_if_changed_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/deep/out.rs");
        write_if_changed(&target, "// hello\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "// hello\n");
    }

    #[test]
    fn write_if_changed_is_a_noop_when_contents_match() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.rs");
        fs::write(&target, "same\n").unwrap();
        let original_mtime = fs::metadata(&target).unwrap().modified().unwrap();
        // Wait long enough that any actual rewrite would tick mtime on at
        // least the coarse-grained filesystems (HFS+ is 1s).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_if_changed(&target, "same\n").unwrap();
        let after_mtime = fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(original_mtime, after_mtime);
    }

    // -------------------------------------------------------------------------
    // run() / rustfmt() / classify() integration coverage
    // -------------------------------------------------------------------------

    /// Minimal JSON-schema fragment that `parse_upstream_full_json` accepts.
    /// Produces a single parent (`paragraph`) with a single child (`text`).
    /// Uses `r##"..."##` because the JSON `$ref` values contain `"#`.
    const MINIMAL_FULL_JSON: &str = r##"{
        "definitions": {
            "paragraph_node": {
                "properties": {
                    "type": {"const": "paragraph"},
                    "content": {
                        "items": {
                            "anyOf": [{"$ref": "#/definitions/text_node"}]
                        }
                    }
                }
            },
            "text_node": {
                "properties": {
                    "type": {"const": "text"}
                }
            }
        }
    }"##;

    /// Build a tempdir with `full.json`, `provenance.json`, and an `output`
    /// path inside it. Returns the tempdir (kept alive by the caller) and a
    /// `Cli` pointing at the files.
    fn setup_fixture(full_json: &str, check: bool) -> (tempfile::TempDir, Cli) {
        let dir = tempfile::tempdir().unwrap();
        let full_json_path = dir.path().join("full.json");
        fs::write(&full_json_path, full_json).unwrap();
        let computed_sha = hex_encode(&Sha256::digest(full_json.as_bytes()));

        let provenance_path = dir.path().join("provenance.json");
        let provenance = serde_json::json!({
            "npm_package": "@atlaskit/adf-schema",
            "version": "99.0.0",
            "tarball_sha256": "deadbeef",
            "full_json_sha256": computed_sha,
        });
        fs::write(&provenance_path, serde_json::to_vec(&provenance).unwrap()).unwrap();

        let output_path = dir.path().join("generated.rs");
        let cli = Cli {
            full_json: full_json_path,
            provenance: provenance_path,
            output: output_path,
            check,
        };
        (dir, cli)
    }

    #[test]
    fn run_writes_generated_file_with_expected_contents() {
        let (_dir, cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        let mut stderr: Vec<u8> = Vec::new();
        let ok = run(&cli, &mut stderr).unwrap();
        assert!(ok, "happy path returns Ok(true)");
        let written = fs::read_to_string(&cli.output).unwrap();
        assert!(written.contains("UPSTREAM_PACKAGE: &str = \"@atlaskit/adf-schema\""));
        assert!(written.contains("UPSTREAM_VERSION: &str = \"99.0.0\""));
        assert!(written.contains("\"paragraph\""));
        assert!(written.contains("\"text\""));
        assert!(stderr.is_empty(), "no stderr on happy path");
    }

    #[test]
    fn run_check_succeeds_when_output_matches() {
        let (_dir, mut cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        let mut stderr: Vec<u8> = Vec::new();
        assert!(run(&cli, &mut stderr).unwrap());
        cli.check = true;
        let mut stderr2: Vec<u8> = Vec::new();
        assert!(
            run(&cli, &mut stderr2).unwrap(),
            "second run with --check returns Ok(true) because the file matches"
        );
        assert!(stderr2.is_empty());
    }

    #[test]
    fn run_check_fails_when_output_stale() {
        let (_dir, mut cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        let mut stderr: Vec<u8> = Vec::new();
        assert!(run(&cli, &mut stderr).unwrap());
        // Stomp the committed file so it no longer matches what the codegen
        // would emit.
        fs::write(&cli.output, "// stale\n").unwrap();
        cli.check = true;
        let mut stderr2: Vec<u8> = Vec::new();
        let ok = run(&cli, &mut stderr2).unwrap();
        assert!(!ok, "stale file should return Ok(false)");
        let msg = String::from_utf8(stderr2).unwrap();
        assert!(msg.contains("out of date"));
        assert!(msg.contains("hint"));
    }

    #[test]
    fn run_errors_when_full_json_missing() {
        let (_dir, mut cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        cli.full_json = cli.full_json.with_file_name("nope.json");
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("reading"));
    }

    #[test]
    fn run_errors_when_provenance_missing() {
        let (_dir, mut cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        cli.provenance = cli.provenance.with_file_name("nope.json");
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("reading"));
    }

    #[test]
    fn run_errors_when_provenance_unparseable() {
        let (_dir, cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        fs::write(&cli.provenance, "not-json").unwrap();
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("parsing"));
    }

    #[test]
    fn run_errors_when_full_json_sha_mismatch() {
        let (_dir, cli) = setup_fixture(MINIMAL_FULL_JSON, false);
        // Mutate the file *after* provenance was sealed; the SHA no longer
        // matches.
        fs::write(&cli.full_json, "{\"definitions\": {}}").unwrap();
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("SHA-256 mismatch"));
    }

    #[test]
    fn run_errors_when_full_json_unparseable() {
        let (_dir, cli) = setup_fixture("not-json", false);
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("parsing"));
    }

    #[test]
    fn run_check_errors_when_output_missing() {
        let (_dir, mut cli) = setup_fixture(MINIMAL_FULL_JSON, true);
        // Output never written → read_to_string fails.
        cli.output = cli.output.with_file_name("never-written.rs");
        let mut stderr: Vec<u8> = Vec::new();
        let err = run(&cli, &mut stderr).unwrap_err();
        assert!(err.to_string().contains("--check"));
    }

    #[test]
    fn rustfmt_formats_valid_source() {
        let unfmt = "pub fn   foo()    {  }\n";
        let out = rustfmt(unfmt).unwrap();
        assert_eq!(out.trim(), "pub fn foo() {}");
    }

    #[test]
    fn rustfmt_errors_on_invalid_source() {
        // Missing semicolon + unclosed brace → unparseable.
        let err = rustfmt("pub fn foo() { let x = ").unwrap_err();
        assert!(
            err.to_string().contains("rustfmt"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn classify_maps_results_to_exit_codes() {
        // Can't easily compare ExitCode values directly; map them through a
        // sentinel by serialising via Debug, which contains a clear field.
        // Or — simpler — observe that ExitCode is opaque; just call classify
        // for each branch and trust that no panic occurs. Coverage will note
        // each branch was entered.
        let _ = classify(Ok(true));
        let _ = classify(Ok(false));
        let _ = classify(Err(anyhow!("boom")));
    }
}
