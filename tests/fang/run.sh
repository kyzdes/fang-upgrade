#!/usr/bin/env bash
# tests/fang/run.sh — run every FANG reproduction in this directory and say,
# in one place and without rounding, what each one actually proved.
#
# WHY THIS IS NOT JUST `for f in *.sh; do "$f"; done`
#
#   The scripts in here do not agree on how to report themselves, and two of
#   them do not report themselves at all. Pretending otherwise is how a
#   directory of eight probes turns into "all green":
#
#     * A-2.sh answers in its EXIT CODE, and inverted at that: 0 means the
#       defect reproduced, 1 means it did not.
#     * FANG-31.sh / FANG-43.sh answer in a "RESULT: RED|GREEN|INCONCLUSIVE"
#       line on stdout; their exit code is whatever the last command in the
#       script happened to return and means nothing.
#     * A-1.sh, A-4.sh, A-6.sh, A-7.sh and FANG-45.sh print measurements and
#       a written-out red criterion, and leave the verdict to a reader. There
#       is no honest way to machine-grade them, so this runner does not
#       pretend to: they come back as REVIEW, which is not a pass.
#     * The four sprint-4 scenario repros (FANG-9/10/13/47) were written to a
#       fixed convention — see MODE exit4 below — including a distinct code
#       for "my own control failed, so I proved nothing".
#
#   And five of them spend real money: A-4, A-6, A-7, FANG-43 and FANG-45 all
#   drive real agents against real providers on the staging box. They are
#   gated behind --costly and reported as SKIPPED, with the reason, when the
#   gate is closed. A skip is never a pass here.
#
# VERDICT MODES
#   exit4    0=RED(reproduced) 1=GREEN(not reproduced) 2=REFUSED 3=SKIPPED
#            4=INCONCLUSIVE(the script's own control failed)
#   a2       A-2.sh's own convention: 0=RED 1=GREEN, anything else = ERROR
#   result   last "RESULT: RED|GREEN|INCONCLUSIVE" line on stdout wins;
#            the exit code is recorded but not believed
#   manual   no machine-readable verdict exists -> REVIEW
#
# OUTCOMES
#   PASS          the verdict matched what this build is expected to produce
#   FAIL          it did not (a fixed defect came back, or a repro rotted)
#   INCONCLUSIVE  the repro ran but disqualified itself -> counted as failure
#   REVIEW        ran, produced evidence, needs a human -> not a pass
#   SKIPPED       did not run, with a reason -> never a pass
#   ERROR         crashed, timed out, or returned a code its mode cannot read
#
# EXIT CODE
#   0  every repro that ran reached its expected verdict, and at least one ran
#   1  at least one FAIL or INCONCLUSIVE
#   2  at least one ERROR (harness problem, not a product verdict)
#   3  refused: pointed at production
#   4  nothing ran at all — everything was skipped. Deliberately not 0: a run
#      that executed no repro has proven nothing, and "no failures" out of an
#      empty set is the exact shape of a report that reads as all-clear.
#
# Usage:
#   ./run.sh [--costly] [--only id[,id...]] [--list] [--timeout SECS]
#            [--out DIR] [--base-url URL]

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_URL="${OPENFANG_URL:-http://127.0.0.1:4201}"
CONTAINER="${OF_CONTAINER:-openfang-staging}"
TIMEOUT_SECS=900
RUN_COSTLY=0
ONLY=""
OUT_DIR=""
LIST_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --costly)   RUN_COSTLY=1; shift ;;
    --only)     ONLY="$2"; shift 2 ;;
    --list)     LIST_ONLY=1; shift ;;
    --timeout)  TIMEOUT_SECS="$2"; shift 2 ;;
    --out)      OUT_DIR="$2"; shift 2 ;;
    --base-url) BASE_URL="$2"; shift 2 ;;
    -h|--help)  sed -n '2,60p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------- registry --
