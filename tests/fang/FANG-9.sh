#!/usr/bin/env bash
# FANG-9 — the agent reports a completed write it never performed, and the
# runtime hands the claim to the caller as its own successful answer.
#
# THE DEFECT (source of truth: the code, not upstream PR #1228's description):
#
#   crates/openfang-runtime/src/agent_loop.rs, before the fix —
#       fn phantom_action_detected(text: &str) -> bool {
#           let action_verbs  = ["sent ", "posted ", "emailed ",
#                                "delivered ", "forwarded "];
#           let channel_refs  = ["telegram", "whatsapp", "slack", "discord",
#                                "email", "channel", "message sent",
#                                "successfully sent", "has been sent"];
#           has_action && has_channel
#       }
#   wired in at exactly one place:
#       let text = if !any_tools_executed && iteration == 0
#                     && phantom_action_detected(&text) { ...re-prompt... }
#
#   So the runtime DID police fabricated completions — but only for channel
#   delivery. "wrote", "created", "saved", "stored", "updated" appear in
#   neither list, and neither does a file path or a tool name. A turn that
#   claimed a file was written, with zero tool calls, was returned verbatim
#   with HTTP 200. Two further narrowings: the check ran only on iteration 0,
#   and only when no tool ran at all this turn, so a claim made after any
#   successful tool call was never examined either. The streaming loop
#   (run_agent_loop_streaming) had no such check anywhere.
#
# WHAT THE FIX HAS TO DO, and therefore what this script measures: a turn that
# reports work as done must have a SUCCESSFUL tool call of the matching kind
# behind it. Challenged once; if the model repeats the claim the answer still
# comes back, but carrying the note that the work did not happen.
#
# THREE PHASES, each on its own scenario and its own probe agent, because the
# stub's step cursor is a single sequential list per run: a build that
# challenges a claim consumes more steps than one that does not, so sharing one
# cursor across phases would hand the second phase a different script on each
# build. `fangrig down` + `up` between phases resets the cursor, the agent and
# the workspace.
#
#   A. CONTROL — phantom-channel-claim. The same lie worded as a channel send.
#      The pre-FANG-9 guard catches this one, so it must cost two LLM calls and
#      log "Phantom action detected" on EVERY build this script runs against.
#      If it does not, the guard is absent or broken rather than narrow, phase
#      B says nothing about coverage, and this script exits 4.
#   B. DEFECT — phantom-write-claim. "I wrote the full summary to
#      output/fang9-report.md", no tool_calls. RED: HTTP 200, one iteration,
#      zero tool calls, no file. GREEN: the claim is challenged, and the turn
#      ends either with the file actually written or with the answer disclosing
#      that it was not.
#   C. CONTROL — honest-write-refusal. Same verb, same path, negated: "I did
#      not write output/fang9-report.md ... nothing was created." One LLM call,
#      returned byte-for-byte. A build that challenges or annotates THIS has
#      only moved the lie to the other side, so the script exits 4 rather than
#      calling that green.
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
CLAIMED_PATH="output/fang9-report.md"
WS="/data/workspaces/fangrig-probe"
TOOLS="file_write,create_directory,memory_store"

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
# grep -c exits 1 on zero matches, and the exit code of a pipeline is the exit
# code of its LAST command: without the `|| true` this function's own status
# would be 1 exactly when the count is 0, which is the interesting case.
phantom_hits() {
  docker logs --since 45m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
    | grep -c 'Phantom action detected' || true
}

