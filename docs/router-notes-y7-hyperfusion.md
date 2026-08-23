# Two OpenAI-compatible routers: `y7router` and `hyperfusion`

Notes on two third-party routers that OpenFang can be pointed at. They exist because the
failure modes in section 3 are not in either router's own documentation, and each of them
returns HTTP 200 while doing the wrong thing.

**What used to be here and is not any more.** An earlier version of this file opened with
"read off a running instance" and carried the inventory of one: its agent roster with each
agent's model and fallback chain, its provider and model counts, and its deployed
`config.toml`. That is a map of somebody's working system, and a public repository is the
wrong place for it — it is more sensitive than a hostname, because it says what is worth
attacking and where. It was removed rather than obfuscated. What is left is about the two
routers, which are public services, and about how to configure any instance against them.

**Dates.** The router behaviour in section 3 and the model tables in section 2 were measured
on **2026-08-16** and are not re-measured here. Catalogues change without notice: treat both
tables as a starting point and re-run section 5 rather than trusting them.

**No credentials here.** Keys live in environment variables named below; the values are not in
this file, this repository, or any log it tells you to read.

---

## 1. Configuring the two routers

OpenFang ships **42 builtin providers** (`builtin_providers()` in
`crates/openfang-runtime/src/model_catalog.rs` — `grep -c 'id: "'` over that function returns
42). Neither router below is one of them; both are added by `[provider_urls]`. Most builtins
sit with `auth_status: missing` and no key, which is harmless but made `/api/providers`
misleading at a glance.

**This fork narrows both catalogue endpoints instead.** `/api/providers` and `/api/models`
now return the providers you actually have — the ones named in `config.toml`, the ones with a
credential detected, the local ones that need no credential — plus a short curated list of
well-known providers kept visible so they can still be set up from the dashboard. Pass
`?all=true` to either endpoint to get the unfiltered catalogue back for diagnostics.

Two counters, and they do not mean the same thing, which is why both exist:

| Endpoint | `total` | also |
|---|---|---|
| `/api/providers` | providers in *this response* | `catalog_total` — the whole catalogue |
| `/api/models` | models in the whole *catalogue* | `shown` / `shown_available` — this response |

`/api/models` keeps `total` and `available` counting the catalogue because that is what they
counted before, and a field that quietly changes meaning is worse than a field with an awkward
name.

**Do not read `auth_status: missing` as "42 dead entries".** Several builtins need no key at all
and report `not_required` rather than `missing` — the local ones (`ollama`, `vllm`, `lmstudio`,
`lemonade`) among them; `grep -B4 'key_required: false'` over `builtin_providers()` lists which.
An earlier version of this narrowing treated only `configured` as real and dropped them, which
removed a working local ollama and its models from both endpoints.

| Provider id | Base URL | Key env var | Models live |
|---|---|---|---|
| `hyperfusion` | `https://api.hyperfusion.io/v1` | `HYPERFUSION_API_KEY` | 9 |
| `y7router` | `https://router.y7.hk/v1` | `Y7ROUTER_API_KEY` | 15 |

Both are declared under `[provider_urls]`. A config pointed at them looks like this — it is an
example to copy, not a transcript of anyone's deployment:

```toml
api_key = "<this instance's own dashboard/API key — not a provider key>"

[default_model]
provider = "hyperfusion"
model = "google/gemma-4-31b-it"
api_key_env = "HYPERFUSION_API_KEY"
base_url = "https://api.hyperfusion.io/v1"

[provider_urls]
hyperfusion = "https://api.hyperfusion.io/v1"
y7router = "https://router.y7.hk/v1"
```

Everything else — which agent runs on which model, what it falls back to — lives in the agent
manifests, not in `config.toml`. That is the first thing people get wrong when they try to
reproduce a setup by copying the config file.

### Two traps in this file specifically

**Empty provider keys read as configured.** Upstream's compose file declares `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and friends as empty-but-present. OpenFang treats *present* as *configured*: on
an empty string it boots claiming an anthropic provider and enables the OpenAI embedding driver,
so text leaves the machine. Replace that block, do not extend it.

**`api_key` at the top is the dashboard/API key, not a provider key.** Without it the API is open
to anything that reaches the port, and agents can run shell commands. Note that `GET /api/agents`
answers `200` without a key *by design* — test authentication on a write, not on that read.

---

## 2. Models

Both routers report `input_cost_per_m` and `output_cost_per_m` as `0.0` and `tier: custom`. That is
what the routers publish, not a claim that inference is free — cost accounting through
`/api/usage/by-model` will read zero for every one of these models.

### `hyperfusion` — 9 models, all live, all tool-capable

