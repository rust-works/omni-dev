# Vendored `@atlaskit/adf-schema` artefacts

Source artefacts for the code-generated ADF schema table (issue #732).

## Files

- `full.json` — the upstream `dist/json-schema/v1/full.json` extracted from the
  pinned tarball. The only file the generator reads.
- `provenance.json` — the npm package version, tarball URL, tarball SHA-256,
  and a SHA-256 of `full.json` itself. The generator bakes these into the
  emitted source's `pub const`s so the binary carries the provenance.

## Refresh workflow

1. Download a new upstream tarball:
   ```
   curl -sL https://registry.npmjs.org/@atlaskit/adf-schema/-/adf-schema-<ver>.tgz -o /tmp/adf-schema.tgz
   shasum -a 256 /tmp/adf-schema.tgz
   ```
2. Extract `package/dist/json-schema/v1/full.json` into this directory.
3. Update `provenance.json` with the new version, tarball SHA, and the new
   `full.json` SHA (`shasum -a 256 assets/adf-schema/full.json`).
4. Run the generator:
   ```
   cargo run --bin adf-schema-codegen
   ```
   This rewrites `src/atlassian/adf_schema/generated.rs`.
5. Update `SCHEMA_VERSION` (`<npm-version>-<refresh-date>`) and
   `UPSTREAM_TARBALL_SHA256` in `src/atlassian/adf_schema.rs` to match the
   new provenance. Review the full JSON diff, not just parent/child atoms;
   attribute and mark constraints can change without content-model drift.
   Update `CONTENT_ENTRIES` only if the content rules have changed.
6. Run the consistency test:
   ```
   cargo test --lib atlassian::adf_schema
   cargo test --test adf_schema_test
   ```
   Drift between the upstream snapshot and the hand-maintained
   `CONTENT_ENTRIES` is reported, modulo the documented leniency allowlist in
   the test.
7. Commit `full.json`, `provenance.json`, `generated.rs`, and the runtime
   provenance constants together.

The generator is also CI-checkable: `cargo run --bin adf-schema-codegen --
--check` exits non-zero if the committed `generated.rs` is out of date with
respect to the vendored `full.json`.