# id | script | mode | expect | cost | covers
#
# `expect` is what THIS branch should produce, and it is a claim about the
# code, not a wish: RED for defects with no fix commit on `ours`, GREEN for
# defects a fix commit claims to have closed. If a GREEN entry comes back RED
# the fix regressed; if a RED entry comes back GREEN either someone fixed it
# or the repro stopped reproducing — both are FAIL, both need a human.
REPROS=(
  "A-1|A-1.sh|manual|-|cheap|PUT /api/agents/{id}/update was a 200-returning no-op (fixes b86e65b, 6cb20be)"
  "A-2|A-2.sh|a2|GREEN|cheap|remove_custom_model did not recompute provider.model_count (fix 5891b2b)"
  "A-4|A-4.sh|manual|-|costly|'## Current Date' and other service prompt sections leaked into agent output (fix cdd70de)"
  "A-6|A-6.sh|manual|-|costly|a fallback-served turn never disclosed which model actually answered (fix cbb0660)"
  "A-7|A-7.sh|manual|-|costly|manifest [[fallback_models]] inherited [default_model].base_url (fix 366d62f)"
  "FANG-31|FANG-31.sh|result|RED|cheap|Telegram 409: channel reload abandons an in-flight getUpdates (NO FIX ON ours)"
  "FANG-43|FANG-43.sh|result|GREEN|costly|the Telegram bot token reached the LLM prompt and the on-disk session (fix acc85d7)"
  "FANG-45|FANG-45.sh|manual|-|costly|file_read truncated silently with no way to page the rest (fix 8b502f9)"
  "FANG-9|FANG-9.sh|exit4|RED|cheap|agent reports a write it never performed; the phantom-action guard covers channels only (NO FIX)"
  "FANG-10|FANG-10.sh|exit4|RED|cheap|max_iterations exceeded -> HTTP 500 carrying none of the turn's work (NO FIX)"
  "FANG-13|FANG-13.sh|exit4|RED|cheap|an empty-but-valid provider response is returned as a successful turn (NO FIX)"
  "FANG-47|FANG-47.sh|exit4|RED|cheap|the max-iterations exit discards the whole turn's accounting (NO FIX)"
)

field() { printf '%s' "$1" | cut -d'|' -f"$2"; }

if [ "$LIST_ONLY" = 1 ]; then
  printf '%-8s %-13s %-7s %-7s %-7s %s\n' ID SCRIPT MODE EXPECT COST COVERS
  for r in "${REPROS[@]}"; do
    printf '%-8s %-13s %-7s %-7s %-7s %s\n' \
      "$(field "$r" 1)" "$(field "$r" 2)" "$(field "$r" 3)" \
      "$(field "$r" 4)" "$(field "$r" 5)" "$(field "$r" 6)"
  done
  exit 0
fi

# -------------------------------------------------------------- prod guard --
case "$BASE_URL" in
  *:4200*) echo "REFUSING: BASE_URL=$BASE_URL looks like production. Staging is :4201." >&2; exit 3 ;;
esac
if [ "$CONTAINER" = "openfang-openfang-1" ]; then
  echo "REFUSING: OF_CONTAINER=$CONTAINER is the production container." >&2
  exit 3
fi

# --------------------------------------------------------------- staging? --
STAGING_UP=0
STAGING_WHY="ok"
if ! command -v docker >/dev/null 2>&1; then
  STAGING_WHY="docker not on PATH"
elif ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
  STAGING_WHY="container '$CONTAINER' does not exist"
elif [ "$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null)" != "true" ]; then
  STAGING_WHY="container '$CONTAINER' is not running"
elif ! python3 - "$BASE_URL" <<'PY'
import sys, urllib.request
try:
    urllib.request.urlopen(sys.argv[1] + "/api/health", timeout=5)
except Exception:
    sys.exit(1)
PY
then
  STAGING_WHY="API at $BASE_URL did not answer /api/health"
else
  STAGING_UP=1
fi

TS="$(date -u '+%Y%m%d-%H%M%S')"
[ -n "$OUT_DIR" ] || OUT_DIR="/tmp/fang-run/$TS"
mkdir -p "$OUT_DIR" || { echo "cannot create $OUT_DIR" >&2; exit 2; }

echo "======================================================================"
echo "tests/fang/run.sh — $TS"
echo "target      : $BASE_URL   container: $CONTAINER"
if [ "$STAGING_UP" = 1 ]; then
  echo "staging     : up (image $(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null))"
else
  echo "staging     : DOWN — $STAGING_WHY"
  echo "              every repro below needs it; they will be SKIPPED, not passed."
fi
echo "costly gate : $([ "$RUN_COSTLY" = 1 ] && echo "OPEN (--costly): repros that bill real providers WILL run" \
                                            || echo "closed: repros that bill real providers are SKIPPED (pass --costly)")"
echo "logs        : $OUT_DIR"
echo "======================================================================"
echo

