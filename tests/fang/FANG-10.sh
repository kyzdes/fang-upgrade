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
# Which exit door: MAX_ITERATIONS is 50 and the loop guard's global circuit
# breaker is raised to max_iterations*3 = 150 for autonomous agents, so the loop
# leaves by the max-iterations door. The text below proves which door it was: a
# circuit break says "Circuit breaker: exceeded ... total tool calls", a
# max-iterations exit says "Max iterations exceeded (50)".
#
# And the same loop guard is why this repro checks the summary's arithmetic and
# not just its existence. Every iteration asks for the SAME file_write with the
# SAME arguments, so the per-hash block threshold (5 identical calls) stops most
# of them: only the first few reach the tool at all. A summary that reports all
# 50 as executed is announcing work that never happened — which is the defect
# FANG-9 is open for, committed inside the patch that closes FANG-10. So the
# green criterion below counts both groups and makes them add up to 50, and
# checks the blocked count against the guard's own refusals in the transcript.
#
# TWO SURFACES, because there are two loops. POST /api/agents/{id}/message goes
# through run_agent_loop; the WebSocket at /api/agents/{id}/ws goes through
# run_agent_loop_streaming (ws.rs:700 -> kernel.rs:2389), which had its own copy
# of the same `Err(MaxIterationsExceeded)` and its own way of losing the turn:
# a WS client's transcript is the concatenation of the text_delta frames
# (ws.rs:781-782), so a streaming turn that ends without emitting one ends
# having said nothing. Turn 3 below is the same defect over that socket.
#
# What turn 3 does NOT establish: this scenario's steps carry no assistant text
# alongside their tool calls, so it cannot show whether the final delta repeats
# text the client already had. That property is asserted where it can be —
# agent_loop::tests::test_streaming_max_iterations_notice_does_not_repeat_the_
# streamed_text drains the loop's own StreamEvent channel and counts each chunk.
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

# The key resolves the way ofctl and harness/lib.sh resolve it: the environment
# first, then the config file. Config-only made tests/fang/run.sh's preflight a
# statement about a credential this script never sent — it proved $OPENFANG_API_KEY
# against the target, printed "credential: ACCEPTED", and then every call below
# went out with whatever api_key happened to sit in $CONFIG.
API_KEY="${OPENFANG_API_KEY:-$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)}"
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

echo "--- turn 3 (defect, second surface): the same turn over the WebSocket ---"
WS_JSON='{}'
if [ -x "$HERE/harness/ws_probe.py" ]; then
  curl -sS -m 120 -X POST -H 'Content-Type: application/json' "${AUTH[@]}" -d '{}' \
       "$BASE_URL/api/agents/$AID/session/reset" >/dev/null
  WS_JSON="$("$HERE/harness/ws_probe.py" "$BASE_URL" "$AID" "start the unbounded task" "$API_KEY" 600)"
else
  echo "  (harness/ws_probe.py missing or not executable — no WS observation)"
fi
printf '%s' "$WS_JSON" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print('  <no ws result>'); sys.exit()
print('  terminal      : %s' % d.get('terminal'))
print('  frames        : %s' % d.get('frames'))
print('  text_delta    : %d frame(s)' % len(d.get('deltas') or []))
print('  detail        : %s' % d.get('detail'))
r = d.get('response') or {}
if r:
    print('  response      : iterations=%s in=%s out=%s' % (
        r.get('iterations'), r.get('input_tokens'), r.get('output_tokens')))
    print('  response.content:')
    for line in (r.get('content') or '').splitlines()[:8]:
        print('    ' + line)
    n = len((r.get('content') or '').splitlines())
    if n > 8: print('    ... (%d more lines)' % (n - 8))
"
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
# 2 for the control turn, 50 for turn 2's iterations, 50 for turn 3's. If the
# WS probe could not run, turn 3 served nothing and the expected total is 52.
WS_RAN_AT_ALL=no
printf '%s' "$WS_JSON" | grep -q '"frames": *[1-9]' && WS_RAN_AT_ALL=yes
if [ "$WS_RAN_AT_ALL" = yes ]; then EXPECT_SERVED=102; else EXPECT_SERVED=52; fi
echo