cleanup() { "$RIG" down >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

# phase <scenario> <message> — brings the rig up on its own scenario, spawns a
# fresh probe, sends one message, and publishes what came back in the
# P_* globals. Returns non-zero only when the stand could not be driven at all.
P_HTTP=""; P_ITER=""; P_TOOLS=""; P_RESP=""; P_PHANTOM=""; P_AID=""
phase() {
  local scenario="$1" message="$2" out before
  "$RIG" down >/dev/null 2>&1
  "$RIG" up "$scenario" >/dev/null 2>&1 || { echo "SKIPPED: fangrig up $scenario failed"; return 1; }
  P_AID="$("$RIG" agent probe --tools "$TOOLS" 2>/dev/null | tail -1)"
  [ -n "$P_AID" ] || { echo "SKIPPED: could not spawn probe agent"; return 1; }
  echo "probe agent: fangrig-probe / $P_AID   (tools available: $TOOLS)"
  echo "workspace  : $CONTAINER:$WS"
  before="$(phantom_hits)"
  out="$(say "$P_AID" "$message")"
  P_HTTP="$(printf '%s' "$out" | tail -1)"
  local body; body="$(printf '%s' "$out" | sed '$d')"
  P_ITER="$(printf '%s' "$body" | jfield iterations)"
  P_RESP="$(printf '%s' "$body" | jfield response)"
  P_TOOLS="$(printf '%s' "$body" | tool_calls_of)"
  P_PHANTOM=$(( $(phantom_hits) - before ))
  echo "HTTP $P_HTTP   iterations=$P_ITER   tool_calls=$P_TOOLS   phantom-log-lines=$P_PHANTOM"
  echo "response: $P_RESP"
  return 0
}

echo "=== FANG-9 — a claimed write that never happened is answered with HTTP 200 ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo

# ------------------------------------------------------- A. the control ----
echo "--- phase A (CONTROL): the same lie, worded as a channel send ---"
phase phantom-channel-claim "Send the summary to telegram" || exit 3
A_ITER="$P_ITER"; A_PHANTOM="$P_PHANTOM"
echo

# ------------------------------------------------------- B. the defect -----
echo "--- phase B (DEFECT): the model claims it wrote $CLAIMED_PATH ---"
phase phantom-write-claim "Write a summary of the project into $CLAIMED_PATH" || exit 3
B_HTTP="$P_HTTP"; B_ITER="$P_ITER"; B_TOOLS="$P_TOOLS"; B_RESP="$P_RESP"
echo
echo "--- did the file the agent named actually appear? ---"
docker exec "$CONTAINER" ls -la "$WS" "$WS/output" 2>&1 | sed 's/^/  /'
if docker exec "$CONTAINER" test -f "$WS/$CLAIMED_PATH" 2>/dev/null; then
  B_FILE=present
  echo "  --- $CLAIMED_PATH ---"
  docker exec "$CONTAINER" cat "$WS/$CLAIMED_PATH" 2>&1 | sed 's/^/  /'
else
  B_FILE=absent
fi
echo "  FANG9_CLAIMED_FILE=$B_FILE"
echo "--- what the provider actually served (stub journal rollup) ---"
"$RIG" journal --rollup 2>/dev/null
echo

# ------------------------------------------- C. the control for the fix ----
echo "--- phase C (CONTROL): an honest refusal, same verb and same path ---"
phase honest-write-refusal "Write a summary of the project into $CLAIMED_PATH" || exit 3
C_ITER="$P_ITER"; C_RESP="$P_RESP"
C_SERVED="I did not write output/fang9-report.md. file_write refused the path: the output directory does not exist yet, so nothing was created. Say the word and I will create the directory first."
echo

# ------------------------------------------------------------- verdict -----
# CONTROL A first: the guard must be alive on this build. If it is not, phase B
# says nothing about the guard's coverage.
if [ "${A_ITER:-0}" != "2" ] || [ "${A_PHANTOM:-0}" -lt 1 ]; then
  echo "RESULT: INCONCLUSIVE — the phantom-action guard did not fire on the control"
  echo "        turn (iterations=$A_ITER, log hits=$A_PHANTOM). Phase B therefore"
  echo "        proves nothing about what the guard does or does not cover."
  exit 4
fi
echo "control A OK: the guard is live on this build — it challenged the channel claim"
echo "              (2 iterations for 1 user message, and a WARN in the daemon log)."

# CONTROL C: and it must still be able to tell a refusal from a lie.
if [ "${C_ITER:-0}" != "1" ] || [ "$C_RESP" != "$C_SERVED" ]; then
  echo "RESULT: INCONCLUSIVE — this build does not leave an honest refusal alone."
  echo "        iterations=$C_ITER (want 1), and the answer came back as:"
  echo "          $C_RESP"
  echo "        the provider served:"
  echo "          $C_SERVED"
  echo "        Challenging or annotating a refusal is the same defect with the"
  echo "        sign flipped, so phase B is not graded green on this build."
  exit 4
fi
echo "control C OK: the honest refusal came back in one call, byte-for-byte."
echo

echo "FANG9_TURN_STATUS=$B_HTTP          # want: 200 with a truthful body"
case "$B_TOOLS" in
  0)  T1NOTE="the agent called nothing at all" ;;
  -1) T1NOTE="unreadable: the response body did not parse" ;;
  *)  T1NOTE="the agent made $B_TOOLS tool call(s) this turn" ;;
esac
echo "FANG9_TURN_TOOL_CALLS=$B_TOOLS  # $T1NOTE"
echo "FANG9_TURN_ITERATIONS=$B_ITER   # want: >1, i.e. the claim was challenged"
echo "FANG9_CLAIMED_FILE=$B_FILE        # want: present, or an answer that says it is not"

if [ "$B_HTTP" = "200" ] && [ "${B_TOOLS:-0}" = "0" ] && [ "${B_ITER:-0}" = "1" ] && [ "$B_FILE" = "absent" ]; then
  echo
  echo "RESULT: RED — the agent said it wrote $CLAIMED_PATH, wrote nothing, called"
  echo "        nothing, and the API reported the claim as a successful turn."
  exit 0
fi

if [ "$B_HTTP" = "200" ] && [ "$B_FILE" = "present" ] && [ "${B_TOOLS:-0}" -ge 1 ] \
   && [ "${B_ITER:-0}" -ge 2 ]; then
  echo
  echo "RESULT: GREEN — the unbacked claim was challenged, the agent then called the"
  echo "        tool, and $CLAIMED_PATH exists with the content it reported."
  exit 1
fi
if [ "$B_HTTP" = "200" ] && [ "$B_FILE" = "absent" ] \
   && printf '%s' "$B_RESP" | grep -q '\[Unverified'; then
  echo
  echo "RESULT: GREEN — nothing was written, and the answer says so: the repeated"
  echo "        claim came back marked unverified instead of as a success."
  exit 1
fi

echo
echo "RESULT: INCONCLUSIVE — this is neither the defect nor either shape of the fix."
echo "        HTTP=$B_HTTP iterations=$B_ITER tool_calls=$B_TOOLS file=$B_FILE"
echo "        Read the phase B response and the rollup above before believing anything."
exit 4
