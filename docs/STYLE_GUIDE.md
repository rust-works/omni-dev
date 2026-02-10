# Style Guide

Coding conventions for the omni-dev project. Each item has a unique ID for easy reference.

## STYLE-0000: Style guide structure

### Situation

A new convention needs to be added to this style guide.

### Guidance

Assign the next sequential ID (currently next is `STYLE-0021`) and include three subheadings:

- **Situation** — when this rule applies
- **Guidance** — what to do (with examples where helpful)
- **Motivation** — why this rule exists

### Motivation

Consistent structure makes the guide scannable, and stable IDs allow code review comments and ADRs to reference specific rules unambiguously.

---

## Error Handling

### STYLE-0001: Default error type

#### Situation

A function can fail and needs to return an error.

#### Guidance

Use `anyhow::Result<T>` as the return type. Import both `Context` and `Result`:

```rust
use anyhow::{Context, Result};

fn open_repo() -> Result<Repository> {
    Repository::open(".").context("Failed to open git repository")?;
    // ...
}
```

Reserve `thiserror` enums for domain boundaries where callers need to match on specific
error variants. Currently the only custom error type is `ClaudeError` in
`src/claude/error.rs`, which covers API-specific failure modes (key not found, rate limit,
network error). These convert to `anyhow::Error` automatically via the blanket impl.

Use `anyhow::bail!()` for early returns with an error message:

```rust
anyhow::bail!("Repository is in detached HEAD state");
```

#### Motivation

`anyhow` provides lightweight error chaining without defining boilerplate error types.
Reserving `thiserror` for domain boundaries keeps the type surface small while still
allowing pattern matching where it matters.

### STYLE-0002: Context message style

#### Situation

Adding `.context()` or `.with_context()` to a fallible operation.

#### Guidance

Write context messages in **sentence case** describing the **failed operation**:

```rust
// Good — describes the operation that failed
.context("Failed to get HEAD reference")?;
.context("Cannot amend commits with uncommitted changes")?;
.context("Not in a git repository")?;

// Bad — includes function name
.context("open_repo: could not open")?;

// Bad — too generic
.context("error")?;
```

Use `.with_context()` when the message needs runtime values:

```rust
.with_context(|| format!("Failed to parse start commit: {}", start_spec))?;
```

Prefer `.context()` over `.with_context()` for static messages since it avoids the closure
allocation.

#### Motivation

Sentence-case messages read naturally in error chains printed by `main.rs`. Describing the
operation (not the function) keeps messages useful regardless of refactoring. The
`with_context` pattern avoids allocating format strings on the success path.

### STYLE-0003: Panicking operations

#### Situation

Considering `unwrap()`, `expect()`, or other panicking calls.

#### Guidance

**`unwrap()` is acceptable** in these cases only:

- **Static regex** — use `std::sync::LazyLock` so the pattern is compiled once and the
  `unwrap()` is confined to the initialiser. Clippy's `invalid_regex` lint (deny by default)
  validates the literal at compile time, so the `unwrap()` is provably safe.

  ```rust
  use std::sync::LazyLock;
  use regex::Regex;

  static SCOPE_RE: LazyLock<Regex> =
      LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]*$").unwrap());
  ```

- **Known-safe constructors** — `FixedOffset::east_opt(0).unwrap()` where the argument is
  a constant that cannot fail.
- **Test code** — tests may use `unwrap()` freely.

**`expect()` is acceptable** for truly catastrophic I/O that should terminate the process:

```rust
io::stdout().flush().expect("Failed to flush stdout");
```

**Never** use `unwrap()` or `expect()` on user-supplied or runtime data in library code.
Use `?` with `.context()` instead.

#### Motivation

Panics in library code produce poor diagnostics and cannot be handled by callers. Limiting
panics to provably-safe or catastrophic cases keeps the error surface predictable.
A lazy static avoids recompiling the regex on every call and makes the safety argument
obvious at the declaration site.

### STYLE-0018: Silent error suppression

#### Situation

Handling a `Result` or `Option` where the error/`None` case is intentionally ignored.

#### Guidance

**Never silently discard an error that could indicate a real problem.** Three patterns to watch
for:

1. **`let _ = fallible_call();`** — If the operation can meaningfully fail, at least log the
   error at `debug!` or `warn!` level. If the failure is truly inconsequential (best-effort
   cleanup), add a comment explaining why:

   ```rust
   // Bad — caller has no idea the abort failed
   let _ = Command::new("git").args(["rebase", "--abort"]).output();

   // Good — intent is documented, failure is logged
   // Best-effort cleanup; the rebase may already have been aborted.
   if let Err(e) = Command::new("git").args(["rebase", "--abort"]).output() {
       tracing::debug!("Rebase abort during cleanup failed: {e}");
   }
   ```

2. **`if let Ok(x) = ... { use(x) }`** with no `else` — returning a silent default on parse
   or I/O failure hides broken configuration files from the user:

   ```rust
   // Bad — silently returns empty vec on malformed YAML
   if let Ok(content) = fs::read_to_string(&path) {
       if let Ok(config) = serde_yaml::from_str(&content) {
           return config.scopes;
       }
   }
   Vec::new()

   // Good — warns so the user knows their file was ignored
   match fs::read_to_string(&path) {
       Ok(content) => match serde_yaml::from_str(&content) {
           Ok(config) => return config.scopes,
           Err(e) => tracing::warn!("Ignoring {}: {e}", path.display()),
       },
       Err(e) if e.kind() != io::ErrorKind::NotFound => {
           tracing::warn!("Cannot read {}: {e}", path.display());
       }
       _ => {} // File not found is expected in the fallback chain
   }
   ```

3. **`.unwrap_or_default()` on non-trivial results** — acceptable for genuinely optional data,
   but not as a blanket substitute for error handling on operations that should succeed.

**Acceptable silent discards:**

- Closing a file or flushing a logger during shutdown.
- Sending on a channel where the receiver may have been dropped.
- Test cleanup in `Drop` implementations.

#### Motivation

Silent error suppression is one of the hardest bugs to diagnose because nothing visibly fails —
the program simply produces wrong results or missing data. Logging at `debug!` or `warn!` level
costs nothing on the success path and provides a trail when something goes wrong. The explicit
comment requirement for `let _ =` forces the author to justify the suppression at write time,
which often reveals that the error should not be ignored after all.

---

## Module Organization

### STYLE-0004: Module file layout

#### Situation

Adding a new module or reorganizing an existing one.

#### Guidance

Use the **named-file layout** (Rust 2018+) for modules with submodules. Place the parent
module in a file named after the module alongside a directory of the same name:

```
src/
├── claude.rs           # declares submodules, re-exports public types
├── claude/
│   ├── client.rs
│   ├── error.rs
│   ├── prompts.rs
│   ├── ai.rs           # declares ai submodules
│   ├── ai/
│   │   ├── bedrock.rs
│   │   ├── claude.rs
│   │   └── openai.rs
│   ├── context.rs      # declares context submodules
│   └── context/
│       ├── branch.rs
│       ├── discovery.rs
│       ├── files.rs
│       └── patterns.rs
├── core.rs             # no submodules, so just a single file
├── lib.rs
└── main.rs
```

Do **not** use `mod.rs` for new modules. The named-file layout gives every module root a
unique filename, which avoids ambiguous editor tabs and search results when multiple
`mod.rs` files exist.

Re-export key public types from each module root so consumers can import from the parent
module:

```rust
// src/git.rs
pub use amendment::AmendmentHandler;
pub use commit::{CommitAnalysis, CommitInfo};
pub use repository::GitRepository;
```

Only re-export types that appear in the module's public API signatures. Internal helpers,
intermediate types, and implementation details should stay private to their submodule even
if they are `pub` there. A re-export is a promise that the type is part of the module's
contract.

> **Migration note:** The existing codebase uses `mod.rs` in some modules. Convert to
> named-file layout opportunistically when making significant changes to a module, but do
> not churn files solely to migrate.

#### Motivation

The named-file layout is recommended by the Rust Book and is the default assumed by
`rust-analyzer`. Each module root has a distinct filename (e.g., `claude.rs` vs `context.rs`)
instead of multiple `mod.rs` files, making editor tabs, file search, and `git log` output
unambiguous. Re-exports in the module root present a clean public interface per module.
Limiting re-exports to API-surface types prevents leaking implementation details that would
be hard to remove later.

