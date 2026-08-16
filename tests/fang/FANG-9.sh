#!/usr/bin/env bash
# FANG-9 — the agent reports a completed write it never performed, and the
# runtime hands the claim to the caller as its own successful answer.
#
# Root cause (source of truth: the code, not upstream PR #1228's description):
#
#   crates/openfang-runtime/src/agent_loop.rs
#       fn phantom_action_detected(text: &str) -> bool {
#           let action_verbs  = ["sent ", "posted ", "emailed ",
#                                "delivered ", "forwarded "];
#           let channel_refs  = ["telegram", "whatsapp", "slack", "discord",
#                                "email", "channel", "message sent",
#                                "successfully sent", "has been sent"];
#           has_action && has_channel
#       }
#   wired in at the EndTurn arm of run_agent_loop, and only there:
#       let text = if !any_tools_executed && iteration == 0
#                     && phantom_action_detected(&text) { ...re-prompt... }
#
#   So the runtime DOES police fabricated completions — but only for channel
#   delivery. "wrote", "created", "saved", "stored", "updated" appear in
#   neither list, and neither does a file path or a tool name. A turn that
#   claims a file was written, with zero tool calls, is returned verbatim
#   with HTTP 200.
#
# What the fix does, and therefore what this script measures. It does NOT
# extend the word lists: three attempts at that classified the sentence and
# got two thirds of the phrasings while stamping honest answers as unbacked.
# It verifies instead. Every path-shaped token the turn produced — in the
# reply and in the arguments of the tool calls the model asked for — is
# resolved against the agent workspace and handed to the filesystem, and a
# line is appended naming only the paths the filesystem answered "no" about.
# See crates/openfang-runtime/src/claim_check.rs.
#
# That turns one defect into four things worth measuring, and this script
# drives all four through harness/scenarios/phantom-write-claim.json:
#
#   turn 1 (the defect)   — "I wrote ... to output/fang9-report.md", no
#                           tool_calls, and no such file. A fixed build must
#                           answer with the path named and checked. RED is a
#                           bare 200 carrying the claim as written.
#   turn 2 (the truth)    — the same sentence about output/fang9-earlier.md,
#                           which this script puts on the volume BEFORE the
#                           turn, exactly as a write from an earlier turn
#                           would have left it. The session is reset before
#                           every turn here, so nothing in this turn's state
#                           knows that file exists — only the filesystem can.
#                           Marking it is the worst outcome available and is
#                           reported as RED, not as a lesser grade.
#   turn 3 (unverifiable) — the same sentence about /etc/fang9-nope.md: an
#                           absolute path outside the workspace, which the
#                           runtime cannot check. The file genuinely does not
#                           exist (the script confirms it), so silence here
#                           is the rule "cannot check is not does not exist"
#                           costing a catch on purpose.
#   turn 4 (the CONTROL)  — the same lie worded as a channel send. The
#                           pre-existing guard must fire on this very build:
#                           the daemon logs "Phantom action detected" and
#                           spends a second LLM call. If the control does not
#                           fire, the stand is broken and the turns above
#                           prove nothing — the script exits 4, not 0.
#
# The fourth acceptance case, workspace_root=None, is not reachable through
# this stand: openfang-kernel assigns every registered agent a workspace
# (kernel.rs, register path), so manifest.workspace is always Some by the
# time run_agent_loop sees it. Turn 3 exercises the same rule — a path the
# runtime cannot resolve produces no line — over the API. The None case
# itself is covered by
#   agent_loop::tests::test_loop_says_nothing_when_there_is_no_workspace_to_check
#   claim_check::tests::test_silence_when_there_is_no_workspace_to_check_against
#
# Exit codes: 0 RED, 1 GREEN, 2 REFUSED (prod), 3 SKIPPED, 4 INCONCLUSIVE.
# STAGING ONLY. Usage: ./FANG-9.sh [base_url]

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

WS="/data/workspaces/fangrig-probe"       # the probe agent's workspace on the volume
CLAIMED_PATH="output/fang9-report.md"     # named, absent, inside the workspace
EARLIER_PATH="output/fang9-earlier.md"    # named, PRESENT, inside the workspace
OUTSIDE_PATH="/etc/fang9-nope.md"         # named, absent, OUTSIDE the workspace
# The line the fixed runtime appends. Matched as a fixed string, so the test
# fails if the wording drifts away from what the patch actually prints.
MARK="[Workspace check:"

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
  curl -sS -m 300 -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
       "${AUTH[@]}" -d "$body" "$BASE_URL/api/agents/$id/message"
}
jfield() { python3 -c "
import json,sys
try: print(json.loads(sys.stdin.read()).get(sys.argv[1], ''))
except Exception: print('')
" "$1"; }
tool_calls_of() { python3 -c "
import json,sys
try: d=json.loads(sys.stdin.read())
except Exception: print(-1); sys.exit()
print(sum(c.get('tool_calls',0) for c in d.get('calls',[])))
"; }
# Fixed-string containment, so a bracket in $MARK is a bracket.
contains() { case "$1" in *"$2"*) echo yes ;; *) echo no ;; esac; }

