# fang-upgrade — a patched OpenFang v0.6.9

This is a fork of [RightNow-AI/openfang](https://github.com/RightNow-AI/openfang) at
**v0.6.9** (`version = "0.6.9"` in the workspace `Cargo.toml`), with defects fixed
and kept fixed by CI.

It is not a rewrite and not a new product. Same crates, same binary name, same
config file. The difference is that a list of things v0.6.9 got wrong are no
longer wrong here.

## Why this exists

Upstream's `main` has not moved since **2026-05-12** — its newest commit is
`acf2587 bump v0.6.9`. This fork's base commit `f6e8539 bump v0.6.9` has the
identical tree (`git diff acf2587 f6e8539` is empty); everything after it here is
the fork's own work. **43 pull requests** were open against upstream when this was
last checked — 2026-08-23,
`gh api 'search/issues?q=repo:RightNow-AI/openfang+is:pr+is:open' --jq .total_count`.

A defect found downstream therefore has nowhere upstream to go. It gets fixed here.

## What is fixed

The authoritative list is not a paragraph — it is a registry that prints itself:

```bash
bash tests/fang/run.sh --list
```

Each row names the defect, its reproduction script, and whether it is fixed at all.
`FANG-9` is listed as `NO FIX`: the agent can still report a write it never
performed, and the phantom-action guard covers channels only. That row is in the
list on purpose. **[`FORK-NOTES.md`](FORK-NOTES.md)** explains each fix with its
before/after measurement.

Measured, not estimated: **77 commits** since the v0.6.9 base (`f6e8539`), of which
**34** are `fix(...)`/`feat(...)` (`git log --oneline --no-merges f6e8539..main`).
No headline "N defects fixed" number appears in this README, for the reason
`FORK-NOTES.md` gives: an earlier copy of these notes carried one, and it went
stale while the work continued.

The themes, each traceable to commits in `git log`:

- **Usage is metered per LLM call, not per agent turn**, and the response says
  which model actually answered — a fallback-served turn no longer bills the model
  that answered nothing (`ad1fdbf`; reproduction `A-6`).
- **A fallback model resolves its own `base_url`** instead of inheriting the
  primary's, so provider B's key stops being sent to provider A (`cf1a8cf`).
- **A turn that exhausts `max_iterations` hands back its work** instead of an
  HTTP 500 carrying none of it (`c2a7fe9`, `927793b`, `9281436`).
- **An empty-but-valid provider response fails the turn** instead of succeeding
  with a sentence the runtime wrote itself (`f05a1f9`, `16902f3`).
- **`file_read` discloses truncation and supports `offset`/`limit` paging**
  (`07924e9`).
- **Secrets stop leaking**: the Telegram bot token out of network-error logs
  (`c25cf2a`), out of the LLM prompt and the on-disk session (reproduction
  `FANG-43`), and out of `reqwest` error bodies in six more adapters (`45dd243`).
- **Routes that cannot work say so**: `PUT /api/agents/{id}/update` returns 501 and
  names the routes that do work, instead of a 200-shaped no-op (`5aa1992`,
  `f23c058`).
- **Shutdown is bounded** and eleven other spin loops were fixed with it
  (`24714d3`); channel adapters are actually stopped and Telegram is drained
  before restart (`418c298`).
- **Passkey login for the dashboard**, handed out by one-time link (`5cd4234`),
  with the machine API key scoped back to being a fallback entrance rather than a
  second door to the internet (`deba852`).

## Running it

The image is `ghcr.io/kyzdes/fang-upgrade`. It is built by CI only after
`fmt + clippy + test` passes, and published only from `main`.

```bash
docker run -d --name openfang \
  -e OPENFANG_LISTEN=0.0.0.0:4200 \
  -p 127.0.0.1:4200:4200 \
  -v openfang-data:/data \
  ghcr.io/kyzdes/fang-upgrade:main
```

`OPENFANG_LISTEN` is not optional here, and this is v0.6.9 behaviour rather than
something the fork introduced: without it the daemon binds `127.0.0.1:50051`
*inside* the container, where a published port cannot reach it. Measured on
`:1009ed23…` — the log line reads `OpenFang API server listening on
http://127.0.0.1:50051` and `curl` against the published port answers nothing.
With the variable set, `GET /api/health` returns `{"status":"ok","version":"0.6.9"}`.

The image exposes `4200`, stores everything under `/data` (`OPENFANG_HOME=/data`,
declared `VOLUME`), and its entrypoint is `openfang start`.

A fork build says which build it is — `GET /api/version` on the same container:

```json
{"name":"openfang","version":"0.6.9","git_sha":"1009ed230dcbbc86afd81d0dd17c5cd83e1b7231",
 "git_describe":"fang-v1-19-g1009ed2","build_date":"2026-08-23T08:14:31Z",
 "rust_version":"rustc 1.91.1 (ed61e7d7e 2025-11-07)","platform":"linux","arch":"x86_64"}
```

Two tags exist, and only two:

| Tag | Moves? | Use it for |
|---|---|---|
| `:main` | yes | looking at the current tip |
| `:<full-sha>` | no | deploying |

**There is no `latest`, deliberately.** A moving tag is a way to ship something
other than what you tested; a server that pins a sha ships what it tested. The
reasoning is in `.github/workflows/fork-ci.yml` next to the `tags:` block.
Verified: `docker run --rm --entrypoint openfang
ghcr.io/kyzdes/fang-upgrade:1009ed230dcbbc86afd81d0dd17c5cd83e1b7231 --version`
prints `openfang 0.6.9`.

## Passkey login

The dashboard can be closed behind a WebAuthn passkey — Face ID, Touch ID,
Windows Hello or a device PIN — with access handed out as a one-time link whose
token lives in the URL fragment and is stored only as a SHA-256 hash.

**It is off by default.** `AuthConfig::default()` in
`crates/openfang-types/src/config.rs` sets `enabled: false`, and with it off the
server behaves exactly as v0.6.9 did. Turning it on requires `rp_id`, `rp_origin`
and `rp_name`; an incomplete or non-HTTPS configuration is refused at startup
rather than half-applied.

The name shown above the heading on `/login` comes from `auth.rp_name`, not from
the source — this is a general-purpose fork, and no installation's brand is
compiled into it.

- Operating procedure: [`docs/passkey-runbook.md`](docs/passkey-runbook.md)
- Configuration reference: [`docs/configuration.md`](docs/configuration.md)

## Building it yourself

The whole toolchain version lives in one place and CI refuses to start if the four
copies of it disagree (`rust-toolchain.toml`, `Cargo.toml`'s `rust-version`, both
`FROM` lines in `Dockerfile`, and the workflow). It is currently **1.91**.

The supported path is the multi-stage `Dockerfile` (`rust:1.91-slim-bookworm` for
both stages). Its builder stage runs `cargo build --release --bin openfang`:

```bash
docker build -t fang-upgrade \
  --build-arg GIT_SHA=$(git rev-parse HEAD) \
  --build-arg GIT_DESCRIBE=$(git describe --tags --always --dirty) \
  --build-arg BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ) .
```

Those three build args are what `/api/version` reports. Without them the build
still succeeds and honestly answers `"git_sha":"unknown"`.

Building on a host instead needs the system libraries the automated builds
install, and they are listed in exactly two places rather than described here:
`pkg-config libssl-dev perl make` in the `Dockerfile`'s builder stage, and the
Tauri libraries in the `Install Tauri system deps` step of `fork-ci.yml` — the
desktop crate is a workspace member, so `--workspace` pulls it in even when you
only want the daemon.

## How the checks run

`.github/workflows/fork-ci.yml` runs on pushes to `main` (and `ours`) and on pull
requests into them. Two gates, both required:

1. **`fmt + clippy + test`**

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace -- --test-threads=2
   ```

2. **The image build.** The Dockerfile is built on every pull request and thrown
   away; only a push to `main` publishes it. This gate exists because a broken
   Dockerfile used to surface *after* a merge — that is how a `rust 1.88` builder
   stage, below the declared MSRV, once reached `main`.

Upstream's own `ci.yml` and `release.yml` are kept in the tree but triggered only
on a branch named `upstream-sync-only`, which does not exist on this remote — so
they do not run.

The registry rows carry `fix <sha>` references inherited from an earlier copy of
this work; several of those shas (`cbb0660`, `acc85d7`, `b86e65b`, …) do not
resolve in this repository. Trust the row's description and its script, not its
sha.

Shell reproduction scripts under `tests/fang/` need a Docker daemon and named
containers; they are run by hand, not by CI, and `run.sh --list` says which ones
cost a real provider call.

## Layout notes

- [`FORK-NOTES.md`](FORK-NOTES.md) — what was wrong, what changed, measured
  before/after.
- [`docs/upstream-README.md`](docs/upstream-README.md) — upstream's README, kept
  verbatim as heritage. It describes upstream's install script, website and
  release channels, none of which this fork controls; none of its claims have been
  re-verified here.
- [`CLAUDE.md`](CLAUDE.md) and [`docs/subagent-task-template.md`](docs/subagent-task-template.md)
  — how work on this fork is actually run and accepted.

## License

Unchanged from upstream: Apache-2.0 OR MIT (`license` in `[workspace.package]`).