| Model | Context | Max output |
|---|---|---|
| `google/gemma-4-31b-it` | 262 144 | 262 144 |
| `MiniMaxAI/MiniMax-M2.7` | 196 608 | 196 608 |
| `gonka/MiniMaxAI/MiniMax-M2.7` | 240 000 | 16 384 |
| `gonka/moonshotai/Kimi-K2.6` | 240 000 | 16 384 |
| `google/gemma-3-27b-it` | 131 072 | 131 072 |
| `openai/gpt-oss-120b` | 131 072 | 32 768 |
| `openai/gpt-oss-20b` | 131 072 | 131 072 |
| `qwen/qwen3-30b-a3b` | 131 072 | 131 072 |
| `qwen/qwen3-32b` | 40 960 | 40 960 |

These carry **real limits published by the router**, which is why `[default_model]` above points
here rather than at y7.

### `y7router` — 15 models

| Model | Context | Max output | Tools |
|---|---|---|---|
| `kimi/k3` | 1 000 000 | 32 768 | yes |
| `ark/deepseek-v4-pro` | 131 072 | 16 384 | yes |
| `ark/deepseek-v4-flash` | 131 072 | 16 384 | yes |
| `ark/glm-5.2` | 131 072 | 16 384 | yes |
| `ark/glm-4.7` | 131 072 | 16 384 | yes |
| `ark/gpt-oss-120b` | 131 072 | 16 384 | yes |
| `ds/deepseek-v4-pro` | 131 072 | 16 384 | yes |
| `ds/deepseek-v4-flash` | 131 072 | 16 384 | yes |
| `xiaomi/mimo-v2.5` | 131 072 | 16 384 | yes |
| `xiaomi/mimo-v2.5-pro` | 131 072 | 16 384 | yes |
| `z/glm-5.2` | 131 072 | 16 384 | yes |
| `wave/max` | 131 072 | 16 384 | **no** |
| `wave/medium` | 131 072 | 16 384 | **no** |
| `wave/fast` | 131 072 | 16 384 | **no** |
| `wave/ghost` | 131 072 | 16 384 | **no** |

Every context window here except `kimi/k3` is a **placeholder of 131 072** that OpenFang filled in
because the router publishes no limits. Do not plan around those numbers; only `kimi/k3` is measured.

---

## 3. What y7router does that will cost you a day if you don't know

This is the part worth reading twice. All of it was found by measurement.

**It ignores every OpenAI parameter it accepts.** `max_tokens`, `stop`, `seed`, `n`, `logprobs`
and `response_format` all return `200` and are silently dropped. Asking for 5 output tokens
produced **419**. Nothing in the response says the parameter was ignored. Practical consequence:
**you cannot bound output length or cost on this router.** If you need a ceiling, enforce it on
your side or use `hyperfusion`.

**Every request carries ~5 800 tokens of injected hidden prompt** before your own text. Budget for
it when you size context or estimate spend.

**`wave/*` silently ignores the `tools` array** — all four of them. Worse, `wave/fast` will claim
it called your function anyway. Never give a `wave/*` model an agent that depends on tools; it will
report success and do nothing.

**`ds/*` works non-streamed but emits zero `delta.content` chunks when streaming** — the whole
answer arrives in `reasoning_content`. A streaming client reading `delta.content` sees an empty
response and no error.

**Models the catalogue lists but that do not answer.** Historically `alibaba/*` and 13 of 14
`opencode/*` returned `429` (pool full), `z/glm-5.2` returned `502 access-denied`, and
`ark/deepseek-v3-2` returned an empty body with no `usage`. The live catalogue is now down to the
15 above, so most of those are simply gone — but `z/glm-5.2` is still listed. Probe before you
rely on it.

> `opencode/` here is a **y7router model-id prefix**, not a provider. Worth stating because the
> confusion has already cost something: a first attempt at narrowing the catalogue (section 1)
> put `"opencode"` in its list of provider ids, where it matched no builtin provider at all. It
> is out of that list now: `grep -rn opencode crates/` returns a single hit, and it is the
> comment on the regression test that keeps it out. A dead id in a filter list is invisible: it
> never errors, it just quietly makes the list mean less than it reads.

**`kimi/k3` is the one model here with measured behaviour:** needle-in-haystack retrieved at start,
middle and end up to ~300 000 words; `prompt_tokens` clamps at exactly 1 000 000; past ~10 MB of
payload it returns an empty body in ~5 seconds with no error.

### The general rule these add up to

A router that publishes no limits and honours no parameters will fail **quietly**. Every failure
mode above returns HTTP 200. Before trusting a new model on y7, send it one real request with tools
and one with streaming, and look at what actually came back — not at the status code.

