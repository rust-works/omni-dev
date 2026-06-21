# Transcript subcommand

`omni-dev transcript` fetches captions and transcripts from external media
platforms. YouTube is the only source today; the CLI namespace and the
underlying library are designed so additional sources (Vimeo, podcast RSS,
generic VTT/SRT URLs, …) can be added without restructuring.

## CLI usage

The provider is the first positional argument so per-source flags and help
output stay clean:

```bash
# Fetch captions for an unrestricted, captioned video.
omni-dev transcript youtube fetch https://www.youtube.com/watch?v=jNQXAC9IVRw

# Same video, written to disk as WebVTT.
omni-dev transcript youtube fetch jNQXAC9IVRw \
  --format vtt --output me-at-the-zoo.vtt

# Fall through to auto-generated captions when no manual track exists.
omni-dev transcript youtube fetch <url> --auto

# Synthesise a translated track when no native track matches the language.
omni-dev transcript youtube fetch <url> --lang fr --translate fr

# List every caption track on a video, with `kind` distinguishing manual
# from auto-generated.
omni-dev transcript youtube list-langs <url>

# Show top-level metadata (title, channel, duration, available languages).
omni-dev transcript youtube info <url> --output json

# Sync every captioned video in one or more channels to a directory,
# incrementally. Writes a transcript and a metadata sidecar per video.
omni-dev transcript youtube sync @RickAstleyYT --out ./transcripts --auto

# Re-fetch metadata sidecars older than two days (missing ones are always
# backfilled; this also refreshes stale ones).
omni-dev transcript youtube sync @RickAstleyYT --out ./transcripts \
  --refresh-metadata-older-than "2 days ago"
```

### `fetch` flags

| Flag                | Default | Effect                                                                     |
|---------------------|---------|----------------------------------------------------------------------------|
| `--lang <code>`     | `en`    | Preferred language. Prefix fallback applies — `en` matches `en-US`.        |
| `--format <fmt>`    | `srt`   | One of `srt`, `vtt`, `txt`, `json`.                                        |
| `--auto`            | off     | Allow falling through to auto-generated (ASR) captions.                    |
| `--translate <lang>`| —       | Synthesise a translated track in `<lang>` when no native track matches.    |
| `-o`, `--output`    | stdout  | Write the rendered transcript to a file instead of stdout.                 |

### `sync` output layout and metadata sidecars

`sync` enumerates a channel's videos and writes, per video, into
`<out>/<channel-id>/`:

- a **transcript** `<video-id>.<lang>.<format>` (e.g. `dQw4w9WgXcQ.en.srt`); and
- a **metadata sidecar** `<video-id>.meta.yaml` — one per video,
  language-independent, written atomically (temp file + rename).

"Already synced" is filesystem state: an existing transcript file means the
transcript is skipped. Sidecars are planned by a separate **directory scan** of
`<out>/<channel-id>/` (every transcript file is a synced video; `*.meta.yaml`
and in-flight `.*`/`*.tmp` files are ignored), so backfill and refresh cover the
full set of already-synced videos **without** re-enumerating the channel
(`--full`) and **without** touching the bot-gated transcript path. A video with
no usable transcript leaves no anchor file and so gets no sidecar.

Metadata is fetched with a single **WEB-client** `/player` call — un-gated, no
`visitorData` bootstrap — which carries the `microformat` block (publish date,
like count, category) that the `ANDROID_VR` transcript path lacks. Metadata
failures are tallied separately and never block or fail transcript syncing.

A sidecar looks like:

```yaml
schema: 1
video_id: dQw4w9WgXcQ
title: Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)
channel: Rick Astley
channel_id: UCuAXFkgsw1L7xaCfnd5JJOw
channel_url: http://www.youtube.com/@RickAstleyYT
category: Music
published_at: 2009-10-24T23:57:33-07:00
duration_seconds: 213
description: |
  The official video for "Never Gonna Give You Up" by Rick Astley.
keywords:
  - rick astley
view_count: 1781429760
like_count: 19148727
is_live_content: false
is_unlisted: false
thumbnail_url: https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg
fetched_at: 2026-06-11T03:12:45Z
```

`fetched_at` (UTC) is the snapshot time for the point-in-time `view_count` /
`like_count`, and the staleness key for refresh. `schema: 1` versions the
format. `microformat`-sourced fields (`like_count`, `category`, `published_at`,
`is_unlisted`, …) are omitted when absent — `like_count` when ratings are
disabled, all of them for private/removed videos (the sidecar is then written
from `videoDetails` alone).

#### `sync` metadata flags

| Flag                                  | Default | Effect                                                                                          |
|---------------------------------------|---------|------------------------------------------------------------------------------------------------|
| `--refresh-metadata-older-than <spec>`| —       | Re-fetch sidecars whose `fetched_at` predates `<spec>`. Missing sidecars are always backfilled. |

`<spec>` accepts an absolute date (`YYYY-MM-DD`, midnight UTC), a full RFC 3339
timestamp, or a relative spec `<N> <unit>[s] ago` (units: `minute`, `hour`,
`day`, `week`, `month`, `year`) resolved against now. Without the flag, no
refresh occurs; missing sidecars are still downloaded. An invalid spec errors at
plan time.