### STYLE-0005: Visibility

#### Situation

Deciding whether to make an item `pub`, `pub(crate)`, or private.

#### Guidance

Default to **private** (no visibility modifier). Use three visibility levels:

| Visibility   | Meaning                  | Use when                                               |
|--------------|--------------------------|--------------------------------------------------------|
| *(none)*     | Private to the module    | Internal helpers                                       |
| `pub(crate)` | Visible within the crate | Shared across modules but not part of the external API |
| `pub`        | Fully public             | Part of the crate's published API surface              |

```rust
impl AmendmentFile {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> { ... }  // public API
    pub(crate) fn validate_schema(&self) -> Result<()> { ... }              // crate-internal
    fn format_multiline_yaml(&self, yaml: &str) -> String { ... }          // module-private
}
```

When in doubt, start private and widen visibility only when needed. Prefer `pub(crate)`
over `pub` for items that other modules need but external consumers should not rely on.

The rustc lint `unreachable_pub` (allowed by default) can be enabled to detect `pub` items
that are not actually reachable from outside the crate.

#### Motivation

Minimal visibility reduces the API surface that must be maintained (Effective Rust, Item
22). Using `pub(crate)` for internal cross-module items prevents accidentally promising API
stability to external consumers. Making a public item private is a breaking change; making
a private item public is not.

---

## Naming Conventions

### STYLE-0006: Naming patterns

#### Situation

Naming a new type, function, CLI command, environment variable, or YAML field.

#### Guidance

| Element           | Convention            | Examples                                      |
|-------------------|-----------------------|-----------------------------------------------|
| Structs / Enums   | PascalCase            | `CommitInfo`, `ClaudeError`, `WorkType`       |
| Traits            | PascalCase (adj/verb) | `AiClient`, `Serialize`, `Display`            |
| Functions/Methods | snake_case            | `from_git_commit()`, `analyze_commit()`       |
| Type aliases      | PascalCase            | `Result<T>` (for crate-local aliases)         |
| Constants         | UPPER_SNAKE_CASE      | `VERSION`                                     |
| Environment vars  | UPPER_SNAKE_CASE      | `CLAUDE_API_KEY`, `AI_SCRATCH`                |
| CLI commands      | kebab-case            | `help-all`, `commit message view`             |
| YAML fields       | snake_case            | `original_message`, `in_main_branches`        |
| Modules / files   | snake_case            | `model_config.rs`, `ai_scratch.rs`            |

**Project-specific pattern — `*ForAI` suffix:** When a data structure has a variant that
includes additional content for AI processing (e.g., full diff text), suffix the variant
with `ForAI`:

```rust
pub struct CommitInfo { ... }       // standard version
pub struct CommitInfoForAI { ... }  // includes diff_content field
```

#### Motivation

Standard Rust naming (`PascalCase` types, `snake_case` functions) is enforced by compiler
warnings and `clippy`. The `*ForAI` suffix convention makes it immediately clear which
structs carry the heavier AI-oriented payload. Kebab-case CLI commands follow `clap`
conventions and are standard across Unix tools.

---

## Commit Conventions

### STYLE-0007: Commit message format

#### Situation

Writing a commit message.

#### Guidance

Follow [`.omni-dev/commit-guidelines.md`](../.omni-dev/commit-guidelines.md) for the full
specification including types, scopes, subject line rules, body guidelines, and breaking
change conventions.

The commit guidelines must themselves follow **Conventional Commits** and remain consistent
with the scope definitions in `.omni-dev/scopes.yaml`.

#### Motivation

Keeping the detailed commit specification in `.omni-dev/commit-guidelines.md` allows the AI
context system to consume it directly, avoiding duplication between this style guide and the
machine-readable guidelines.

### STYLE-0020: Single-purpose commits

#### Situation

Preparing a set of changes that involves refactoring, new functionality, or bug fixes.

#### Guidance

Each commit should do **one kind of work**. Keep refactoring commits separate from
implementation commits, and both separate from bug-fix commits.

If a refactoring would make a subsequent implementation or fix cleaner, land the refactoring
as an **earlier** commit so that:

