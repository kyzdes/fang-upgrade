# OpenFang v0.6.9 — patched fork

Self-hosted agent runtime. This is [RightNow-AI/openfang](https://github.com/RightNow-AI/openfang)
at `v0.6.9` with fourteen defects fixed, running in production on our own box.

Upstream `main` has not moved since 2026-05-12 while 41 pull requests sit unmerged, so the fixes
live here.

## Start here

| If you want to… | Read |
|---|---|
| know what is different from upstream, and what is still broken | **[FORK-NOTES.md](FORK-NOTES.md)** |
| put this on a server, step by step, with checks | **[INSTALL-AGENT.md](INSTALL-AGENT.md)** |
| restyle the dashboard without breaking it | **[DESIGN-BRIEF.md](DESIGN-BRIEF.md)** |

`INSTALL-AGENT.md` is written as a task for an AI agent with root on a fresh machine, but it
reads fine as an ordinary runbook — every step ends in a command whose output tells you whether
it worked.

## Quick install

```bash
git clone https://github.com/kyzdes/openfang-patched.git
cd openfang-patched
docker compose up -d --build
curl -s http://127.0.0.1:4200/api/health
```

First build takes ~12 minutes on 4 cores. Then set `api_key` in the config — see
[INSTALL-AGENT.md, step 4](INSTALL-AGENT.md).

## Three things to get right before this is reachable from the internet

These are not hypothetical. Each one bit us on our own box.

**Docker publishing bypasses UFW.** Upstream's compose file publishes the dashboard on
`0.0.0.0`, and a `ufw deny 4200` will not stop it — port publishing writes a DNAT rule that
skips the INPUT chain. Bind to loopback or a private interface. The
`docker-compose.override.yml` shipped here already does.

**Empty provider keys read as configured.** Upstream declares `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY` and friends as empty-but-present. OpenFang treats *present* as *configured*:
on an empty string it boots claiming an anthropic provider and enables the OpenAI embedding
driver, so text leaves the machine. The override replaces that block rather than extending it.

**Without `api_key` the API is open.** Anything that reaches the port can drive the agents, and
agents can run shell commands. Note that `GET /api/agents` answers `200` without a key *by
design* — the dashboard needs it. Test authentication on a write (`POST`), not on that read.

## Migration and rollback

The fork adds four nullable columns to `usage_events` (schema v8 → v9), two indexes and one
`UPDATE`. No `DROP`, nothing rewritten. Rollback was tested end to end: the old binary starts on
a v9 database, reads every row, writes new ones, and re-upgrading is idempotent.

Back up the volume before upgrading anyway — it is tens of megabytes:

```bash
docker run --rm -v openfang_openfang-data:/d -v "$PWD":/b alpine \
  tar czf /b/openfang-backup.tar.gz -C /d .
```

## Provenance

Every fix has a reproduction that fails on stock `v0.6.9` and passes here. They live in the
working fork, [kyzdes/openfang](https://github.com/kyzdes/openfang), under `tests/fang/` —
kept out of this repository because they contain output from our own instance.

Licence and copyright are upstream's: Apache-2.0 OR MIT. This fork claims no additional rights
and offers no warranty; read [FORK-NOTES.md](FORK-NOTES.md#known-issues-not-fixed-here) for the
defects that are still open before relying on it for anything that matters.