N_PASS=0; N_FAIL=0; N_SKIP=0; N_REVIEW=0; N_ERROR=0
SUMMARY=()

for r in "${REPROS[@]}"; do
  id="$(field "$r" 1)"; script="$(field "$r" 2)"; mode="$(field "$r" 3)"
  expect="$(field "$r" 4)"; cost="$(field "$r" 5)"; covers="$(field "$r" 6)"

  if [ -n "$ONLY" ] && ! printf ',%s,' "$ONLY" | grep -q ",$id,"; then
    continue
  fi

  log="$OUT_DIR/$id.log"
  path="$HERE/$script"

  # ---- gates, each with its own reason -----------------------------------
  reason=""
  if [ ! -x "$path" ]; then
    reason="no executable $script in $HERE"
  elif [ "$STAGING_UP" != 1 ]; then
    reason="staging is down: $STAGING_WHY"
  elif [ "$cost" = costly ] && [ "$RUN_COSTLY" != 1 ]; then
    reason="makes real, billed LLM calls against live providers; pass --costly to run it"
  fi
  if [ -n "$reason" ]; then
    printf '%-8s SKIPPED   %s\n' "$id" "$reason"
    SUMMARY+=("SKIPPED|$id|$reason|$covers")
    N_SKIP=$((N_SKIP + 1))
    continue
  fi

  # ---- run it ------------------------------------------------------------
  # Protocol note: NOT a pipeline. `cmd | tee` would hand us tee's exit code,
  # and every verdict in `exit4`/`a2` mode lives in cmd's. Redirect to a file,
  # capture $?, and grep the file afterwards as a separate statement.
  #
  # </dev/null is not decoration either: these scripts reach into the staging
  # container with `docker exec -i`, which forwards whatever stdin it was
  # given. Handed a terminal or an open pipe, a bodyless request inside can
  # sit on stdin.read() forever — a silent hang with no output at all. Give
  # every repro a stdin that is already at EOF.
  printf '%-8s running   (%s, mode=%s, expect=%s) ... ' "$id" "$script" "$mode" "$expect"
  start=$(date +%s)
  timeout --signal=TERM --kill-after=30 "$TIMEOUT_SECS" "$path" "$BASE_URL" >"$log" 2>&1 </dev/null
  rc=$?
  took=$(( $(date +%s) - start ))

  # ---- read the verdict --------------------------------------------------
  verdict=""; note=""
  if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
    verdict=ERROR; note="timed out after ${TIMEOUT_SECS}s"
  else
    case "$mode" in
      exit4)
        case "$rc" in
          0) verdict=RED ;;
          1) verdict=GREEN ;;
          2) verdict=REFUSED; note="script refused the target" ;;
          3) verdict=SKIPPED; note="$(grep -m1 '^SKIPPED' "$log" 2>/dev/null || echo 'script reported missing prerequisites')" ;;
          4) verdict=INCONCLUSIVE; note="the script's own control failed" ;;
          *) verdict=ERROR; note="exit $rc is not part of the exit4 convention" ;;
        esac
        ;;
      a2)
        case "$rc" in
          0) verdict=RED ;;
          1) verdict=GREEN ;;
          *) verdict=ERROR; note="exit $rc; A-2.sh only ever means 0=RED or 1=GREEN" ;;
        esac
        ;;
      result)
        line="$(grep -E '^RESULT: (RED|GREEN|INCONCLUSIVE)' "$log" 2>/dev/null | tail -1)"
        if [ -z "$line" ]; then
          verdict=ERROR; note="no 'RESULT:' line in the output (exit was $rc)"
        else
          verdict="$(printf '%s' "$line" | awk '{print $2}')"
          note="exit code was $rc and is not used in this mode"
        fi
        ;;
      manual)
        verdict=REVIEW
        note="no machine-readable verdict; read $log against the red criterion in the script header (exit $rc)"
        ;;
      *)
        verdict=ERROR; note="unknown mode '$mode' in the registry"
        ;;
    esac
  fi

  # ---- grade it ----------------------------------------------------------
  case "$verdict" in
    REVIEW)       outcome=REVIEW;  N_REVIEW=$((N_REVIEW + 1)) ;;
    ERROR)        outcome=ERROR;   N_ERROR=$((N_ERROR + 1)) ;;
    REFUSED)      outcome=ERROR;   N_ERROR=$((N_ERROR + 1)) ;;
    SKIPPED)      outcome=SKIPPED; N_SKIP=$((N_SKIP + 1)) ;;
    INCONCLUSIVE) outcome=INCONCLUSIVE; N_FAIL=$((N_FAIL + 1)) ;;
    "$expect")    outcome=PASS;    N_PASS=$((N_PASS + 1)) ;;
    *)            outcome=FAIL;    N_FAIL=$((N_FAIL + 1))
                  note="got $verdict, expected $expect${note:+ ($note)}" ;;
  esac

  printf '%s (%ss)\n' "$outcome" "$took"
  [ -n "$note" ] && printf '         %s\n' "$note"
  SUMMARY+=("$outcome|$id|${verdict}${note:+ — $note}|$covers")