1. The refactoring can be reviewed on its own terms (no behaviour change expected).
2. The implementation commit starts from a cleaner baseline and is easier to understand.
3. Either commit can be reverted independently if needed.

```
# Good — reviewable, bisectable, revertible
git log --oneline
a1b2c3  refactor(cli): extract shared repository-view builder
d4e5f6  feat(cli): add --json output to check command

# Bad — mixed intent, hard to review or revert half of it
git log --oneline
f7g8h9  feat(cli): add --json output and refactor repo-view builder
```

**Acceptable exceptions:**

- Trivial renames or import cleanups that are a natural by-product of the implementation
  (a few lines, not a standalone refactoring effort).
- Prototype or spike branches where commit hygiene is deferred to a squash before merge.

#### Motivation

Single-purpose commits make `git bisect` reliable, code review focused, and reverts
surgical. When refactoring is interleaved with behaviour changes, reviewers cannot tell
whether a difference is a deliberate new behaviour or a mechanical restructuring — so they
must verify every line as if it were new logic. Separating the two cuts review effort
roughly in half.

---

## Documentation Standards

### STYLE-0008: Doc comments

#### Situation

Adding or updating documentation on a module, type, or function.

#### Guidance

**Module-level docs** — every module file starts with a `//!` comment:

```rust
//! Git commit operations and analysis.
```

**Item-level docs** — every public struct, enum, field, variant, and method gets `///`:

```rust
/// Represents a single commit with its metadata and analysis.
pub struct CommitInfo {
    /// Full SHA-1 hash of the commit.
    pub hash: String,
    /// Commit author name and email address.
    pub author: String,
}
```

**Summary line style** — write in **third-person singular present indicative** per
[RFC 505](https://rust-lang.github.io/rfcs/0505-api-comment-conventions.html). Use full
sentences ending with a period:

```rust
/// Creates a `CommitInfo` from a `git2::Commit`.
pub fn from_git_commit(...) -> Result<Self> { ... }

/// Returns the suggested level of detail for commit messages.
pub fn suggested_verbosity(&self) -> VerbosityLevel { ... }
```

| Correct (third-person)         | Incorrect (imperative)        |
|--------------------------------|-------------------------------|
| `/// Returns the length.`      | `/// Return the length.`      |
| `/// Creates a new client.`    | `/// Create a new client.`    |
| `/// Parses the input string.` | `/// Parse the input string.` |

The crate-level lint `#![warn(missing_docs)]` in `src/lib.rs` will warn on any public item
missing a doc comment.

**`# Examples` sections** — public functions that are not self-explanatory should include a
doc example. These are compiled and run by `cargo test`, so they serve as both documentation
and regression tests:

```rust
/// Parses a conventional commit subject line.
///
/// # Examples
///
/// ```
/// let parsed = parse_subject("feat(cli): add --fresh flag");
/// assert_eq!(parsed.commit_type, "feat");
/// assert_eq!(parsed.scope, Some("cli"));
/// ```
pub fn parse_subject(input: &str) -> ParsedSubject { ... }
```

Doc examples are not required for trivial getters, builders, or `From`/`Into`
implementations where the behaviour is obvious from the type signature.

#### Motivation

The third-person convention matches the Rust standard library and `rustdoc` output, where
doc summaries read as descriptions of what the item *does* (e.g., `Vec::push` — "Appends
an element to the back of a collection."). RFC 505 codifies this as the official Rust API
documentation style. `#![warn(missing_docs)]` turns documentation into a compile-time
obligation rather than an afterthought. Doc examples provide compile-tested usage patterns
and catch API regressions that unit tests might miss.

---

## Testing Conventions

### STYLE-0009: Test structure

#### Situation

Writing a new test.

#### Guidance

Place unit tests in a `#[cfg(test)] mod tests` block at the **end** of the source file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_creation() {
        let app = App::new();
        assert!(!app.config.verbose);
    }
}
```

**Naming pattern:** `<thing_being_tested>[_<condition>]` — omit the `test_` prefix since
the `#[test]` attribute and `tests` module already identify these as tests. Clippy's
`redundant_test_prefix` lint (restriction group) flags the prefix as redundant.

```rust
fn load_model_registry() { ... }
fn parse_beta_header_valid() { ... }
fn app_with_config() { ... }
```