### Locator forms

`<url>` accepts any of:

- `https://www.youtube.com/watch?v=<id>` (extra query params ignored)
- `https://youtu.be/<id>` (with optional trailing query / fragment)
- `https://www.youtube.com/shorts/<id>`
- `https://www.youtube.com/embed/<id>`
- A bare 11-character video ID like `jNQXAC9IVRw`

### Errors

Failures surface as typed variants of
[`TranscriptError`](https://docs.rs/omni-dev/latest/omni_dev/transcript/enum.TranscriptError.html)
rather than generic HTTP errors:

| Variant                       | When                                                                  |
|-------------------------------|-----------------------------------------------------------------------|
| `InvalidLocator`              | URL did not parse, or bare ID failed validation.                      |
| `LanguageNotFound`            | No track matched `--lang` (manual or, with `--auto`, ASR).            |
| `AutoCaptionsRequireOptIn`    | Only ASR matched, but `--auto` was not passed.                        |
| `PlayabilityRefused`          | Age-gated, region-locked, removed, or login-required (carries status).|
| `MissingVisitorData`          | YouTube watch-page format drifted; bootstrap regex needs retuning.    |
| `ParseError`                  | InnerTube or json3 response did not match the expected shape.         |
| `Http`                        | Non-2xx response from YouTube.                                        |

## Library architecture

The library lives at [`src/transcript/`](../src/transcript/) and has no
`clap` dependency — it is reusable by other commands or external consumers.
The CLI in [`src/cli/transcript/`](../src/cli/transcript/) is a thin layer
that bridges `clap` argument parsing to library types.

```
src/
  transcript/                     # library: no clap
    cue.rs                        # Cue { start_ms, end_ms, text }
    error.rs                      # TranscriptError + Result alias
    source.rs                     # TranscriptSource trait + value types
    format.rs                     # Format enum + dispatch
    format/{srt,vtt,txt,json}.rs  # source-agnostic converters
    sources/
      youtube.rs                  # impl TranscriptSource for Youtube
      youtube/{url,player_response,timedtext,innertube,watch_page}.rs
  cli/transcript/                 # CLI: clap dispatch only
    mod.rs                        # TranscriptCommand + TranscriptSubcommands
    format.rs                     # CliFormat ↔ Format bridge
    youtube/{mod,fetch,info,list_langs}.rs
```

The trait contract:

```rust
#[async_trait]
pub trait TranscriptSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(url: &str) -> bool where Self: Sized;

    async fn fetch(&self, locator: &str, opts: &FetchOpts) -> Result<Transcript>;
    async fn list_languages(&self, locator: &str) -> Result<Vec<LanguageInfo>>;
    async fn info(&self, locator: &str) -> Result<MediaInfo>;
}
```

`matches` is `where Self: Sized` so it stays out of the `dyn` vtable —
sources can be used through `Box<dyn TranscriptSource>` (planned for a
future `omni-dev transcript fetch <url>` auto-detect path).

Format converters take `&[Cue]` and never reach back into a source, so
they are reused as-is by every implementation.

## Adding a new source

Adding a source is intentionally small: one library module, one CLI
module, two single-line additions to enums. The trait, the value types,
and the format converters are not touched. The recipe below walks through
adding a stub `vimeo` source.

### 1. Library module

Create `src/transcript/sources/vimeo.rs`:

```rust
//! Vimeo TranscriptSource — stub.

use async_trait::async_trait;

use crate::transcript::error::Result;
use crate::transcript::source::{
    FetchOpts, LanguageInfo, MediaInfo, Transcript, TranscriptSource,
};

/// Vimeo transcript source.
pub struct Vimeo {
    http: reqwest::Client,
}

impl Vimeo {
    /// Construct a Vimeo source with default HTTP settings.
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().build()?,
        })
    }
}

#[async_trait]
impl TranscriptSource for Vimeo {
    fn name(&self) -> &'static str {
        "vimeo"
    }

    fn matches(url: &str) -> bool {
        url.contains("vimeo.com/")
    }

    async fn fetch(&self, _locator: &str, _opts: &FetchOpts) -> Result<Transcript> {
        todo!("call Vimeo's text-tracks API, parse to Vec<Cue>")
    }

    async fn list_languages(&self, _locator: &str) -> Result<Vec<LanguageInfo>> {
        todo!()
    }

    async fn info(&self, _locator: &str) -> Result<MediaInfo> {
        todo!()
    }
}
```

Register the module by adding one line to
[`src/transcript/sources.rs`](../src/transcript/sources.rs):

```rust
pub mod vimeo;
```

That's the entire library surface. Note what is *not* needed:

- No new error variants — `TranscriptError` already covers parse / HTTP /
  language-not-found / playability-refused.
- No new format converters — the four shipped formats consume `&[Cue]`.
- No changes to `TranscriptSource`, `Transcript`, `Cue`, or `FetchOpts`.

### 2. CLI module

Create `src/cli/transcript/vimeo/mod.rs` mirroring the YouTube layout
(`fetch.rs`, `info.rs`, `list_langs.rs`). Each subcommand instantiates the
source and dispatches:

```rust
//! Vimeo transcript subcommands.

pub mod fetch;
pub mod info;
pub mod list_langs;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
pub struct VimeoCommand {
    #[command(subcommand)]
    pub command: VimeoSubcommands,
}

#[derive(Subcommand)]
pub enum VimeoSubcommands {
    Fetch(fetch::FetchCommand),
    ListLangs(list_langs::ListLangsCommand),
    Info(info::InfoCommand),
}

impl VimeoCommand {
    pub async fn execute(self) -> Result<()> {
        match self.command {
            VimeoSubcommands::Fetch(cmd) => cmd.execute().await,
            VimeoSubcommands::ListLangs(cmd) => cmd.execute().await,
            VimeoSubcommands::Info(cmd) => cmd.execute().await,
        }
    }
}
```

The individual subcommand structs follow the same shape as
[`src/cli/transcript/youtube/fetch.rs`](../src/cli/transcript/youtube/fetch.rs)
— construct the source via `Vimeo::new()?`, call the trait method, and
hand the result to the same `Format::render` and `print_table` helpers
the YouTube subcommands already use.

### 3. Top-level wiring

Two single-line edits in
[`src/cli/transcript/mod.rs`](../src/cli/transcript/mod.rs):

```rust
pub mod vimeo;

pub enum TranscriptSubcommands {
    Youtube(youtube::YoutubeCommand),
    Vimeo(vimeo::VimeoCommand),  // ← new
}

impl TranscriptCommand {
    pub async fn execute(self) -> Result<()> {
        match self.command {
            TranscriptSubcommands::Youtube(cmd) => cmd.execute().await,
            TranscriptSubcommands::Vimeo(cmd) => cmd.execute().await,  // ← new
        }
    }
}
```

After landing CLI changes, run the
[`update-snapshots`](../.claude/skills/update-snapshots/SKILL.md) skill to
refresh
[`tests/snapshots/integration_test__help_all_output.snap`](../tests/snapshots/integration_test__help_all_output.snap).

### 4. Tests

Per [STYLE-0009](STYLE_GUIDE.md), tests live in `#[cfg(test)] mod tests`
inside each source file. The YouTube source ships a two-layer pattern that
ports cleanly to a new source:

1. **Offline parsers** — fixture-driven `#[test]` cases for URL parsing,
   API-response parsing, track selection, and any source-specific format
   variant. Fixtures live next to the source in `fixtures/`.
2. **HTTP layer** — `#[tokio::test]` cases that drive
   `Source::with_base_url(server.uri())` against a `wiremock::MockServer`
   serving the source's endpoints.

See [`src/transcript/sources/youtube.rs`](../src/transcript/sources/youtube.rs)
for the worked layout, including `expect(1)` mocks that pin caching
behaviour and golden-output round-trips that compare HTTP and offline
pipelines against the same `.srt` reference.

An online integration test against the live platform is gated on
`#[cfg(online_tests)]` (declared in `Cargo.toml`'s `[lints.rust]`), so
`cargo test` and `cargo test --all-features` neither compile nor run it.
Operators run it manually with
`RUSTFLAGS='--cfg online_tests' cargo test online_<source>_against_public_video`.

## YouTube refresh signals

The YouTube source pins client constants that drift over months as
YouTube tightens its bot-detection signals. When `/player` starts
returning empty or refused responses for known-healthy videos, refresh:

- [`CLIENT_VERSION`](../src/transcript/sources/youtube/innertube.rs) and
  the matching version token in
  [`USER_AGENT`](../src/transcript/sources/youtube.rs) — bump to the value
  currently shipped by the Oculus YouTube app.
- [`INNERTUBE_API_KEY`](../src/transcript/sources/youtube/innertube.rs) —
  refresh if the ANDROID-family key starts being rejected.
- The `visitorData` regex in
  [`watch_page.rs`](../src/transcript/sources/youtube/watch_page.rs) — if
  the watch-page `ytcfg.set` block changes shape, the bootstrap will
  surface `MissingVisitorData` rather than silently fall through.

The `BROWSER_USER_AGENT` used for the watch-page bootstrap is independent
of the InnerTube User-Agent — they target different YouTube surfaces and
must not be conflated.

The metadata sidecar path (`sync`) reads a different surface again: the
**WEB-client** `/player` response and its `microformat.playerMicroformatRenderer`
block, parsed in
[`metadata.rs`](../src/transcript/sources/youtube/metadata.rs). This shape can
drift independently of the `ANDROID_VR` constants above. If sidecars start
coming back empty or [`metadata::parse`] begins failing for known-healthy
videos, re-check the field paths there (`publishDate`, `likeCount`, the
`microformat` nesting) against a live WEB response — count fields in particular
have flipped between JSON string and number forms before, which the parser
tolerates but is the first thing to verify.

[`metadata::parse`]: ../src/transcript/sources/youtube/metadata.rs