done

# ------------------------------------------------------------------ report --
echo
echo "======================================================================"
echo "RESULTS"
echo "======================================================================"
printf '%-14s %-8s %s\n' OUTCOME ID DETAIL
for s in "${SUMMARY[@]}"; do
  printf '%-14s %-8s %s\n' "$(field "$s" 1)" "$(field "$s" 2)" "$(field "$s" 3)"
done
echo
echo "PASS=$N_PASS  FAIL=$N_FAIL  REVIEW=$N_REVIEW  SKIPPED=$N_SKIP  ERROR=$N_ERROR"
if [ "$N_REVIEW" -gt 0 ]; then
  echo
  echo "REVIEW is not PASS. Those scripts measure; the verdict is a human's."
fi
if [ "$N_SKIP" -gt 0 ]; then
  echo "SKIPPED is not PASS either. Each skip above carries its reason."
fi

# ------------------------------------------------------- coverage of fixes --
# 14 fix commits on `ours` that are not on `main`:
#   git log --format='%h %s' ours ^main | grep '^[0-9a-f]* fix'
# The mapping fix -> repro is declared here (nothing on disk records it); the
# baseline/after columns are READ OFF THE FILESYSTEM, so a missing evidence
# file shows up as a hole instead of quietly vanishing from the table.
echo
echo "======================================================================"
echo "COVERAGE — 14 fix commits on 'ours', and what reproduces them"
echo "======================================================================"
COVERAGE=(
  "5891b2b|direct|A-2|fix(runtime): recount provider model_count on custom model removal"
  "366d62f|direct|A-7|fix(kernel): resolve fallback base_url per provider, not from default_model"
  "cdd70de|direct|A-4|fix(runtime): wrap runtime prompt data in tags, not ## headings"
  "63f9185|direct|-|fix(runtime): cap workspace context by whole file blocks"
  "b86e65b|direct|A-1|fix(api): PUT /agents/{id}/update returns 501 and names the working routes"
  "6cb20be|direct|A-1|fix(api): the 501 body was wrong about skills, mcp_servers and tools"
  "61dd183|direct|-|fix(runtime): drop a dead assignment clippy was right about"
  "bf0fee0|direct|-|fix(telegram): stop leaking the bot token into logs on network errors"
  "cbb0660|direct|A-6|fix(metering): meter usage per LLM call, not per agent turn"
  "308cd7a|direct|-|fix(kernel): report what hot-reload actually applied, not what it detected"
  "acc85d7|direct|FANG-43|fix(channels): stop token/secret leaks in Telegram file content and 6 adapters"
  "8b502f9|direct|FANG-45|fix(tools): make file_read disclose truncation and support offset/limit paging"
  "8a2f8d4|bundle|A-1,A-2,A-6,A-7|fix: four defects the adversarial review found in the sprint-2 patches"
  "d83abbf|bundle|FANG-43,FANG-45|fix: three defects the adversarial review found in the sprint-3 patches"
)
printf '%-9s %-7s %-18s %-10s %s\n' COMMIT KIND REPRO EVIDENCE SUBJECT
NO_REPRO=0; NO_GREEN=0; FULL=0; N_BUNDLE=0
for c in "${COVERAGE[@]}"; do
  sha="$(field "$c" 1)"; kind="$(field "$c" 2)"; repros="$(field "$c" 3)"; subj="$(field "$c" 4)"
  if [ "$repros" = "-" ]; then
    printf '%-9s %-7s %-18s %-10s %s\n' "$sha" "$kind" "none" "none" "$subj"
    NO_REPRO=$((NO_REPRO + 1))
    continue
  fi
  ev="" ; have_base=0; have_after=0
  IFS=',' read -ra rs <<<"$repros"
  for one in "${rs[@]}"; do
    [ -f "$HERE/baseline/$one.txt" ] && have_base=1
    { [ -f "$HERE/after/$one.txt" ] || [ -f "$HERE/after-v2/$one.txt" ]; } && have_after=1
  done
  if [ "$have_base" = 1 ] && [ "$have_after" = 1 ]; then ev="base+after"
  elif [ "$have_base" = 1 ]; then ev="base only"
  else ev="NONE"; fi
  printf '%-9s %-7s %-18s %-10s %s\n' "$sha" "$kind" "$repros" "$ev" "$subj"
  # Bundle commits amend patches that already have their own row above. They
  # inherit that coverage; counting them again would inflate the tally.
  if [ "$kind" = bundle ]; then
    N_BUNDLE=$((N_BUNDLE + 1))
  elif [ "$ev" = "base+after" ]; then FULL=$((FULL + 1))
  elif [ "$ev" = "base only" ]; then NO_GREEN=$((NO_GREEN + 1))
  else NO_REPRO=$((NO_REPRO + 1)); fi
