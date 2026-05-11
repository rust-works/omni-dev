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
5. Run the consistency test:
   ```
   cargo test --test adf_schema_test
   ```
   Drift between the upstream snapshot and the hand-maintained
   `CONTENT_ENTRIES` is reported, modulo the documented leniency allowlist in
   the test.
6. Commit `full.json`, `provenance.json`, and `generated.rs` together.

The generator is also CI-checkable: `cargo run --bin adf-schema-codegen --
--check` exits non-zero if the committed `generated.rs` is out of date with
respect to the vendored `full.json`.
