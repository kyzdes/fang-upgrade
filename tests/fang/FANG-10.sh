#!/usr/bin/env bash
# FANG-10 — a turn that exhausts max_iterations is delivered to the caller as
# a bare HTTP 500 that carries none of the work the turn actually did.
#
# Root cause (source of truth: the code):
#
#   crates/openfang-runtime/src/agent_loop.rs:1202-1221 — the loop's exit
#   after `for iteration in 0..max_iterations`:
#       // Save session before failing so conversation history is preserved
#       memory.save_session_async(session)      <- the work IS kept on disk
#       hook_reg.fire(AgentLoopEnd, {"reason": "max_iterations_exceeded"})
#       Err(OpenFangError::MaxIterationsExceeded(max_iterations))
#   The Err carries a number and nothing else: no AgentLoopResult, so no
#   assistant text, no tool results, no total_usage, no calls[].
#
#   crates/openfang-kernel/src/kernel.rs:1968+ / :2493+ — the Err arm just
#   records a panic and re-wraps, and the API turns that into
#       HTTP 500 {"error":"Message delivery failed: Max iterations exceeded (50) ..."}
#
#   The result is a turn that really ran, really called tools, really changed
#   the agent's workspace — and reports itself as a flat failure with no
#   partial result attached. The caller cannot tell "nothing happened" from
#   "48 things happened and I am not going to tell you which".
#
# Shape: harness/scenarios/max-iterations-500.json answers `tool_call`
# FOREVER via `default_step` (that is the whole point — toolcall-then-text.json
# has a TEXT default and can never reach this exit). The tool is file_write to
# a fixed canary path, so the partial result is a real file, not a notion.
#
# Which exit door: MAX_ITERATIONS is 50 (agent_loop.rs:38) and the loop guard's
# global circuit breaker is raised to max_iterations*3 = 150 at agent_loop.rs:558,
# so the loop leaves by the max-iterations door. The error text below proves
# which door it was: a circuit break says "Circuit breaker: exceeded ... total
# tool calls", a max-iterations exit says "Max iterations exceeded (50)".
#
# CONTROL, in the same scenario: steps 0-1 are an ordinary tool_call-then-text
# turn on the same endpoint and the same agent, which must answer HTTP 200
# with a body. Without it a 500 could just mean the rig is broken.
#
# Exit codes: 0 RED, 1 GREEN, 2 REFUSED (prod), 3 SKIPPED, 4 INCONCLUSIVE.
# STAGING ONLY. Usage: ./FANG-10.sh [base_url]

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
CANARY="FANG10-PARTIAL-RESULT-CANARY"
PARTIAL="output/fang10-partial.txt"

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

