# Providers, models and fallbacks as actually deployed

Read this before pointing a second OpenFang instance at the same routers. Everything below was
read off a running instance on 2026-08-16, not copied from a vendor page — where the two
disagree, the running instance won.

**No credentials here.** Keys live in environment variables named below; the values are not in
this file, this repository, or any log it tells you to read.

---

## 1. The two providers that are actually configured

OpenFang ships a catalogue of 44 providers. Two of them are ours; the other 42 sit there with
`auth_status: missing` and no key, which is harmless but makes `/api/providers` misleading at a
glance.

| Provider id | Base URL | Key env var | Models live |
|---|---|---|---|
| `hyperfusion` | `https://api.hyperfusion.io/v1` | `HYPERFUSION_API_KEY` | 9 |
| `y7router` | `https://router.y7.hk/v1` | `Y7ROUTER_API_KEY` | 15 |

Both are declared under `[provider_urls]`. The whole of the deployed `config.toml` is 23 lines:

```toml
api_key = "<the instance's own API key — not a provider key>"

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
reproduce this setup by copying the config file.

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

These carry **real limits published by the router**, which is why they are the ones we default to.

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

**`kimi/k3` is the one model here with measured behaviour:** needle-in-haystack retrieved at start,
middle and end up to ~300 000 words; `prompt_tokens` clamps at exactly 1 000 000; past ~10 MB of
payload it returns an empty body in ~5 seconds with no error.

### The general rule these add up to

A router that publishes no limits and honours no parameters will fail **quietly**. Every failure
mode above returns HTTP 200. Before trusting a new model on y7, send it one real request with tools
and one with streaming, and look at what actually came back — not at the status code.

---

## 4. Fallbacks as deployed

13 agents. The pattern in use is deliberately simple: **primary on y7router, fallback to
hyperfusion.** The reasoning is in section 3 — y7 is fast and free-form but unpredictable, so the
safety net is the router that publishes real limits.

| Agents | Primary | Fallback chain |
|---|---|---|
| `AgentGLM52`, `AgentGLM52B`, `AgentGLM52C`, `AgentRAG`…`AgentRAG4` (7 agents) | `y7router` / `ark/glm-5.2` | → `hyperfusion` / `openai/gpt-oss-120b` |
| `AgentKimi3` | `y7router` / `kimi/k3` | → `hyperfusion` / `openai/gpt-oss-120b` → `y7router` / `ark/deepseek-v4-pro` |
| `AgentDeepSeek4` | `y7router` / `ark/deepseek-v4-pro` | none |
| `AgentGemma4`, `assistant`, `youtube-insights` | `hyperfusion` / `google/gemma-4-31b-it` | none |
| `AgentGptOss` | `hyperfusion` / `openai/gpt-oss-120b` | none |

A fallback entry names **both** provider and model. In a manifest:

```toml
model = { provider = "y7router", model = "ark/glm-5.2" }

[[fallback_models]]
provider = "hyperfusion"
model = "openai/gpt-oss-120b"
```

### Four things about fallback that are not in the docs

**It fails over on 429 too, not only on 5xx.** The documentation says rate limits are not a
failover trigger; the code disagrees (`fallback.rs:46-66` — every error triggers it). If your
fallback provider is metered, a rate-limit storm on the primary will spend money there. Budget for it.

**A fallback entry used to inherit `[default_model].base_url`** — meaning a fallback naming a
different provider still went to the default provider's endpoint. **This fork fixes that**; stock
v0.6.9 does not. On stock, the workaround is to declare the fallback provider explicitly in
`[provider_urls]` and never rely on inheritance.

**An unknown model gets an assumed 200 000-token context window** (`agent_loop.rs:225,502`),
regardless of what it can actually take. Combined with y7's placeholder windows, this means context
budgeting on that router is guesswork unless you measured the model yourself.

**Which model actually served the turn is visible in this fork.** The response carries
`model_used`, a per-call `calls[]` array and a `fallback` object with six fields. On stock v0.6.9 a
fallback-served turn looks identical to a normal one — you cannot tell from the response that a
substitution happened, and the only trace is in the daemon log.

---

## 5. Setting up a second instance against the same routers

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
  | python3 -c 'import sys,json; [print(p["id"], p["auth_status"], p["model_count"]) \
      for p in json.load(sys.stdin) if p["auth_status"]!="missing"]'
```

Expect exactly the two, both `configured`. Anything else `configured` means a key leaked in from
the environment — most likely the empty-key trap in section 1.

5. Then send one real message per model you intend to use, with tools enabled, and read the body.
   Section 3 exists because status codes lie on this router.

---

*Read off a live instance, 2026-08-16. Model catalogues change without notice — re-run the checks
in section 5 rather than trusting this table a month from now.*
