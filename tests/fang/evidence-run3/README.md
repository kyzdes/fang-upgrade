# Evidence for run 3 of the review of `tests/fang/run.sh`

Red before green for every item the review raised, measured on this box on
2026-08-14. Every file here is captured output, not a retelling.

## The stands

Nothing here ran against the shared staging box. Three throwaway stands were
created for the measurement and destroyed afterwards:

| name     | port | volume         | role |
|----------|------|----------------|------|
| `of-r3a` | 4211 | `of-r3a-data`  | stand A — the one `--base-url` names |
| `of-r3b` | 4212 | `of-r3b-data`  | stand B — the innocent bystander `--container` names |
| `of-r3s` | 4213 | anonymous      | a stub that answers `401` to a known-bad key and `500` to every other, to exercise the preflight |

`of-r3a` and `of-r3b` are `openfang:sprint3`, the same image staging runs, each
on its own fresh volume with its own `api_key`.

## Findings, red then green

| # | finding | red | green |
|---|---------|-----|-------|
| 1 | the clean-up `rm -rf`s a path derived from `--container` using a name read from `--base-url`, so a run against one stand deletes files on another | `red-1-cross-stand-rm.txt` | `green-1-cross-stand-refused.txt` |
| 1b | the same wrong path is exported as `OPENFANG_VOLUME`, so `A-1.sh` (and `A-7.sh`) `rm -rf` the other stand too | `red-1b-exported-path-rm.txt` | `green-1-cross-stand-refused.txt` |
| 1c | the correct pairing must still run, sweep, and say what it left | — | `green-1b-correct-pairing.txt` |
| 2 | A-7 had no witness: a probe that did nothing came back REVIEW and the run exited 0, while the header claimed two barriers in front of every grade | `red-2-a7-no-witness.txt` | `green-2-a7-witness.txt` |
| 3 | an honest `SKIPPED` (exit 3, "preconditions not met") was rewritten as INCONCLUSIVE, losing the reason and turning exit 4 into exit 1 | `red-3-skipped-to-inconclusive.txt` | `green-3-skipped-stays-skipped.txt` |
| 4 | the preflight proved one key while `harness/lib.sh` handed FANG-9/10/13/47 another (401 vs 200 on the same route) | `red-4-preflight-wrong-key.txt` | `green-4-preflight-right-key.txt` |
| 5 | the preflight called the credential ACCEPTED on any answer but 401/403/000 — 500 included | `red-5-preflight-accepts-500.txt` | `green-5-preflight-500-unverified.txt` |
| 6 | `--only ''` silently ran all twelve instead of being the usage error the block was written to give | `red-6-only-empty.txt` | `green-6-only-empty-refused.txt` |
| 7 | a two-pattern witness with both patterns missing was reported as if only the last were missing | `red-7-witness-names-only-last.txt` | `green-7-witness-names-all.txt` |
| 8 | the claim that the costly repros' witnesses came from recorded runs was false for FANG-45 — and for FANG-31, which the review had not caught | `red-8-fang45-witness.txt` | `green-8-witness-provenance.txt` |
| 9 | control: a garbage key must still give no PASS | — | `green-9-garbage-key-still-no-pass.txt` |
| 10 | control: the whole cheap suite still passes against one proven stand | — | `green-10-full-cheap-suite.txt` |
| 11 | a run whose only rows are REVIEW no longer closes with "every repro that ran reached its expected verdict" | quoted in the file | `green-11-nothing-graded.txt` |
| 12 | `A-1.sh` and `A-7.sh` defaulted their own delete path to the staging volume whatever `BASE_URL` said — the same defect one level down, and live whenever they are run by hand | `red-1b-exported-path-rm.txt` | `green-12-probes-do-not-default-the-path.txt` |

`live-FANG-31.log` is the FANG-31 log from the run in `green-10`; it is the
first captured run in which that repro's witness pattern actually appears, and
the provenance check in `run.sh` reads it. FANG-45's witness is still
unconfirmed by any recorded run — the repro is costly and was not run here —
and the report says so by name, every run, instead of a claim in a comment.

## How the two-stand scenario was measured

    # a file that belongs to stand B and to nobody else
    echo ... > /var/lib/docker/volumes/of-r3b-data/_data/agents/test-a1-probe/precious.txt
    # an agent of that name on stand A, which is where the run works
    curl -X POST .../4211/api/agents  (manifest naming test-a1-probe)
    # the run: base-url says A, container says B
    OPENFANG_API_KEY=<A key> ./run.sh --only A-2 --base-url http://127.0.0.1:4211 --container of-r3b

Before the fix that deleted `precious.txt` on stand B and reported "removed
agent test-a1-probe" and "the stand carries no probe agent now" — both about a
machine the run never sent a request to. After the fix the run refuses with
exit 2 and `precious.txt` is byte-identical afterwards (`md5sum` in both files).