# ------------------------------------------------------------- assertions --
if [ "$S1" != "200" ]; then
  echo "RESULT: INCONCLUSIVE — the control turn did not answer 200 (got $S1)."
  echo "        A 500 on turn 2 would say nothing about max_iterations."
  exit 4
fi
if [ "$SERVED" != "$EXPECT_SERVED" ]; then
  echo "RESULT: INCONCLUSIVE — the stub served $SERVED calls, expected $EXPECT_SERVED"
  echo "        (2 for the control turn, 50 for turn 2's iterations, and 50 more"
  echo "        for turn 3's if the WebSocket probe ran — it did: $WS_RAN_AT_ALL)."
  exit 4
fi
echo "control OK: turn 1 answered 200, and the stub served exactly $EXPECT_SERVED calls —"
echo "            2 for the control and one per iteration for each turn that followed."
echo

HAS_MAXITER=no
printf '%s' "$B2" | grep -q 'Max iterations exceeded' && HAS_MAXITER=yes
HAS_CANARY=no
printf '%s' "$B2" | grep -q "$CANARY" && HAS_CANARY=yes
HAS_RESPONSE=no
printf '%s' "$B2" | grep -q '"response"' && HAS_RESPONSE=yes

# What the summary claims about the turn's tool calls, taken apart. The
# scenario asks for the SAME file_write with the SAME arguments every
# iteration, which is precisely what the loop guard blocks from the 5th
# occurrence on (LoopGuardConfig::default().block_threshold = 5). So most of
# the 50 calls never reached the tool, and a summary that counts all 50 as
# executed is reporting work that did not happen — the defect FANG-9 is open
# for, in the patch that closes FANG-10.
read -r RAN STOPPED <<<"$(printf '%s' "$B2" | python3 -c "
import json,re,sys
try: body = json.load(sys.stdin)
except Exception: print(-1, -1); sys.exit()
resp = body.get('response','')
def count(pat):
    m = re.search(pat + r' \((\d+)\)', resp)
    return int(m.group(1)) if m else 0
print(count('Tool calls that ran and succeeded'), count('Tool calls stopped before they ran'))
")"

# The independent count. Every call the loop guard stops leaves its refusal in
# the transcript as the tool result ("Blocked: tool '...' called N times with
# identical parameters"), and the transcript is written by the guard, not by
# the summary. If the two disagree, the summary is making it up.
GUARD_BLOCKS="$(printf '%s' "$SESSION_JSON" | grep -o 'Blocked: tool' | wc -l | tr -d ' ')"

# The same three questions of the WebSocket turn, asked of what the socket
# actually carried. WS_NOTICE_IN_DELTAS counts the notice in the concatenated
# text_delta frames — that concatenation IS the client's transcript (ws.rs
# builds `response` from it), so 1 means a streaming caller was told why the
# turn ended, and 0 means the turn ended saying nothing.
read -r WS_STATE WS_NOTICE_IN_DELTAS WS_RAN WS_STOPPED <<<"$(printf '%s' "$WS_JSON" | python3 -c "
import json,re,sys
try: d = json.load(sys.stdin)
except Exception: print('noresult 0 -1 -1'); sys.exit()
deltas = ''.join(d.get('deltas') or [])
seen = len(re.findall(r'Max iterations exceeded \(50\)', deltas))
term = d.get('terminal') or 'noresult'
r = d.get('response')
if term != 'response' or not r:
    print(term, seen, -1, -1); sys.exit()
def count(pat, hay):
    m = re.search(pat + r' \((\d+)\)', hay)
    return int(m.group(1)) if m else 0
content = r.get('content','')
print('response', seen,
      count('Tool calls that ran and succeeded', content),
      count('Tool calls stopped before they ran', content))
")"

