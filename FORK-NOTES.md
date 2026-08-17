# OpenFang v0.6.9 — patched fork

This is [RightNow-AI/openfang](https://github.com/RightNow-AI/openfang) at `v0.6.9` with the
defects below fixed. Upstream's `main` has not moved since 2026-05-12 while 41 pull requests
sit unmerged, so these fixes live here rather than there.

No defect count is given in this sentence on purpose. An earlier copy of these notes said
"fourteen defects fixed" and kept saying it through two more rounds of work, which is how a
headline number becomes the least reliable line in a document. The authoritative list is the
reproduction registry, and it prints itself:

```bash
tests/fang/run.sh --list
```

Each row names the defect, the commit that fixed it, and whether it is fixed at all — `FANG-9`
is listed as `NO FIX` because it is not fixed. Read that output, not this paragraph.

Everything below was reproduced against the unpatched build first, fixed, then measured
again — the numbers in each section are before/after, not estimates.

## Why you might want this

The short version: **v0.6.9 tells you things that are not true, and does it quietly.**

If you run agents with a fallback model, and the primary fails, the answer comes from the
fallback and nothing says so. Your usage report attributes the tokens to the model that
answered nothing. A batch job silently mixes results from two models. That is how it hit us:
one document out of eleven came out half the expected size, and the cause was only found a
day later by comparing byte-per-section against its neighbours.

## What is fixed

### The response now says which model wrote it

Before, `POST /api/agents/{id}/message` returned four fields and none named a model. Three
routes — `GET /api/agents/{id}`, `/api/usage/by-model`, `/api/metrics` — did worse than stay
silent: they asserted the *configured* model as the one that ran. A cost report built on them
was confidently wrong.

Usage is now metered **per LLM call**, not per agent turn, because a single turn can be served
by two different models across iterations and their tokens cannot be split afterwards:

```json
{
  "response": "...",
  "model_used": "google/gemma-4-31b-it",
  "provider_used": "hyperfusion",
  "fallback": {"used": true, "calls": 1, "of": 2, "requested": "ark/glm-5.2",
               "served_by": ["google/gemma-4-31b-it"], "reason": "API error (502)"},
  "calls": [
    {"n": 0, "model": "google/gemma-4-31b-it", "requested": "ark/glm-5.2",
     "input_tokens": 202, "output_tokens": 22, "cost_usd": 0.000268}
  ]
}
```

The `fallback` object carries six fields, and `calls`/`of` are two of them: they say how many
of the turn's LLM calls a substitute served, which is the difference between a dead primary and
one that recovered part-way. Checked against `crates/openfang-types/src/usage.rs` —
`FallbackSummary` declares exactly `used`, `calls`, `of`, `requested`, `served_by`, `reason`.

Per-call sums add up to the turn totals, including cost. `/api/usage/by-model` credits the
model that actually ran. New Prometheus series `openfang_llm_calls_total`,
`openfang_llm_tokens_total` and `openfang_llm_fallback_calls_total` carry provider and model
labels; the pre-existing `openfang_tokens_total` is left untouched so existing dashboards and
alerts keep working.

Disclosure is on the blocking route, on SSE (`/message/stream`) and on the WebSocket.

### A fallback on another provider no longer sends your key to the wrong host

`[[fallback_models]]` without an explicit `base_url` inherited `[default_model].base_url` —
so a fallback declared for provider B was called at provider A's address, with B's key. The
result was a 500 carrying A's 401 body. It looked intermittent because it is harmless when
both providers happen to match.

The fallback now resolves its address from `[provider_urls]` like the primary does, and the
resolved address is baked in once so the `ModelNotFound` retry chain agrees with it.

### Runtime context stopped leaking into what agents write

The system prompt was assembled from markdown sections (`## Current Date`, `## Memory`,
`## Workspace`, ~20 of them). An agent writing a markdown document copies them into its
output — we found one knowledge-base document ending with a verbatim `## Current Date`
block, which then lives in your RAG corpus as a chunk that surfaces on unrelated queries.

Factual sections are now wrapped in tags the model does not reproduce as prose. Instructional
sections (safety, tool-call behaviour, operational guidelines) deliberately keep their
headings — heading structure is part of how a model reads text as a directive, and breaking
that costs more than the defect. The workspace-context cap now drops whole `<file>` blocks
instead of cutting one open, and says how many it dropped.

### Endpoints stopped claiming success they did not deliver

- `PUT /api/agents/{id}/update` parsed a manifest, discarded it and answered `200 acknowledged`.
  It now answers `501` and lists the routes that do work — including `PATCH /api/agents/{id}/config`,
  which takes 14 fields. We recreated an agent through DELETE + POST, losing its id and history,
  because the docs pointed at the wrong method.
- Config hot-reload logged `Config hot-reload applied: [ReloadChannels]` for actions it had
  only noticed. It now separates what was applied from what is deferred, and says a restart is
  needed. `ReloadProviderUrls` is honestly deferred: the boot config wins over the runtime
  catalog in address resolution, so the reload updates `/api/providers` and not the driver.
- `remove_custom_model` did not recompute `provider.model_count`, so the dashboard showed a
  stale number until a restart. Drift accumulated over consecutive deletes — 36 shown against
  13 real, in our case.

### Secrets stopped reaching places they should not

- The Telegram bot token was written to the log on every network error, because `reqwest`
  attaches the request URL to connection errors and the Bot API puts the token *in* the URL.
  Now stripped at every send site.
- Provider error bodies quote the key they rejected. That raw body was handed to the caller in
  `fallback.reason` and onward to `/v1/chat/completions`, SSE and WS. It now goes through the
  same sanitiser the ordinary error path already used.

### Channel credentials stopped leaving the machine

Two paths, both closed:

- **The Telegram file URL contains the bot token** — that is how the Bot API works — and it
  was pasted straight into the prompt: `[User sent a file (x.pdf): https://…/file/bot<TOKEN>/…]`.
  So every file, photo or voice message sent to your bot put the token in the body of a request
  to your LLM provider, and in the session history on disk. Reproduced against a fake Bot API:
  the token appeared twice — in the prompt, and in the model's own `web_fetch` call, because it
  tried to follow the URL. Now redacted before the text is built.
- **Six adapters leaked credentials through `reqwest` errors.** `reqwest` attaches the request
  URL to connection errors, and `dingtalk`, `messenger`, `flock`, `threema`, `wecom` and
  `gotify` put the credential in the URL. A network blip wrote it to the log. (`gotify` was
  wrongly believed clean earlier — its WebSocket path is fine, but `validate()` and
  `api_send_message()` go through reqwest.)

If you have been running any of these channels, treat the credentials as exposed and rotate
them — this fix stops future leaks, it does not unpublish past ones.

### file_read stops lying about how much it gave you

`file_read` returns at most 30% of the context window. It always marked the cut, but there was
no `offset`/`limit` to read the rest, and the model's own answer would report the file's full
size as if it had read all of it. Measured: 78 483 of 117 517 bytes delivered, and a summary
written as though nothing was missing. That is a second, independent cause of thin output —
separate from the silent model substitution above.

Now: paging via `offset`/`limit`, and a header stating what was actually delivered, what
remains, and where to continue. The header is corrected at the truncation layer, because a
`limit` larger than the budget would otherwise promise bytes the model never received and send
it past the gap.

### openfang_tool_calls_total was always zero

The counter was declared, zeroed in three places, read — and never incremented. It exported to
Prometheus, looked healthy, and reported zero for every agent since the beginning. It now counts
the tool calls each response asks for. If you graphed it before, expect a series that was flat
at zero to start moving; the `HELP` text says so.

### A channel reload no longer leaves the old poller running

`ChannelAdapter::stop()` was implemented by all 43 channel adapters and called by none, so
every channel reload leaked its predecessor. Measured on Telegram: 5 `409 Conflict` responses
and 31 seconds of deafness per reload, because two pollers held overlapping `getUpdates`
long-polls. In the Discord and Slack adapters the leaked task also spun hot — its shutdown arm
could not tell a closed watch channel from an unchanged one, so `changed()` returning `Err`
looped instead of exiting.

`BridgeManager` now owns its adapters, which is what makes stop-before-drop an invariant rather
than a convention, and shutdown awaits each `stop()` under a deadline so one wedged adapter
cannot hold the process open. The drain is bounded, not unbounded: `stop_fast` exists for the
paths that must not wait.

### A turn that runs out of iterations returns its work

Hitting `max_iterations` used to produce HTTP 500 with no partial result and no usage
accounting at all — the tokens were spent and then discarded, which is the one failure mode
that costs money twice. The turn now comes back with its accounting intact and a notice that
says what happened, how many iterations were allowed, and which tool calls completed before
the limit. Tool calls are tracked with an explicit fate — completed, errored, or blocked with
the reason it never reached the tool — so the notice distinguishes work done from work
attempted.

### An empty provider response is no longer reported as an answer

A provider can return a valid response carrying no text at all. That came back as a successful
turn holding a sentence the runtime had written itself, which is indistinguishable from the
model having said it. The turn now fails, and the failure names what actually happened,
including the token counts, rather than being relabelled `Format` / "Request failed" by an
error classifier that never saw a failed request.

## Database migration

Schema v8 → v9 adds four nullable columns to `usage_events`, two indexes and one `UPDATE`.
No `DROP`, nothing rewritten.

**Rollback works and was tested end to end**, not merely reasoned about: the old binary starts
on a v9 database, reads every row, writes new ones, and re-upgrading is idempotent. One
cosmetic effect — the old binary stamps `user_version` back to 8 while the v9 columns remain;
re-upgrading fixes it.

Back up the volume before upgrading anyway. It is small (tens of MB) and it costs nothing:

```bash
docker run --rm -v openfang_openfang-data:/d -v "$PWD":/b alpine \
  tar czf /b/openfang-backup.tar.gz -C /d .
```

## Install

Same as upstream — the fork changes no build or runtime requirements.

```bash
git clone https://github.com/kyzdes/fang-upgrade.git
cd fang-upgrade
docker compose up -d --build
curl -s http://127.0.0.1:4200/api/health
```

A first build takes about 12 minutes on 4 cores. The `Dockerfile` carries BuildKit cache
mounts, so later builds after a one-file change are around 4 minutes rather than another 12.
For a faster first build at the cost of a slower binary:

```bash
docker compose build --build-arg LTO=false --build-arg CODEGEN_UNITS=16
```

Building needs rustc 1.91 or newer. `rust-version` in the workspace `Cargo.toml` says so
because it was measured: under `RUSTUP_TOOLCHAIN=1.88.0` the build stops with
`cranelift-assembler-x64@0.130.2 requires rustc 1.91.0`, from wasmtime. Note that
`rust-toolchain.toml` pins `channel = "stable"`, and rustup honours that over whatever
toolchain a Docker image happens to ship — so the `FROM rust:…` tag in the `Dockerfile` does
not pin your compiler, and a tag alone is not reproducibility.

### Two things worth knowing before you expose it

Upstream's `docker-compose.yml` publishes the dashboard on `0.0.0.0`, and **Docker's port
publishing bypasses UFW** — a firewall rule denying 4200 will not stop it. Bind to loopback or
to a private interface, and reach it through a tunnel or a reverse proxy:

```yaml
ports:
  - "127.0.0.1:4200:4200"
```

Upstream also declares `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` and friends as empty-but-present
environment variables. OpenFang reads *present* as *configured*: on an empty string it will
boot claiming an anthropic provider and auto-enable the OpenAI embedding driver, which means
text leaves the machine. Override the block rather than leaving the empty values in place.

`docker-compose.override.yml` in this repo does both, and it replaces those blocks instead of
extending them — which is the whole point, and is checkable in one command that builds nothing:

```bash
docker compose config
```

Expect an `environment` holding `OPENFANG_LISTEN` alone, and one published port carrying
`host_ip: 127.0.0.1`. Remove the `!override` tags and re-run it to see what is at stake: the
seven empty provider keys come back, and upstream's `0.0.0.0` publication reappears *beside*
the loopback one rather than being replaced.

Set `api_key` in `config.toml` — without it the API is open to anything that can reach the
port, and agents can run shell commands.

## Known issues, not fixed here

Kept honest on purpose — these are real and still open. The two entries that used to head this
list, the uncalled `ChannelAdapter::stop()` and the discarded accounting on `max_iterations`,
are gone from it because they were fixed; see the two sections above.

- **An agent still reports writes it never performed.** The phantom-action guard covers
  channel replies only, so a turn can claim a file was written when no write tool ran.
  `tests/fang/run.sh --list` marks `FANG-9` as `NO FIX`, and its reproduction is expected to
  stay red — a green FANG-9 means the repro broke, not that the defect went away.
- **The empty-response fix does not reach the two SSE surfaces.** The blocking route reports a
  no-text turn as a failure; the streaming surfaces still report nothing. Tracked as FANG-84,
  and `tests/fang/FANG-13.sh` prints it as a named gap rather than passing over it.

## Provenance

Every fix has a reproduction that fails on `v0.6.9` and passes here, under `tests/fang/`, with
a registry that `tests/fang/run.sh --list` prints.

**Those reproductions stay out of the public mirror.** They carry output from our own
instance — agent ids, session ids, prompts — and that is the reason, not tidiness. If you are
publishing from this branch, `tests/fang/` is not part of what gets published.

Licence and copyright are upstream's: Apache-2.0 OR MIT.

## CI

Upstream's `.github/workflows/ci.yml` and `release.yml` are kept unmodified, so a diff against
upstream stays readable. This fork's own checks live beside them in `fork-ci.yml`, which runs
`fmt`, `clippy -D warnings` and the workspace test suite on the `ours` branch.

Worth running locally either way:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

An earlier copy of these notes warned that upstream CI was red on `feishu.rs`
(`clippy::question_mark`) and that a red run was "not necessarily your doing". That warning is
withdrawn rather than repeated: `cargo clippy -p openfang-channels --all-targets -- -D warnings`
is clean on this branch. It is not restated for the whole workspace, because the workspace
clippy run was not reproduced here — `openfang-desktop` needs GTK development packages that
`fork-ci.yml` installs and a bare container does not.
