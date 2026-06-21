# Recipe: querying your own Facebook data through the browser bridge

This recipe shows how to use the
[`omni-dev browser bridge`](../browser-bridge.md) to page **your own** Facebook
profile timeline via Facebook's internal Relay/GraphQL API — borrowing the
authenticated session of a logged-in browser tab without exfiltrating its
cookies or tokens.

> **Your own data only.** This drives your already-authenticated session against
> your **own** account. It is not for scraping anyone else's data, and you are
> responsible for staying within
> [Facebook's Terms of Service](https://www.facebook.com/terms.php). For a stable
> archive, prefer Facebook's official **"Download Your Information"** export; this
> recipe is for ad-hoc, programmatic access.

## Just want the data? Use the built-in command

This whole procedure is already packaged as a built-in harvester — see
[Harvesting your own data](../browser-bridge.md#harvesting-your-own-data-best-effort):

```bash
omni-dev browser bridge harvest facebook posts --output my-posts.jsonl
```

Read on only if you want to **understand or debug** what that command does — for
example when Facebook rotates something and the command fails with a staged
error naming the step that drifted. The manual recipe below is the procedure the
command automates, step for step. (The reference implementation lives in
[`src/browser/harvest/facebook.rs`](../../src/browser/harvest/facebook.rs); it is
the authoritative source for every pattern and field path used here.)

> **Best-effort against undocumented internals.** Facebook's persisted-query
> `doc_id`s, relay-provider flags, page structure, and response shape are
> undocumented and rotate frequently. **Re-harvest every volatile value on each
> run; never hardcode a `doc_id` or token.** This recipe can break whenever
> Facebook changes its internals.

## Why this needs the bridge (and two cross-origin flags)

Facebook is a Relay/GraphQL SPA: the data you want is behind *persisted queries*
(server-stored operations addressed by a numeric `doc_id`) that the page replays
with CSRF tokens minted into its own HTML. You cannot reproduce that session with
`curl`. The bridge runs the requests **inside the authenticated tab**, so the
session cookies and CSRF tokens are the page's own.

Two of the three steps are plain same-origin requests. The middle step —
discovering the pagination `doc_id` — reaches a **cross-origin** CDN bundle and
so needs both cross-origin flags introduced for exactly this case:

- `--allow-origin <url>` ([#918](https://github.com/rust-works/omni-dev/issues/918))
  clears the bridge's **server-side** outbound-origin guard (relative URLs only by
  default; without it the bridge returns `403` *before* issuing the fetch).
- `--credentials omit` ([#920](https://github.com/rust-works/omni-dev/issues/920))
  clears the **browser's** CORS gate: `static.xx.fbcdn.net` serves
  `Access-Control-Allow-Origin: *`, which the browser refuses to expose to a
  *credentialed* request. `omit` sends no cookies — fine here because the bundle
  is public and anonymous.

Neither flag is needed for the same-origin calls in steps 1 and 3.

## Prerequisites

- A running bridge — either `omni-dev browser bridge serve` or the
  [daemon](../browser-bridge.md#running-under-the-daemon) — with the omni-dev
  browser snippet pasted into a tab **already logged in to
  `https://www.facebook.com`**. Verify the tab is connected:

  ```bash
  omni-dev browser bridge request --url /__bridge/status   # or: curl … /__bridge/status
  ```

- A build that has `request --allow-origin` (#918) and `request --credentials`
  (#920).
- `OMNI_BRIDGE_TOKEN` exported to the session token printed by `serve` (under the
  daemon the thin client discovers the token file automatically — see
  [Token discovery](../browser-bridge.md#token-discovery-no-foreground-stdout)).
- `jq` for slicing JSON out of the response envelopes.
- If several tabs are connected, add `--target <id|origin>` to every command (see
  [Routing to a specific tab](../browser-bridge.md#routing-to-a-specific-tab)).

The `request` thin client prints the response **envelope** as JSON; the upstream
body is the envelope's `.body` string — `jq -r .body` pulls it out.

## Step 1 — harvest session tokens (same-origin)

Fetch the profile shell and keep its HTML; the values you need are inlined in the
page's `ServerJS`/preloader blocks:

```bash
omni-dev browser bridge request --url /me --header "Accept: text/html" \
  | jq -r .body > me.html
```

Pull these out of `me.html` (the patterns below are the ones the reference
harvester greps for):

| Value      | Where in `me.html` |
|------------|--------------------|
| `fb_dtsg`  | `"DTSGInitialData",[],{"token":"<TOKEN>"}` |
| `lsd`      | `"LSD",[],{"token":"<TOKEN>"}` |
| `USER_ID`  | `"USER_ID":"<digits>"` |
| initial `variables` | the `"variables":{…}` object next to the `ProfileCometTimelineFeedQuery` preloader — it carries the **relay-provider flags** you reuse in step 3 |
| initial `doc_id` | `"queryID":"<digits>"` (fall back to `"doc_id":"<digits>"`) in the same `ProfileCometTimelineFeedQuery` block |

The relay-provider flags in that `variables` object (e.g. assorted `__relay_*` /
provider booleans) change over time — carry them through verbatim rather than
trying to reconstruct them.

## Step 2 — discover the pagination `doc_id` (cross-origin, anonymous)

**The key gotcha:** the initial `ProfileCometTimelineFeedQuery` returns only ~3
posts and **ignores the cursor**, so it can never page your history. Pagination
uses a *different* persisted query, `ProfileCometTimelineFeedRefetchQuery`, whose
`doc_id` is **not** in `/me` — it lives in a lazily-loaded JavaScript bundle on
`https://static.xx.fbcdn.net`.

Collect the bundle URLs `me.html` references and fetch them cross-origin with
**both** flags (see [the rationale above](#why-this-needs-the-bridge-and-two-cross-origin-flags)):

```bash
# Bundle URLs referenced by the shell (de-duplicated, in first-seen order):
grep -oE 'https://static\.xx\.fbcdn\.net/[^"'\'' )]+?\.js[^"'\'' )]*' me.html \
  | awk '!seen[$0]++' > bundles.txt

# Fetch a bundle through the bridge, cross-origin + anonymous:
omni-dev browser bridge request \
  --allow-origin https://static.xx.fbcdn.net \
  --credentials omit \
  --url "$(head -n1 bundles.txt)" | jq -r .body > bundle.js
```

Grep each bundle for the refetch persisted-operation module and read its
exported id:

```
__d("ProfileCometTimelineFeedRefetchQuery_facebookRelayOperation",[],
    (function(...){a.exports="<DOC_ID>"}),null);
```

```bash
grep -oE 'ProfileCometTimelineFeedRefetchQuery_facebookRelayOperation".{0,2000}' bundle.js \
  | grep -oE 'exports[[:space:]]*=[[:space:]]*"[0-9]+"' | head -n1
```

That `<DOC_ID>` is the pagination query id. The `doc_id` rotates — you may have
to try several bundles before one contains the marker, and you must re-discover it
on every session.

## Step 3 — paginate (same-origin GraphQL POST)

POST `application/x-www-form-urlencoded` to `/api/graphql/` (same origin, so no
cross-origin flags). The body fields and headers below mirror
`Harvester::graphql_body` / `run_page` in the reference harvester.

**Body fields:**

| Field | Value |
|-------|-------|
| `av` | `<USER_ID>` |
| `__a` | `1` |
| `fb_dtsg` | harvested token |
| `lsd` | harvested token |
| `fb_api_caller_class` | `RelayModern` |
| `fb_api_req_friendly_name` | `ProfileCometTimelineFeedRefetchQuery` |
| `variables` | JSON — see below |
| `server_timestamps` | `true` |
| `doc_id` | `<DOC_ID>` from step 2 |

**Headers:** `content-type: application/x-www-form-urlencoded`,
`x-fb-friendly-name: ProfileCometTimelineFeedRefetchQuery`, `x-fb-lsd: <lsd>`.

**`variables` for the refetch query** = the initial provider flags from step 1,
**minus `userID`**, **plus**:

```json
{ "id": "<USER_ID>", "count": 10, "cursor": "<END_CURSOR>" }
```

On the very first page you have no cursor yet. Either issue the initial
`ProfileCometTimelineFeedQuery` once (friendly name + initial `doc_id`,
`variables` = provider flags + `count` + `cursor: null`) to obtain the first
`end_cursor`, then switch to the refetch query for every subsequent page.

### Reading the response (stream/defer JSONL)

The response is **not** one JSON document — it is newline-delimited, one JSON
object per line (Facebook's `@stream`/`@defer` incremental delivery):

- the first line + each `@stream` line carry post `edges` (either a full `edges`
  array, or a single streamed `{node, cursor}`);
- a trailing `@defer` line carries
  `page_info.{end_cursor, has_next_page}`.

Mind the nesting difference: the **refetch** response nests under
`data.node.timeline_list_feed_units`, whereas the **initial** query nests under
`data.user.timeline_list_feed_units`.

**Loop:** parse every line, collect `edges`, take `page_info.end_cursor`, feed it
back as the next `cursor`, and repeat until `has_next_page` is `false`. Stop if
the cursor ever fails to advance (a safety guard against an infinite loop on
drift).

### Field map

Each post's fields live at these paths under the edge `node` (this is the full
map the reference harvester reads — `text`/`time`/`url` plus `id` and a shared
external link):

| Field | Path under `node` |
|-------|-------------------|
| id | `id` |
| text | `comet_sections.content.story.comet_sections.message.story.message.text` |
| time | `comet_sections.context_layout.story.comet_sections.metadata[0].story.creation_time` |
| url | `comet_sections.content.story.wwwURL` |
| shared_link | `attachments[0].styles.attachment.story_attachment_link_renderer.attachment.web_link.url` |

### Worked drain loop

Stitching the three steps with the `request` thin client and `jq` (assumes
`FB_DTSG`, `LSD`, `USER_ID`, `VARS` (the provider flags as JSON, with `userID`
removed), and `DOC_ID` are already harvested from steps 1–2):

```bash
CURSOR=""        # empty on the first refetch page; seed from the initial query
while :; do
  # Build the refetch variables: provider flags + id/count/cursor.
  variables=$(jq -cn --argjson base "$VARS" --arg id "$USER_ID" --arg cur "$CURSOR" \
    '$base + {id: $id, count: 10} + (if $cur == "" then {} else {cursor: $cur} end)')

  body=$(jq -rn \
    --arg av "$USER_ID" --arg dtsg "$FB_DTSG" --arg lsd "$LSD" \
    --arg fn ProfileCometTimelineFeedRefetchQuery --arg vars "$variables" --arg doc "$DOC_ID" \
    '[ "av=\($av)", "__a=1", "fb_dtsg=\($dtsg|@uri)", "lsd=\($lsd|@uri)",
       "fb_api_caller_class=RelayModern", "fb_api_req_friendly_name=\($fn)",
       "variables=\($vars|@uri)", "server_timestamps=true", "doc_id=\($doc)" ] | join("&")')

  page=$(omni-dev browser bridge request --method POST --url /api/graphql/ \
    --header "content-type: application/x-www-form-urlencoded" \
    --header "x-fb-friendly-name: ProfileCometTimelineFeedRefetchQuery" \
    --header "x-fb-lsd: $LSD" \
    --body "$body" | jq -r .body)

  # Each line is one JSON object; collect post nodes and emit them.
  printf '%s\n' "$page" | jq -c '
    .. | objects | select(.timeline_list_feed_units?).timeline_list_feed_units.edges[]?.node
    | { id,
        text: .comet_sections.content.story.comet_sections.message.story.message.text,
        time: .comet_sections.context_layout.story.comet_sections.metadata[0].story.creation_time,
        url:  .comet_sections.content.story.wwwURL }'

  # Pull the deferred page_info from whichever line carries it.
  CURSOR=$(printf '%s\n' "$page" | jq -r '.. | objects | select(.page_info?).page_info.end_cursor // empty' | tail -n1)
  HASNEXT=$(printf '%s\n' "$page" | jq -r '.. | objects | select(.page_info?).page_info.has_next_page // empty' | tail -n1)
  [ "$HASNEXT" = "true" ] && [ -n "$CURSOR" ] || break
done
```

This is illustrative — the built-in `harvest facebook posts` command handles
retries on `504`/token rotation, dedup, `--since`/`--limit`, and resumable state
for you.

## Caveats

- **Your own data only**, within Facebook's ToS (see the banner at the top).
- **`doc_id`s and relay-provider flags rotate** — re-harvest them every session;
  never hardcode.
- **The initial query ignores the cursor** and caps at ~3 posts — this is the
  gotcha that sends people down the refetch path. Pagination *must* use
  `ProfileCometTimelineFeedRefetchQuery`.
- **Multiple connected tabs** require `--target <id|origin>` on every request.
- **Best-effort:** this drives undocumented internals and can break whenever
  Facebook changes them. For a stable archive, use "Download Your Information".

## See also

- [Browser Bridge guide](../browser-bridge.md) — the two planes, the `request`
  thin client, the security model, and the built-in `harvest` command.
- [ADR-0036](../adrs/adr-0036.md) — the confused-deputy trust boundary and
  dual-plane default-closed authentication.
- Issues [#918](https://github.com/rust-works/omni-dev/issues/918)
  (`--allow-origin`), [#920](https://github.com/rust-works/omni-dev/issues/920)
  (`--credentials`), and [#922](https://github.com/rust-works/omni-dev/issues/922)
  (this recipe).
