#!/usr/bin/env bash
# FANG-47 — when a turn ends on max_iterations, the whole meter for that turn
# is thrown away. Not rounded, not partially attributed: gone.
#
# Root cause (source of truth: the code):
#
#   crates/openfang-runtime/src/agent_loop.rs:630-638 — every iteration adds
#   to the running total, right up to the last one:
#       total_usage.input_tokens  += response.usage.input_tokens;
#       total_usage.output_tokens += response.usage.output_tokens;
#       record_call(&mut calls, iteration, &manifest.model, &report, response.usage);
#
#   crates/openfang-kernel/src/kernel.rs — but every consumer of that total
#   hangs off the Ok arm, and only the Ok arm:
#       :1969-1975   scheduler.record_usage / record_tool_calls   (quota)
#       :2456-2472   the same, plus record_turn_usage             (streaming)
#       record_turn_usage is what writes the per-call rows behind
#       /api/usage/by-model, /api/usage/summary and /api/usage/daily.
#   The Err arm is three lines: supervisor.record_panic(), a warn!, return Err.
#
#   agent_loop.rs:1221 returns Err(MaxIterationsExceeded) — so total_usage,
#   which by then holds every token of every iteration, is dropped with the
#   error. The provider has already billed for those calls. Afterwards
#   nothing in the product can tell you they happened.
#
# Shape: harness/scenarios/max-iterations-usage-lost.json answers `tool_call`
# forever via `default_step` (the only shape that reaches this exit), and every
# step reports the same deliberately unmistakable usage — 7919 in / 7907 out,
# two primes that appear nowhere else in this box's ledger — so "the meter did
# not move" is a subtraction, not an impression. 50 iterations x 7919/7907 =
# 395950 input / 395350 output tokens served and never booked.
#
# CONTROL, in the same scenario and against the same by-model row: steps 0-1
# are an ordinary tool_call-then-text turn with the same usage numbers, and
# /api/usage/by-model for adv-primary MUST move by exactly +2 calls /
# +15838 input / +15814 output. Without that control a zero delta on turn 2
# would only prove that this model is not metered at all.
#
# Exit codes: 0 RED, 1 GREEN, 2 REFUSED (prod), 3 SKIPPED, 4 INCONCLUSIVE.
# STAGING ONLY. Usage: ./FANG-47.sh [base_url]

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RIG="$HERE/harness/fangrig"
BASE_URL="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
CONTAINER="${OF_CONTAINER:-openfang-staging}"
CONFIG="${OF_CONFIG:-/var/lib/docker/volumes/openfang-staging-data/_data/config.toml}"

# The target is a URL *and* a container *and* a config file, and fangrig —
# which does the actual creating on the stand — reads all three from the
# environment (harness/lib.sh:16-18), never from our argv. Without these
# exports, `./FANG-x.sh http://127.0.0.1:4213` politely reported one stand in
# its header while fangrig edited the config.toml of, and spawned an agent on,
# whatever sat on lib.sh's default :4201. Export them; one target, one place.
export OPENFANG_URL="$BASE_URL"
export OF_CONTAINER="$CONTAINER"
export OF_CONFIG="$CONFIG"
export OPENFANG_CONFIG="$CONFIG"
MODEL="adv-primary"
STEP_IN=7919
STEP_OUT=7907

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. Staging is :4201." >&2; exit 2 ;;
esac
[ "$CONTAINER" = "openfang-openfang-1" ] && { echo "REFUSING: $CONTAINER is production." >&2; exit 2; }

for b in docker python3 curl; do
  command -v "$b" >/dev/null 2>&1 || { echo "SKIPPED: missing dependency: $b"; exit 3; }
done
docker inspect "$CONTAINER" >/dev/null 2>&1 || { echo "SKIPPED: container $CONTAINER not found"; exit 3; }
[ -f "$CONFIG" ] || { echo "SKIPPED: no staging config at $CONFIG"; exit 3; }
[ -x "$RIG" ] || { echo "SKIPPED: harness/fangrig not executable"; exit 3; }
python3 - "$BASE_URL" <<'PY' || { echo "SKIPPED: staging API not answering at $BASE_URL"; exit 3; }
import sys, urllib.request
try:
    urllib.request.urlopen(sys.argv[1] + "/api/health", timeout=5)
except Exception:
    sys.exit(1)
PY

API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)"
AUTH=(); [ -n "$API_KEY" ] && AUTH=(-H "Authorization: Bearer $API_KEY")

say() {
  local id="$1" text="$2" body
  curl -sS -m 120 -X POST -H 'Content-Type: application/json' "${AUTH[@]}" -d '{}' \
       "$BASE_URL/api/agents/$id/session/reset" >/dev/null
  body="$(python3 -c "import json,sys;print(json.dumps({'message': sys.argv[1]}))" "$text")"
  curl -sS -m 600 -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
       "${AUTH[@]}" -d "$body" "$BASE_URL/api/agents/$id/message"
}

# by_model -> "call_count input_tokens output_tokens" for $MODEL, or "0 0 0"
by_model() {
  curl -sS -m 60 "${AUTH[@]}" "$BASE_URL/api/usage/by-model" | python3 -c "
import json,sys
try: rows = json.load(sys.stdin).get('models', [])
except Exception: rows = []
for r in rows:
    if r.get('model') == '$MODEL':
        print(r.get('call_count',0), r.get('total_input_tokens',0), r.get('total_output_tokens',0))
        break
else:
    print(0, 0, 0)
"
}