echo "FANG10_STATUS=$S2                       # want: 200 — a status the caller can act on"
echo "FANG10_EXIT_DOOR=$HAS_MAXITER           # want: yes = max_iterations, not the circuit breaker"
echo "FANG10_BODY_HAS_RESPONSE=$HAS_RESPONSE     # want: yes — the partial answer is in the body"
echo "FANG10_BODY_HAS_CANARY=$HAS_CANARY         # want: yes — the work is named concretely"
echo "FANG10_RAN=$RAN                          # want: >0  — calls the summary says ran"
echo "FANG10_STOPPED=$STOPPED                     # want: >0  — calls it says the guard stopped"
echo "FANG10_ACCOUNTED=$((RAN + STOPPED))                 # want: 50 — one per iteration, none unaccounted"
echo "FANG10_GUARD_BLOCKS_IN_TRANSCRIPT=$GUARD_BLOCKS   # want: = FANG10_STOPPED, counted from the session"
echo "FANG10_PARTIAL_FILE=$PARTIAL_FILE       # the work the first calls did really happened"
echo "FANG10_SESSION_MESSAGES=$SESS_MSGS      # ...and the runtime kept it"
echo "FANG10_WS_STATE=$WS_STATE           # want: response — the streaming loop answered"
echo "FANG10_WS_NOTICE_IN_DELTAS=$WS_NOTICE_IN_DELTAS       # want: 1 — the client's transcript names the exit door"
echo "FANG10_WS_RAN=$WS_RAN                       # want: >0"
echo "FANG10_WS_STOPPED=$WS_STOPPED                  # want: >0"
echo "FANG10_WS_ACCOUNTED=$((WS_RAN + WS_STOPPED))                # want: 50 — same arithmetic as the REST turn"

# RED — the defect as filed: a bare 500 with none of the turn in it, and the
# streaming surface losing the turn its own way (an error frame, no notice).
if [ "$S2" = "500" ] && [ "$HAS_MAXITER" = yes ] && [ "$HAS_RESPONSE" = no ] \
   && [ "$HAS_CANARY" = no ] && [ "$PARTIAL_FILE" = present ] \
   && [ "$WS_NOTICE_IN_DELTAS" = 0 ]; then
  echo
  echo "RESULT: RED — 50 iterations ran, a real file was written to the workspace,"
  echo "        the session was saved on the way out, and the caller got a 500"
  echo "        containing none of it. Over the WebSocket the same turn came back"
  echo "        as '$WS_STATE' with the exit door named in 0 text_delta frames."
  exit 0
fi

# GREEN — stated as what must be PRESENT, not as "not exactly the old RED".
# Each line is a thing the caller can now do that it could not before:
#   1. a 200 it can act on,                    5. a count of what was stopped,
#   2. the exit door named,                    6. those two adding up to the 50
#   3. the partial answer and its canary,         calls the stub served, with the
#   4. a count of what actually ran,              stopped count equal to the
#                                                 transcript's own guard refusals,
#   7. and the same answer over the WebSocket: the streaming loop reached its
#      caller at all, named the exit door in the frames that make up the
#      client's transcript, and did the same arithmetic.
if [ "$S2" = "200" ] && [ "$HAS_MAXITER" = yes ] && [ "$HAS_RESPONSE" = yes ] \
   && [ "$HAS_CANARY" = yes ] && [ "$PARTIAL_FILE" = present ] \
   && [ "$RAN" -gt 0 ] && [ "$STOPPED" -gt 0 ] \
   && [ "$((RAN + STOPPED))" = 50 ] && [ "$STOPPED" = "$GUARD_BLOCKS" ] \
   && [ "$WS_STATE" = response ] && [ "$WS_NOTICE_IN_DELTAS" = 1 ] \
   && [ "$WS_RAN" -gt 0 ] && [ "$WS_STOPPED" -gt 0 ] \
   && [ "$((WS_RAN + WS_STOPPED))" = 50 ]; then
  echo
  echo "RESULT: GREEN — the max-iterations exit answers 200 carrying the turn:"
  echo "        the exit door is named, the partial answer and its canary are in"
  echo "        the body, and all 50 calls are accounted for — $RAN that ran and"
  echo "        $STOPPED the loop guard stopped, which is exactly the $GUARD_BLOCKS"
  echo "        refusals the saved transcript records. Over the WebSocket the"
  echo "        streaming loop answered too: the exit door reached the client's"
  echo "        transcript in $WS_NOTICE_IN_DELTAS text_delta frame, and its own 50"
  echo "        calls split $WS_RAN ran / $WS_STOPPED stopped."
  exit 1
fi

echo
echo "RESULT: INCONCLUSIVE — the turn matched neither the filed defect nor the"
echo "        fixed behaviour. Read the six FANG10_* lines above: each is a"
echo "        thing the fixed exit must show, and at least one of them is not"
echo "        what it should be."
exit 4