---

## 4. Fallbacks

The roster that used to be here — which named agents ran which models with which fallback
chains — was the inventory this file no longer publishes. The **pattern** is worth keeping and
gives nothing away: **primary on y7router, fallback to hyperfusion.** The reasoning is in
section 3 — y7 is fast and free-form but unpredictable, so the safety net is the router that
publishes real limits. An agent that must not be substituted quietly gets no fallback at all.

A fallback entry names **both** provider and model. In a manifest:

```toml
model = { provider = "y7router", model = "ark/glm-5.2" }

[[fallback_models]]
provider = "hyperfusion"
model = "openai/gpt-oss-120b"
```

### Four things about fallback that are not in the docs

**It fails over on 429 too, not only on 5xx.** The documentation says rate limits are not a
failover trigger; the code disagrees — `FallbackDriver` in
`crates/openfang-runtime/src/drivers/fallback.rs` says so in as many words: "On failure
(including rate-limit and overload), moves to the next driver." Cited by symbol, not by line:
the line numbers this file used to carry had already drifted. If your
fallback provider is metered, a rate-limit storm on the primary will spend money there. Budget for it.

**A fallback entry used to inherit `[default_model].base_url`** — meaning a fallback naming a
different provider still went to the default provider's endpoint. **This fork fixes that**; stock
v0.6.9 does not. On stock, the workaround is to declare the fallback provider explicitly in
`[provider_urls]` and never rely on inheritance.

**An unknown model gets an assumed 200 000-token context window** —
`DEFAULT_CONTEXT_WINDOW` in `crates/openfang-runtime/src/agent_loop.rs`, checked to still be
`200_000` — regardless of what it can actually take. Combined with y7's placeholder windows, this means context
budgeting on that router is guesswork unless you measured the model yourself.

**Which model actually served the turn is visible in this fork.** The response carries
`model_used`, a per-call `calls[]` array and a `fallback` object with six fields. On stock v0.6.9 a
fallback-served turn looks identical to a normal one — you cannot tell from the response that a
substitution happened, and the only trace is in the daemon log.

---

## 5. Setting up an instance against these routers

1. Put the two keys in the environment as `HYPERFUSION_API_KEY` and `Y7ROUTER_API_KEY`. Do not put
   them in `config.toml`; `api_key_env` exists so the value stays out of the file.
2. Declare both under `[provider_urls]` with the base URLs from section 1. `y7router` is named that
   way on purpose — its model ids carry `ark/`, `ds/`, `xiaomi/` prefixes, and a provider name that
   collides with a prefix causes the prefix to be stripped.
3. Set `[default_model]` to a **hyperfusion** model. It is the provider with real published limits;
   pointing the default at y7 means every unconfigured agent inherits a model whose limits are
   placeholders.
4. Verify before building anything on top:

```bash
curl -s -H "Authorization: Bearer $API_KEY" localhost:4200/api/providers \
  | python3 -c 'import sys,json; d=json.load(sys.stdin); print("total:", d["total"]);
[print(" ", p["id"], p["auth_status"], p["model_count"])
 for p in d["providers"] if p["auth_status"] != "missing"]'
```

The earlier version of this command iterated `json.load(sys.stdin)` directly. `/api/providers`
returns an object, not a list, so it iterated the object's keys and died with
`TypeError: string indices must be integers`. It was never run.

Read *your own* output, and read `total` carefully: on a build from before the narrowing in
section 1 it counts the whole catalogue, and on a build carrying it `total` is this response
while `catalog_total` is the catalogue. Neither number is restated here — the counts an earlier
version of this file quoted were one instance's, and quoting them invited people to check
against somebody else's machine instead of their own.

Expect the two routers as `configured`. **Do not** expect only two lines — the local providers
report `not_required` because they need no key, and an
earlier version of this section said "anything else `configured` means a key leaked", which
would have sent you hunting a leak that is not there. What to actually check: nothing
unexpected is `configured`. A stray `configured` — anthropic, openai, gemini — is the empty-key
trap in section 1.

5. Then send one real message per model you intend to use, with tools enabled, and read the body.
   Section 3 exists because status codes lie on this router.

---

*Router behaviour and the model tables were measured on 2026-08-16 and have not been re-measured
since. Model catalogues change without notice — run the checks in section 5 against your own
instance rather than trusting these tables a month from now.*

*Removed on 2026-08-23, deliberately and without replacement: the agent roster, the provider and
model counts of a specific instance, and its deployed `config.toml`. Those described one running
system; this file is published, and what is published should be about the routers, not about
somebody's deployment.*