When a test uses `?` for error propagation, return `Result<()>`:

```rust
#[test]
fn amend_command_with_temporary_repo() -> Result<()> {
    let repo = TestRepo::new()?;
    // ...
    Ok(())
}
```

Place integration tests in the `tests/` directory.

**Test attributes:**

- **`#[should_panic]`** — avoid in favour of `Result`-returning tests that assert on the
  error. `#[should_panic]` matches on panic message substrings which are brittle across
  refactors. Use it only when testing that a documented panic condition (e.g., an `expect()`
  from STYLE-0003) fires correctly.
- **`#[ignore]`** — acceptable for tests that require external resources (network, API keys)
  or are unusually slow. Always add a reason: `#[ignore = "requires CLAUDE_API_KEY"]`. Run
  ignored tests explicitly with `cargo test -- --ignored`.

#### Motivation

The `mod tests` convention is idiomatic Rust and gives tests access to private items via
`use super::*`. Dropping the `test_` prefix avoids the triple-redundancy of
`tests::test_foo` in `cargo test` output. Consistent naming makes
`cargo test parse_beta` filtering predictable.

### STYLE-0010: Test data and fixtures

#### Situation

A test needs a git repository, temporary files, or other fixture data.

#### Guidance

Use `tempfile::TempDir` for isolated file system fixtures. For git-based tests, use a
helper struct that wraps the temp directory:

```rust
struct TestRepo {
    _temp_dir: TempDir,
    repo_path: PathBuf,
    repo: Repository,
    commits: Vec<git2::Oid>,
}

impl TestRepo {
    fn new() -> Result<Self> { ... }
    fn add_commit(&mut self, message: &str, content: &str) -> Result<()> { ... }
}
```

Use the `insta` crate for snapshot (golden) tests where output stability matters.

Do not commit large binary fixtures. Prefer constructing test data programmatically.

#### Motivation

Temporary directories prevent tests from interfering with each other or with the real
working directory. Snapshot testing with `insta` catches unintended output regressions
without manually maintaining expected-output files.

---

## Code Style

### STYLE-0011: Import ordering

#### Situation

Adding `use` statements to a file.

#### Guidance

Group imports into three blocks separated by a blank line, in this order:

1. **Standard library** (`std`, `core`, `alloc`)
2. **External crates** (everything from `Cargo.toml` dependencies)
3. **Crate-internal** (`crate::`, `super::`, `self::`)

Within each group, let `cargo fmt` sort alphabetically.

```rust
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::data::context::ScopeDefinition;
use crate::git::CommitInfo;
```

**Enforcement note:** The rustfmt option `group_imports = "StdExternalCrate"` that codifies
this convention is still unstable. The three-group ordering is therefore a manual discipline
— `cargo fmt` will sort *within* a group but will not insert or enforce the blank-line
separators between groups. Review for this during code review.

#### Motivation

Grouped imports make it easy to see at a glance what a module depends on externally versus
internally. The three-group convention is widely used in the Rust ecosystem. Alphabetical
ordering within groups is enforced by `cargo fmt`.

### STYLE-0012: Clippy configuration

#### Situation

Configuring or overriding Clippy lints.

#### Guidance

The crate-level lints in `src/lib.rs` are the project's Clippy baseline:

```rust
#![warn(missing_docs)]
#![warn(clippy::all)]
#![deny(unsafe_code)]
```

When suppressing a lint on a specific item, use `#[allow(clippy::...)]` with a justification
comment explaining why the suppression is necessary:

```rust
#[allow(clippy::too_many_arguments)] // Builder pattern requires all fields at construction
fn new(title: &str, description: &str, ...) -> Self { ... }
```

Do not add blanket `#[allow(...)]` at module or crate level to silence warnings. Fix the
warning or suppress it at the narrowest possible scope.

#### Motivation

`clippy::all` enables the default set of correctness and style lints. Requiring justification
comments on suppressions ensures each override is a deliberate decision rather than a way to
silence noise. Narrow-scope suppression prevents accidentally disabling a lint for unrelated
code.

### STYLE-0013: Unsafe policy

#### Situation

Considering the use of `unsafe` code.

#### Guidance