cleanup() { "$RIG" down >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

echo "=== FANG-47 — the max-iterations exit loses the turn's accounting entirely ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo "meter : GET /api/usage/by-model, row model=$MODEL"
echo

"$RIG" down >/dev/null 2>&1
"$RIG" up max-iterations-usage-lost >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools memory_store 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
echo "probe agent: fangrig-probe / $AID"
echo

read -r C0 I0 O0 <<<"$(by_model)"
echo "meter BEFORE any turn : calls=$C0 input=$I0 output=$O0"
echo

echo "--- turn 1 (CONTROL): an ordinary 2-call turn, $STEP_IN/$STEP_OUT per call ---"
OUT1="$(say "$AID" "control")"
S1="$(printf '%s' "$OUT1" | tail -1)"
B1="$(printf '%s' "$OUT1" | sed '$d')"
echo "HTTP $S1"
echo "$B1"
read -r C1 I1 O1 <<<"$(by_model)"
echo "meter AFTER control   : calls=$C1 input=$I1 output=$O1"
echo "delta                 : calls=$((C1 - C0)) input=$((I1 - I0)) output=$((O1 - O0))"
echo

echo "--- turn 2 (defect): the provider never stops asking for tools ---"
OUT2="$(say "$AID" "start the unbounded task")"
S2="$(printf '%s' "$OUT2" | tail -1)"
B2="$(printf '%s' "$OUT2" | sed '$d')"
echo "HTTP $S2"
echo "$B2"
read -r C2 I2 O2 <<<"$(by_model)"
echo "meter AFTER defect    : calls=$C2 input=$I2 output=$O2"
echo "delta                 : calls=$((C2 - C1)) input=$((I2 - I1)) output=$((O2 - O1))"
echo

echo "--- what the provider actually served and billed (stub journal) ---"
ROLL="$("$RIG" journal --rollup 2>/dev/null)"
printf '%s' "$ROLL" | python3 -c "
import json,sys,collections
try: d=json.load(sys.stdin)
except Exception: print('  <no rollup>'); sys.exit()
kinds = collections.Counter(l.split('step=')[-1] for l in d.get('log',[]))
print('  scenario : %s' % d.get('scenario'))
print('  counts   : %s' % d.get('counts'))
print('  statuses : %s' % d.get('statuses'))
print('  step mix : %s' % dict(kinds))
"
SERVED="$(printf '%s' "$ROLL" | python3 -c "
import json,sys
try: print(json.load(sys.stdin).get('counts',{}).get('primary',0))
except Exception: print(0)
")"
DEFECT_CALLS=$((SERVED - 2))
echo

# ------------------------------------------------------------- assertions --
CTL_CALLS=$((C1 - C0)); CTL_IN=$((I1 - I0)); CTL_OUT=$((O1 - O0))
WANT_IN=$((2 * STEP_IN)); WANT_OUT=$((2 * STEP_OUT))
if [ "$S1" != "200" ] || [ "$CTL_CALLS" != 2 ] || [ "$CTL_IN" != "$WANT_IN" ] || [ "$CTL_OUT" != "$WANT_OUT" ]; then
  echo "RESULT: INCONCLUSIVE — the control turn did not move the meter as scripted"
  echo "        (HTTP $S1, delta calls=$CTL_CALLS input=$CTL_IN output=$CTL_OUT,"
  echo "        expected 200 / 2 / $WANT_IN / $WANT_OUT). A zero delta on turn 2"
  echo "        would then only mean this model is not metered at all."
  exit 4
fi
if [ "$SERVED" != "52" ]; then
  echo "RESULT: INCONCLUSIVE — the stub served $SERVED calls, expected 52"
  echo "        (2 for the control turn + 50 for the 50 iterations)."
  exit 4
fi
echo "control OK: the same meter, the same model and the same stub moved by exactly"
echo "            +2 calls / +$WANT_IN input / +$WANT_OUT output for turn 1."
echo

DEF_CALLS=$((C2 - C1)); DEF_IN=$((I2 - I1)); DEF_OUT=$((O2 - O1))
SERVED_IN=$((DEFECT_CALLS * STEP_IN)); SERVED_OUT=$((DEFECT_CALLS * STEP_OUT))
echo "FANG47_SERVED_CALLS=$DEFECT_CALLS       # LLM calls the provider really answered"
echo "FANG47_SERVED_INPUT=$SERVED_IN    # tokens the provider really billed"
echo "FANG47_SERVED_OUTPUT=$SERVED_OUT"
echo "FANG47_BOOKED_CALLS=$DEF_CALLS          # what /api/usage/by-model recorded (want: $DEFECT_CALLS)"
echo "FANG47_BOOKED_INPUT=$DEF_IN"
echo "FANG47_BOOKED_OUTPUT=$DEF_OUT"

if [ "$S2" = "500" ] && [ "$DEF_CALLS" = 0 ] && [ "$DEF_IN" = 0 ] && [ "$DEF_OUT" = 0 ]; then
  echo
  echo "RESULT: RED — $DEFECT_CALLS LLM calls and $SERVED_IN/$SERVED_OUT tokens were served"
  echo "        and billed, the turn ended 500, and the ledger moved by zero."
  exit 0
fi
echo
echo "RESULT: GREEN — the max-iterations exit no longer discards the turn's accounting"
exit 1