done
echo
echo "Counted by fix commit (12 direct + 2 adversarial round-ups = 14):"
echo "  direct fixes with a baseline AND an after-the-fix run : $FULL"
echo "  direct fixes with a baseline but no green recorded    : $NO_GREEN"
echo "  direct fixes with no reproduction at all              : $NO_REPRO"
echo "  round-up commits inheriting the rows above            : $N_BUNDLE"
echo
echo "Counted by reproduction script (two commits can share one script, which"
echo "is why these numbers are smaller than the ones above):"
BR_FULL=""; BR_BASE=""; BR_NONE=""
for s in A-1 A-2 A-4 A-6 A-7 FANG-31 FANG-43 FANG-45 FANG-9 FANG-10 FANG-13 FANG-47; do
  [ -f "$HERE/$s.sh" ] || continue
  b=0; a=0
  [ -f "$HERE/baseline/$s.txt" ] && b=1
  { [ -f "$HERE/after/$s.txt" ] || [ -f "$HERE/after-v2/$s.txt" ]; } && a=1
  if   [ "$b" = 1 ] && [ "$a" = 1 ]; then BR_FULL="$BR_FULL $s"
  elif [ "$b" = 1 ];                 then BR_BASE="$BR_BASE $s"
  else                                    BR_NONE="$BR_NONE $s"; fi
done
echo "  baseline + after (a red run and a green run on record):$BR_FULL"
echo "  baseline only (red on record, no green yet)           :$BR_BASE"
echo "  no baseline on disk at all                            :$BR_NONE"
echo
echo "  Note on the 'baseline only' group: for FANG-43 and FANG-45 the missing"
echo "  green is a gap in the evidence. For FANG-9/10/13/31/47 it is not — those"
echo "  defects have no fix commit on 'ours', so red IS the current truth and"
echo "  there is nothing green to record until sprint 5."
echo
echo "Repros in this directory that are NOT tied to a fix commit — they"
echo "reproduce defects that are still open on 'ours', which is why their"
echo "expected verdict above is RED:"
for r in "${REPROS[@]}"; do
  case "$(field "$r" 6)" in
    *"NO FIX"*) printf '  %-8s %s\n' "$(field "$r" 1)" "$(field "$r" 6)" ;;
  esac
done
echo
echo "Baselines on disk:"
for f in "$HERE"/baseline/*.txt; do
  [ -f "$f" ] || continue
  printf '  %-28s %6s bytes\n' "$(basename "$f")" "$(wc -c <"$f" | tr -d ' ')"
done

echo
if [ "$N_ERROR" -gt 0 ]; then
  echo "run.sh: ERROR — $N_ERROR repro(s) could not produce a verdict at all."
  exit 2
fi
if [ "$N_FAIL" -gt 0 ]; then
  echo "run.sh: FAIL — $N_FAIL repro(s) did not reach the expected verdict."
  exit 1
fi
if [ $((N_PASS + N_FAIL + N_REVIEW)) -eq 0 ]; then
  echo "run.sh: NOTHING RAN — all $N_SKIP repro(s) were skipped, so this run"
  echo "        proves nothing. Reasons are listed above; fix them or say so."
  exit 4
fi
echo "run.sh: every repro that ran reached its expected verdict."
echo "        ($N_SKIP skipped, $N_REVIEW awaiting a human — neither is a pass.)"
exit 0