cleanup() { "$RIG" down >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

echo "=== FANG-10 — max_iterations exceeded returns HTTP 500 with no partial result ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo

"$RIG" down >/dev/null 2>&1
"$RIG" up max-iterations-500 >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools file_write 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
WS="/data/workspaces/fangrig-probe"
echo "probe agent: fangrig-probe / $AID   workspace: $CONTAINER:$WS"
if docker exec "$CONTAINER" test -f "$WS/$PARTIAL" 2>/dev/null; then
  echo "SKIPPED: $PARTIAL already exists before the run — a stale workspace would"
  echo "         make the 'written during the failed turn' claim unfalsifiable."
  exit 3
fi
echo "pre-check: $PARTIAL absent before any turn"
echo

echo "--- turn 1 (CONTROL): an ordinary tool_call-then-text turn ---"
OUT1="$(say "$AID" "write the control file")"
S1="$(printf '%s' "$OUT1" | tail -1)"
B1="$(printf '%s' "$OUT1" | sed '$d')"
echo "HTTP $S1"
echo "$B1"
echo

echo "--- turn 2 (defect): the provider never stops asking for tools ---"
T0=$(date +%s)
OUT2="$(say "$AID" "start the unbounded task")"
T1=$(date +%s)
S2="$(printf '%s' "$OUT2" | tail -1)"
B2="$(printf '%s' "$OUT2" | sed '$d')"
echo "HTTP $S2   (turn took $((T1 - T0))s)"
echo "$B2"
echo

echo "--- the partial result the caller was not told about ---"
docker exec "$CONTAINER" ls -la "$WS/output/" 2>&1 | sed 's/^/  /'
if docker exec "$CONTAINER" test -f "$WS/$PARTIAL" 2>/dev/null; then
  PARTIAL_FILE=present
  echo "  content of $PARTIAL:"
  docker exec "$CONTAINER" cat "$WS/$PARTIAL" 2>&1 | sed 's/^/    /'
  echo
else
  PARTIAL_FILE=absent
fi
echo "  FANG10_PARTIAL_FILE=$PARTIAL_FILE"
echo

echo "--- and the session the runtime deliberately saved before failing ---"
SESSION_JSON="$(curl -sS -m 60 "${AUTH[@]}" "$BASE_URL/api/agents/$AID/session")"
SESS_MSGS="$(printf '%s' "$SESSION_JSON" | python3 -c "
import json,sys
try: d=json.loads(sys.stdin.read())
except Exception: print(-1); sys.exit()
m = d.get('messages', d if isinstance(d, list) else [])
print(len(m) if isinstance(m, list) else -1)
")"
echo "  messages in the saved session after the 500: $SESS_MSGS"
echo

echo "--- what the provider actually served (stub journal rollup) ---"
ROLL="$("$RIG" journal --rollup 2>/dev/null)"
# The full log is 52 lines long; print the shape of it, not the whole thing.
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
echo

# ------------------------------------------------------------- assertions --
if [ "$S1" != "200" ]; then
  echo "RESULT: INCONCLUSIVE — the control turn did not answer 200 (got $S1)."
  echo "        A 500 on turn 2 would say nothing about max_iterations."
  exit 4
fi
if [ "$SERVED" != "52" ]; then
  echo "RESULT: INCONCLUSIVE — the stub served $SERVED calls, expected 52"
  echo "        (2 for the control turn + 50 for the 50 iterations)."
  exit 4
fi
echo "control OK: turn 1 answered 200, and the stub served exactly 52 calls —"
echo "            2 for the control and one per iteration for the 50 that followed."
echo

HAS_MAXITER=no
printf '%s' "$B2" | grep -q 'Max iterations exceeded' && HAS_MAXITER=yes
HAS_CANARY=no
printf '%s' "$B2" | grep -q "$CANARY" && HAS_CANARY=yes
HAS_RESPONSE=no
printf '%s' "$B2" | grep -q '"response"' && HAS_RESPONSE=yes

echo "FANG10_STATUS=$S2                    # want: a status the caller can act on"
echo "FANG10_EXIT_DOOR=$HAS_MAXITER        # yes = max_iterations, not the circuit breaker"
echo "FANG10_BODY_HAS_RESPONSE=$HAS_RESPONSE  # want: yes — some partial answer"
echo "FANG10_BODY_HAS_CANARY=$HAS_CANARY   # want: yes — the work is named"
echo "FANG10_PARTIAL_FILE=$PARTIAL_FILE    # the work really happened"
echo "FANG10_SESSION_MESSAGES=$SESS_MSGS   # ...and the runtime kept it, for itself"

if [ "$S2" = "500" ] && [ "$HAS_MAXITER" = yes ] && [ "$HAS_RESPONSE" = no ] \
   && [ "$HAS_CANARY" = no ] && [ "$PARTIAL_FILE" = present ]; then
  echo
  echo "RESULT: RED — 50 iterations ran, a real file was written to the workspace,"
  echo "        the session was saved on the way out, and the caller got a 500"
  echo "        containing none of it."
  exit 0
fi
echo
echo "RESULT: GREEN — the max-iterations exit no longer discards the turn's result"
exit 1