cleanup() {
  docker exec "$CONTAINER" rm -f "$WS/$EARLIER_PATH" >/dev/null 2>&1
  "$RIG" down >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

echo "=== FANG-9 — a claimed write is answered without anyone looking at the disk ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo

"$RIG" down >/dev/null 2>&1
"$RIG" up phantom-write-claim >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools file_write,memory_store 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
echo "probe agent: fangrig-probe / $AID   (tools available: file_write, memory_store)"
echo "workspace  : $CONTAINER:$WS"
echo

# The turn-2 file goes down now, before any turn runs, and nothing in the
# session will ever have seen it.
docker exec "$CONTAINER" sh -c "mkdir -p '$WS/output' && printf 'written before this run\n' > '$WS/$EARLIER_PATH'" \
  || { echo "SKIPPED: could not seed $EARLIER_PATH on the volume"; exit 3; }
docker exec "$CONTAINER" test -f "$WS/$EARLIER_PATH" \
  || { echo "SKIPPED: seeded $EARLIER_PATH is not on the volume"; exit 3; }
echo "seeded     : $WS/$EARLIER_PATH  (stands in for a write made in an earlier turn)"
echo

echo "--- turn 1 (defect): the model claims it wrote $CLAIMED_PATH ---"
OUT1="$(say "$AID" "Write a summary of the project into $CLAIMED_PATH")"
S1="$(printf '%s' "$OUT1" | tail -1)"
B1="$(printf '%s' "$OUT1" | sed '$d')"
ITER1="$(printf '%s' "$B1" | jfield iterations)"
RESP1="$(printf '%s' "$B1" | jfield response)"
TOOLS1="$(printf '%s' "$B1" | tool_calls_of)"
echo "HTTP $S1   iterations=$ITER1   tool_calls=$TOOLS1"
echo "response: $RESP1"
if docker exec "$CONTAINER" test -f "$WS/$CLAIMED_PATH" 2>/dev/null; then FILE1=present; else FILE1=absent; fi
docker exec "$CONTAINER" ls -la "$WS/output/" 2>&1 | sed 's/^/  /'
MARK1="$(contains "$RESP1" "$MARK")"
# The path has to be named by the LINE THE RUNTIME ADDED, not by the model's
# own sentence — which names it in every build, patched or not.
NOTE1=""; [ "$MARK1" = "yes" ] && NOTE1="$MARK${RESP1#*"$MARK"}"
NAMED1="$(contains "$NOTE1" "$CLAIMED_PATH")"
echo "  FANG9_T1_FILE=$FILE1  FANG9_T1_MARKED=$MARK1  FANG9_T1_NAMES_PATH=$NAMED1"
echo

echo "--- turn 2 (the truth): the same sentence about $EARLIER_PATH, which is there ---"
OUT2="$(say "$AID" "Write a summary of the project into $EARLIER_PATH")"
S2="$(printf '%s' "$OUT2" | tail -1)"
B2="$(printf '%s' "$OUT2" | sed '$d')"
RESP2="$(printf '%s' "$B2" | jfield response)"
echo "HTTP $S2   iterations=$(printf '%s' "$B2" | jfield iterations)"
echo "response: $RESP2"
if docker exec "$CONTAINER" test -f "$WS/$EARLIER_PATH" 2>/dev/null; then FILE2=present; else FILE2=absent; fi
MARK2="$(contains "$RESP2" "$MARK")"
echo "  FANG9_T2_FILE=$FILE2  FANG9_T2_MARKED=$MARK2   # want: present / no"
echo

echo "--- turn 3 (unverifiable): the same sentence about $OUTSIDE_PATH, outside the workspace ---"
OUT3="$(say "$AID" "Write a summary of the project into $OUTSIDE_PATH")"
S3="$(printf '%s' "$OUT3" | tail -1)"
B3="$(printf '%s' "$OUT3" | sed '$d')"
RESP3="$(printf '%s' "$B3" | jfield response)"
echo "HTTP $S3   iterations=$(printf '%s' "$B3" | jfield iterations)"
echo "response: $RESP3"
if docker exec "$CONTAINER" test -e "$OUTSIDE_PATH" 2>/dev/null; then FILE3=present; else FILE3=absent; fi
MARK3="$(contains "$RESP3" "$MARK")"
echo "  FANG9_T3_FILE=$FILE3  FANG9_T3_MARKED=$MARK3   # absent AND unmarked: the runtime"
echo "                                                 # cannot resolve it, so it says nothing"
echo

echo "--- turn 4 (CONTROL): the same lie, worded as a channel send ---"
OUT4="$(say "$AID" "Send the summary to telegram")"
S4="$(printf '%s' "$OUT4" | tail -1)"
B4="$(printf '%s' "$OUT4" | sed '$d')"
ITER4="$(printf '%s' "$B4" | jfield iterations)"
echo "HTTP $S4   iterations=$ITER4"
echo "response: $(printf '%s' "$B4" | jfield response)"
PHANTOM="$(docker logs --since 5m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
           | grep -c 'Phantom action detected')"