This project forbids `unsafe` code via `#![deny(unsafe_code)]` in `src/lib.rs`. This lint
is a hard error and applies to the entire crate.

If `unsafe` is ever required (e.g., FFI), it must be:

1. Justified in an ADR
2. Isolated in a dedicated module
3. Annotated with a `// SAFETY:` comment per Clippy's `undocumented_unsafe_blocks` lint

#### Motivation

omni-dev has no need for `unsafe` — it delegates low-level operations to well-audited
dependencies (`git2`, `reqwest`, `tokio`). The `deny` lint makes this a compile-time
guarantee rather than a convention. Requiring an ADR for any future exception ensures the
decision is reviewed and documented.

### STYLE-0016: Named constants

#### Situation

Using a numeric or string literal whose meaning is not obvious from surrounding context.

#### Guidance

Extract **magic literals** into named constants or `const` items. A literal is "magic" when its
purpose is not self-evident at the usage site:

```rust
// Bad — what does 8 mean?
let short = &hash[..8];

// Good — the name documents the intent
const SHORT_HASH_LEN: usize = 8;
let short = &hash[..SHORT_HASH_LEN];
```

```rust
// Bad — why 3?
if auth_attempts > 3 {
    bail!("Too many authentication attempts");
}

// Good
const MAX_AUTH_ATTEMPTS: u32 = 3;
if auth_attempts > MAX_AUTH_ATTEMPTS {
    bail!("Too many authentication attempts");
}
```

Literals that do **not** need extraction:

- **Structural zeros and ones** — `Vec::with_capacity(1)`, `index + 1`, `slice[0]`.
- **Format strings** — `format!("{}: {}", key, value)`.
- **Known-safe constructor arguments** — `FixedOffset::east_opt(0)` (covered by STYLE-0003).
- **Test assertions** — `assert_eq!(result.len(), 3)` where the value is local to the test.

Place constants at the narrowest useful scope: module-level `const` if used across functions in
the same module, crate-level if shared across modules, or function-local `const` if truly local.

#### Motivation

Named constants make the code self-documenting and provide a single point of change when a value
needs updating. Searching for `SHORT_HASH_LEN` finds every usage; searching for `8` returns
hundreds of false positives. The exceptions prevent over-extraction of trivially obvious values.

### STYLE-0017: Function length

#### Situation

Writing or reviewing a function that is growing long.

#### Guidance

Keep functions **under ~50 lines** of logic (excluding doc comments, blank lines, and closing
braces). When a function exceeds this guideline, look for opportunities to extract coherent
sub-operations into well-named helper functions.

Common extraction targets:

- **Setup / teardown** — opening resources, building configuration structs.
- **Distinct phases** — validation, transformation, output formatting.
- **Repeated patterns** — similar blocks that differ only in parameters.
- **Nested closures or callbacks** — especially credential handlers, diff callbacks.

```rust
// Before — 120-line execute() mixing validation, AI calls, file I/O, and display
fn execute(&self) -> Result<()> {
    // ... 120 lines ...
}

// After — orchestrator delegates to focused helpers
fn execute(&self) -> Result<()> {
    let repo_view = self.generate_repository_view()?;
    let context = self.collect_context(&repo_view)?;
    let amendments = self.generate_amendments(&repo_view, &context)?;
    self.apply_and_display(amendments)?;
    Ok(())
}
```

This is a **guideline, not a hard limit**. A 60-line function that reads linearly may be clearer
than three 20-line functions with non-obvious data flow. Use judgement — the goal is readability,
not a line count.

#### Motivation

Long functions are harder to name, test, and review. Extracting sub-operations gives each piece
a name that serves as documentation and makes the top-level flow scannable. The ~50-line
heuristic is a common industry threshold (Clean Code, Effective Rust) that balances granularity
against fragmentation.

---

## Ownership & API Design

### STYLE-0014: `#[must_use]` annotation

#### Situation

A public function or method returns a computed value without side effects.

#### Guidance

Apply `#[must_use]` to public functions whose return value is the entire point of the call.
Discarding the result is almost certainly a bug:

```rust
#[must_use]
pub fn suggested_verbosity(&self) -> VerbosityLevel { ... }

#[must_use]
pub fn is_conventional(&self) -> bool { ... }
```

