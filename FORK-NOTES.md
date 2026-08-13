# OpenFang v0.6.9 — patched fork

This is [RightNow-AI/openfang](https://github.com/RightNow-AI/openfang) at `v0.6.9` with
nine defects fixed. Upstream's `main` has not moved since 2026-05-12 while 41 pull requests
sit unmerged, so these fixes live here rather than there.

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
  "fallback": {"used": true, "requested": "ark/glm-5.2",
               "served_by": ["google/gemma-4-31b-it"], "reason": "API error (502)"},
  "calls": [
    {"n": 0, "model": "google/gemma-4-31b-it", "requested": "ark/glm-5.2",
     "input_tokens": 202, "output_tokens": 22, "cost_usd": 0.000268}
  ]
}
```

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
git clone https://github.com/kyzdes/openfang-patched.git
cd openfang-patched
docker compose up -d --build
curl -s http://127.0.0.1:4200/api/health
```

A first build takes about 12 minutes on 4 cores. The `Dockerfile` carries BuildKit cache
mounts, so later builds after a one-file change are around 4 minutes rather than another 12.
For a faster first build at the cost of a slower binary:

```bash
docker compose build --build-arg LTO=false --build-arg CODEGEN_UNITS=16
```

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

Set `api_key` in `config.toml` — without it the API is open to anything that can reach the
port, and agents can run shell commands.

## Known issues, not fixed here

Kept honest on purpose — these are real and still open:

- `file_read` truncates a file to a fraction of the context window and does not tell the agent.
  We measured a model receiving 66.8% of a transcript and reporting on it as if complete. This
  is a second, independent cause of thin output.
- `ChannelAdapter::stop()` is implemented by all 43 channel adapters and called by none, so a
  channel reload leaks the old poller. Measured on Telegram: 5 conflicts and 31 seconds of
  deafness per reload. In the Discord and Slack adapters the leaked task also spins hot.
- A turn that hits `max_iterations` loses its usage accounting entirely, and returns HTTP 500
  with no partial result.
- The Telegram file URL, which contains the bot token, is pasted into the LLM prompt when a
  user sends a file — so the token reaches your provider and the session store.
- Upstream's own CI is red on `feishu.rs` (`clippy::question_mark`), unrelated to these changes.

## Provenance

Every fix has a reproduction that fails on `v0.6.9` and passes here, kept in the working fork
at [kyzdes/openfang](https://github.com/kyzdes/openfang) under `tests/fang/`. They are left out
of this repository because they contain output from our own instance.

Licence and copyright are upstream's: Apache-2.0 OR MIT.

## A note on CI

Upstream's `.github/workflows/` is not included here. Two reasons: the token used to publish
this repo lacks GitHub's `workflow` scope, and upstream's own CI is currently red on
`feishu.rs` (`clippy::question_mark`) regardless of these changes.

If you want it, copy it from upstream:

```bash
git remote add upstream https://github.com/RightNow-AI/openfang.git
git fetch upstream
git checkout upstream/main -- .github/workflows
```

The checks themselves are worth running locally:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
