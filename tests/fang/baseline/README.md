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

`FANG-9`, `FANG-10`, `FANG-31`, `FANG-47` are red runs against a build that was
**never** patched — those defects have no fix commit on `ours`. There is no green
to record for them yet and their absence from `../after/` is not a gap in the evidence;
it is the evidence. `../run.sh` expects them to come back RED and reports a GREEN as a
failure, because a repro that stops reproducing an unfixed defect has stopped being a
repro. `../run.sh --list` prints which is which.

`FANG-13` is in the first group as of `fix/fang-13-empty-response`: `FANG-13.txt` here is
red against a build of `ours` with no fix, `../after/FANG-13.txt` is green against a build
of that branch, and both files name their image and carry the binary-marker lines that say
which one the container was actually running. The pair replaces an earlier `FANG-13.txt`
produced by a version of the script that no longer exists (see git history) — that one was
captured against `openfang:sprint3`, an image built before several of the merges that are
now on `ours`.

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