**Do not apply** `#[must_use]` to:

- Functions that return `Result` — the `#[must_use]` on `Result` itself already covers this.
- Builder methods that return `&mut Self` — the builder pattern implies chaining.
- Functions with meaningful side effects (I/O, mutation) where the return value is
  supplementary.

#### Motivation

`#[must_use]` turns silent logic errors (ignoring a return value) into compiler warnings.
Applying it deliberately to pure computations catches bugs at compile time without producing
false positives on side-effectful functions. This aligns with `clippy::must_use_candidate`
from the `pedantic` group.

### STYLE-0015: String parameter ownership

#### Situation

Deciding whether a function parameter should be `&str`, `String`, or generic.

#### Guidance

Use the cheapest type that satisfies the function's needs:

| The function…                          | Accept              | Example                                     |
|----------------------------------------|---------------------|---------------------------------------------|
| Only reads the string                  | `&str`              | `fn parse_subject(input: &str)`             |
| Stores the string in a struct/`Vec`    | `String`            | `fn set_title(&mut self, title: String)`    |
| Needs flexibility (public API surface) | `impl Into<String>` | `fn new(name: impl Into<String>) -> Self`   |

Prefer `&str` for internal helpers and `impl Into<String>` sparingly — only at public API
boundaries where caller ergonomics justify the generic. Avoid `impl AsRef<str>` unless you
genuinely need to accept both `String` and `&str` without conversion.

For return types, prefer `&str` when returning a reference to owned data, and `String` when
returning a newly constructed value. Avoid `Cow<'_, str>` unless profiling shows the
borrow-or-own flexibility is needed.

```rust
// Good — borrows for read-only access
pub fn commit_type(&self) -> &str {
    &self.commit_type
}

// Good — takes ownership because it stores the value
pub fn with_title(mut self, title: String) -> Self {
    self.title = title;
    self
}

// Good — constructs a new string
pub fn format_summary(&self) -> String {
    format!("{}: {}", self.commit_type, self.subject)
}
```

#### Motivation

Accepting `&str` avoids unnecessary allocations on the caller side. Taking `String` when
ownership is needed makes the transfer explicit and avoids hidden `.to_string()` calls
inside the function. The `impl Into<String>` pattern is convenient for public APIs but adds
monomorphisation cost, so it should be used judiciously.

### STYLE-0019: Type-safe variant selection

#### Situation

Routing behaviour based on a value that comes from a fixed, known set of alternatives (e.g.,
AI provider, output format, environment name).

#### Guidance

Model the set of alternatives as an **enum** and match on it. Do not use string comparisons to
branch on known variants:

```rust
// Bad — brittle, easy to typo, no exhaustiveness checking
let provider_name = if provider.to_lowercase().contains("openai")
    || provider.to_lowercase().contains("ollama")
{
    "openai"
} else {
    "claude"
};

// Good — the compiler enforces every variant is handled
enum AiProvider {
    Claude,
    Bedrock,
    OpenAi,
    Ollama,
}

fn resolve_provider(raw: &str) -> Result<AiProvider> {
    match raw.to_lowercase().as_str() {
        s if s.contains("openai") => Ok(AiProvider::OpenAi),
        s if s.contains("ollama") => Ok(AiProvider::Ollama),
        s if s.contains("bedrock") => Ok(AiProvider::Bedrock),
        _ => Ok(AiProvider::Claude),
    }
}
```

**Parse once, branch on the enum everywhere else.** The string-to-enum conversion should happen
at the boundary (CLI parsing, config loading, environment variable reading). All downstream code
receives the enum and uses `match`, which the compiler checks for exhaustiveness.

This applies to any situation where the set of values is known at compile time — not just
providers. Output formats, log levels, feature flags, and similar categories all benefit from
the same pattern.

#### Motivation

String-based dispatching defeats Rust's exhaustiveness checking. When a new variant is added,
the compiler cannot tell you which `if` chains need updating — you discover missed branches at
runtime. An enum makes invalid states unrepresentable and turns forgotten branches into compile
errors. The "parse at the boundary" pattern also eliminates repeated `.to_lowercase().contains()`
calls scattered across the codebase.
