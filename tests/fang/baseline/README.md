# Baseline evidence

Raw output from reproducing each defect against the unpatched build, kept so that
"the patch fixed it" is a comparison and not an assertion.

One file is deliberately absent: a copy of a live `config.toml` was committed here by
mistake and removed in a later commit. It contained a real dashboard `api_key`, which
was rotated. Do not commit config snapshots — redact them, or capture only the section
under test.

## Two kinds of baseline live here

`A-1`, `A-2`, `A-4`, `A-6`, `A-7`, `FANG-43`, `FANG-45` are baselines in the original
sense: red runs against a build that was later patched, and `../after/` (plus
`../after-v2/` for the second adversarial round) holds the matching green.

`FANG-10`, `FANG-13`, `FANG-31`, `FANG-47` are red runs against a build that
was **never** patched — those defects have no fix commit on `ours`. There is no green
to record for them yet and their absence from `../after/` is not a gap in the evidence;
it is the evidence. `../run.sh` expects them to come back RED and reports a GREEN as a
failure, because a repro that stops reproducing an unfixed defect has stopped being a
repro. `../run.sh --list` prints which is which.

`FANG-9` moved from the second group to the first when `fix/fang-9-phantom-write`
landed, and it carries three files instead of two, because the script itself was
rewritten (three phases, three scenarios, one step cursor each) and a recording of the
old script proves nothing about the new one:

* `FANG-9.txt` — the original sprint-3 recording, the OLD two-turn script against
  `openfang:sprint3`. Kept as history; do not compare it line-for-line with the others.
* `FANG-9-prepatch-ours.txt` — the new script against a build of `ours` **without** the
  fix (`of-fang9:pre`). This is the red half of the transition.
* `../after/FANG-9.txt` — the same script against the same tree **with** the fix
  (`of-fang9:post`). Green.
* `../after/FANG-9-tautology-broken-verbs.txt` — the same script against a build whose
  patch was broken on purpose in one place (the word "wrote" removed from the write
  vocabulary). Red again, which is what says the green above is a measurement and not a
  script that cannot fail.

The second review round added a fourth phase and two more recordings. The script is now
four phases; phase D is **printed, not graded**, and the two files below are what the
first round was missing rather than what it got wrong:

* `FANG-9-v2-prepatch.txt` — the FOUR-phase script against `openfang:sprint3`, the
  unpatched build. Red, and phase D shows the guard's boundary on a build that has no
  fix at all.
* `../after/FANG-9-v2.txt` — the four-phase script against the patched tree. Green, with
  the same phase D result: that phrasing is outside the guard on **both** builds, which
  is the point of printing it instead of grading it.
* `../after/FANG-9-v2-run-sh-three-builds.txt` — the whole cheap suite on three stands:
  the deployed `openfang:sprint3`, `ours` @ 7606210 built with the patched image's own
  build args, and that tree with the patch. The middle column is the one that matters —
  without it FANG-31 looks like a regression this patch caused, and it is not.
* `../after/FANG-9-v2-stream-order.txt` — SSE and WS driven by hand against the patched
  stand on the `phantom-write-claim-repeated` scenario, the only path that emits the
  `[Unverified]` note. The SSE transcript is the acceptance for "the retraction arrives
  before `done`"; the WS transcript is there because WS drops `ContentComplete`
  entirely (`ws.rs`: `_ => None`) and so never had the defect — a fact worth measuring
  rather than asserting.

One trap worth repeating, because it cost a run here: the Dockerfile builds with
`--mount=type=cache,target=/build/target`, and cargo decides what is stale by mtime. A
worktree checked out *after* the artifacts in that cache — which is what
`git worktree add` produces — is silently treated as up to date, and the image gets the
previous build's binary while every log line says success. Before believing any run,
check the binary actually carries the patch:

```
docker exec <stand> grep -c 'no tool that writes to disk ran' /usr/local/bin/openfang
```

## Re-capturing one

Each file starts with a header naming the branch, the short SHA, the staging image and
the exact command, because a run's output is worth nothing without the build it ran
against. Reproduce that shape:

```
{ echo "# Baseline for FANG-N — ..."; echo "# captured : $(date -u '+%Y-%m-%dT%H:%M:%SZ')"; ... } > baseline/FANG-N.txt
timeout 600 ./FANG-N.sh >> baseline/FANG-N.txt 2>&1 </dev/null
echo "# exit code: $?" >> baseline/FANG-N.txt
```

`</dev/null` is load-bearing, not tidiness: these scripts reach into the staging
container with `docker exec -i`, and a bodyless request inside can block on `stdin.read()`
until EOF. Given a terminal, that is a silent hang with no output.
