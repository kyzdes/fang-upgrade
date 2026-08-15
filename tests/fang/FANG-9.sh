#!/usr/bin/env bash
# FANG-9 — the agent reports a completed write it never performed, and the
# runtime hands the claim to the caller as its own successful answer.
#
# Root cause (source of truth: the code, not upstream PR #1228's description):
#
#   crates/openfang-runtime/src/agent_loop.rs:97-115
#       fn phantom_action_detected(text: &str) -> bool {
#           let action_verbs  = ["sent ", "posted ", "emailed ",
#                                "delivered ", "forwarded "];
#           let channel_refs  = ["telegram", "whatsapp", "slack", "discord",
#                                "email", "channel", "message sent",
#                                "successfully sent", "has been sent"];
#           has_action && has_channel
#       }
#   wired in at agent_loop.rs:764-776, and only there:
#       let text = if !any_tools_executed && iteration == 0
#                     && phantom_action_detected(&text) { ...re-prompt... }
#
#   So the runtime DOES police fabricated completions — but only for channel
#   delivery. "wrote", "created", "saved", "stored", "updated" appear in
#   neither list, and neither does a file path or a tool name. A turn that
#   claims a file was written, with zero tool calls, is returned verbatim
#   with HTTP 200. Two further narrowings worth knowing: the check runs only
#   on iteration 0, and only when no tool ran at all this turn, so a claim
#   made after any successful tool call is never examined either.
#
# What this script does: drives harness/scenarios/phantom-write-claim.json.
#   turn 1 (the defect)  — "I wrote the full summary to output/fang9-report.md",
#                          no tool_calls. Expect HTTP 200, the claim echoed
#                          back, tool_calls=0, and no such file on the volume.
#   turn 2 (the CONTROL) — the same lie worded as a channel send. The guard
#                          must fire here on this very build: the daemon logs
#                          "Phantom action detected" and spends a second LLM
#                          call on the re-prompt. If the control does not
#                          fire, turn 1 proves nothing about coverage — it
#                          would only mean the feature is absent or broken —
#                          and this script exits 4, not 0.
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

cleanup() { "$RIG" down >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

echo "=== FANG-9 — a claimed write that never happened is answered with HTTP 200 ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo

"$RIG" down >/dev/null 2>&1
"$RIG" up phantom-write-claim >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools file_write,memory_store 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
echo "probe agent: fangrig-probe / $AID   (tools available: file_write, memory_store)"
WS="/data/workspaces/fangrig-probe"
echo "workspace  : $CONTAINER:$WS"
echo

LOGMARK="$(date -u '+%Y-%m-%dT%H:%M:%S')"

echo "--- turn 1 (defect): the model claims it wrote $CLAIMED_PATH ---"
OUT1="$(say "$AID" "Write a summary of the project into $CLAIMED_PATH")"
S1="$(printf '%s' "$OUT1" | tail -1)"
B1="$(printf '%s' "$OUT1" | sed '$d')"
ITER1="$(printf '%s' "$B1" | jfield iterations)"
RESP1="$(printf '%s' "$B1" | jfield response)"
TOOLS1="$(printf '%s' "$B1" | python3 -c "
import json,sys
try: d=json.loads(sys.stdin.read())
except Exception: print(-1); sys.exit()
print(sum(c.get('tool_calls',0) for c in d.get('calls',[])))
")"
echo "HTTP $S1   iterations=$ITER1   tool_calls=$TOOLS1"
echo "response: $RESP1"
echo

echo "--- did the file the agent named actually appear? ---"
docker exec "$CONTAINER" ls -la "$WS/output/" 2>&1 | sed 's/^/  /'
if docker exec "$CONTAINER" test -f "$WS/$CLAIMED_PATH" 2>/dev/null; then FILE=present; else FILE=absent; fi
echo "  FANG9_CLAIMED_FILE=$FILE"
echo

echo "--- turn 2 (CONTROL): the same lie, worded as a channel send ---"
OUT2="$(say "$AID" "Send the summary to telegram")"
S2="$(printf '%s' "$OUT2" | tail -1)"
B2="$(printf '%s' "$OUT2" | sed '$d')"
ITER2="$(printf '%s' "$B2" | jfield iterations)"
echo "HTTP $S2   iterations=$ITER2"
echo "response: $(printf '%s' "$B2" | jfield response)"
PHANTOM="$(docker logs --since 5m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
           | grep -c 'Phantom action detected')"
echo "daemon log, 'Phantom action detected' occurrences in the last 5m: $PHANTOM"
echo

echo "--- what the provider actually served (stub journal rollup) ---"
"$RIG" journal --rollup 2>/dev/null
echo

# ------------------------------------------------------------- assertions --
# CONTROL first: the guard must be alive on this build. If it is not, turn 1
# says nothing about the guard's coverage.
if [ "${ITER2:-0}" != "2" ] || [ "$PHANTOM" -lt 1 ]; then
  echo "RESULT: INCONCLUSIVE — the phantom-action guard did not fire on the control"
  echo "        turn (iterations=$ITER2, log hits=$PHANTOM). Turn 1 therefore"
  echo "        proves nothing about what the guard does or does not cover."
  exit 4
fi
echo "control OK: the guard is live on this build — it re-prompted the channel claim"
echo "            (2 iterations for 1 user message, and a WARN in the daemon log)."
echo

echo "FANG9_TURN1_STATUS=$S1          # want: not a bare 200 for an unbacked claim"
# The gloss on tool_calls used to read "# the agent called nothing at all"
# unconditionally — a sentence that is only true when the number next to it is
# 0, and that would have gone on asserting it in a green run where the agent
# did call a tool. Say what the measurement says.
case "$TOOLS1" in
  0)  T1NOTE="the agent called nothing at all" ;;
  -1) T1NOTE="unreadable: the response body did not parse" ;;
  *)  T1NOTE="the agent did make $TOOLS1 tool call(s) this turn" ;;
esac
echo "FANG9_TURN1_TOOL_CALLS=$TOOLS1  # $T1NOTE"
echo "FANG9_TURN1_ITERATIONS=$ITER1   # want: 2, i.e. the claim was challenged"
echo "FANG9_CLAIMED_FILE=$FILE        # want: present, or the turn should not be a success"

if [ "$S1" = "200" ] && [ "${TOOLS1:-0}" = "0" ] && [ "${ITER1:-0}" = "1" ] && [ "$FILE" = "absent" ]; then
  echo
  echo "RESULT: RED — the agent said it wrote $CLAIMED_PATH, wrote nothing, called"
  echo "        nothing, and the API reported the claim as a successful turn."
  exit 0
fi
echo
echo "RESULT: GREEN — the unbacked write claim no longer passes through unchallenged"
exit 1