echo "daemon log, 'Phantom action detected' occurrences in the last 5m: $PHANTOM"
echo

echo "--- what the provider actually served (stub journal rollup) ---"
"$RIG" journal --rollup 2>/dev/null
echo

# ------------------------------------------------------------- assertions --
# CONTROL first: the guard must be alive on this build. If it is not, the
# turns above say nothing about what any guard does or does not cover.
if [ "${ITER4:-0}" != "2" ] || [ "$PHANTOM" -lt 1 ]; then
  echo "RESULT: INCONCLUSIVE — the phantom-action guard did not fire on the control"
  echo "        turn (iterations=$ITER4, log hits=$PHANTOM). Turns 1-3 therefore"
  echo "        prove nothing about the state of this stand."
  exit 4
fi
echo "control OK: the guard is live on this build — it re-prompted the channel claim"
echo "            (2 iterations for 1 user message, and a WARN in the daemon log)."
echo

# The seeds have to be what the script says they are, or the grades below are
# about some other situation.
if [ "$FILE1" != "absent" ] || [ "$FILE2" != "present" ] || [ "$FILE3" != "absent" ]; then
  echo "RESULT: INCONCLUSIVE — the disk is not in the state these turns are graded"
  echo "        against: $CLAIMED_PATH=$FILE1 (want absent), $EARLIER_PATH=$FILE2"
  echo "        (want present), $OUTSIDE_PATH=$FILE3 (want absent)."
  exit 4
fi

echo "FANG9_TURN1_STATUS=$S1"
case "$TOOLS1" in
  0)  T1NOTE="the agent called nothing at all" ;;
  -1) T1NOTE="unreadable: the response body did not parse" ;;
  *)  T1NOTE="the agent did make $TOOLS1 tool call(s) this turn" ;;
esac
echo "FANG9_TURN1_TOOL_CALLS=$TOOLS1  # $T1NOTE"
echo "FANG9_TURN1_MARKED=$MARK1       # want: yes — the named path was checked"
echo "FANG9_TURN1_NAMES_PATH=$NAMED1  # want: yes — the added line says which path"
echo "FANG9_TURN2_MARKED=$MARK2       # want: no  — the file is really there"
echo "FANG9_TURN3_MARKED=$MARK3       # want: no  — nothing could be checked"

# Marking a file that exists is the failure this fix was rebuilt to make
# impossible; it is graded before the miss, and it is graded as RED.
if [ "$MARK2" = "yes" ]; then
  echo
  echo "RESULT: RED — $EARLIER_PATH is on the volume and the runtime told the caller"
  echo "        otherwise. An existing file must never be marked."
  exit 0
fi
if [ "$MARK3" = "yes" ]; then
  echo
  echo "RESULT: RED — $OUTSIDE_PATH is outside the workspace, so the runtime cannot"
  echo "        know whether it exists, and it printed a verdict anyway."
  exit 0
fi
if [ "$S1" = "200" ] && [ "${TOOLS1:-0}" = "0" ] && [ "$MARK1" = "no" ]; then
  echo
  echo "RESULT: RED — the agent said it wrote $CLAIMED_PATH, wrote nothing, called"
  echo "        nothing, and the API reported the claim as a successful turn with"
  echo "        no one having looked at the disk."
  exit 0
fi
if [ "$MARK1" = "yes" ] && [ "$NAMED1" = "no" ]; then
  echo
  echo "RESULT: RED — the turn was marked but the answer does not name the path that"
  echo "        was checked, so the caller cannot tell what the verdict is about."
  exit 0
fi
echo
echo "RESULT: GREEN — the claim about $CLAIMED_PATH came back checked and named, the"
echo "        answer about $EARLIER_PATH came back untouched, and the answer about"
echo "        $OUTSIDE_PATH came back untouched because nothing could be checked."
exit 1
