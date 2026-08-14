#!/usr/bin/env bash
# FANG-13 — a structurally valid provider response that carries no content at
# all is reported to the caller as a successful turn.
#
# Root cause (source of truth: the code, not the ticket text):
#
#   1. crates/openfang-runtime/src/drivers/openai.rs:779
#          if let Some(text) = choice.message.content {
#              if !text.is_empty() { ... push ContentBlock::Text ... }
#          }
#      An empty-string content is silently dropped. Note what this is NOT:
#      an empty `choices` ARRAY is caught 26 lines earlier (openai.rs:753,
#      `LlmError::Parse("No choices in response")`) and becomes a hard
#      error. That is the shape tests/fang/harness/scenarios/no-choices.json
#      already covers, and it is a different defect — an error, not a
#      success. FANG-13 is the shape where the response parses, has a
#      choice, has finish_reason=stop, and is simply empty.
#
#   2. crates/openfang-runtime/src/agent_loop.rs:706-729 — one-shot retry:
#      empty text + no tool calls + no content, on iteration 0 (or when
#      usage is 0/0), re-prompts once. It does not surface anything.
#
#   3. crates/openfang-runtime/src/agent_loop.rs:732-756 — the guard that
#      turns nothing into something:
#          if any_tools_executed {
#              "[Task completed — the agent executed tools but did not
#                produce a text summary.]"
#          } else {
#              "[The model returned an empty response. ...]"
#          }
#      and the turn returns Ok(AgentLoopResult) from there. HTTP 200, no
#      error field, tokens billed, and in the first branch the runtime
#      states that the task COMPLETED on a turn where the provider said
#      nothing at all.
#
# What this script does: drives harness/scenarios/empty-content-success.json
# through the staging daemon twice — turn 1 exercises the any_tools_executed
# branch, turn 2 the plain branch — and reports the HTTP status and body of
# each.
#
# Exit codes (shared by the four sprint-4 scenario repros):
#   0  RED           — defect reproduced (the expected state today)
#   1  GREEN         — not reproduced (fixed, or the repro has rotted)
#   3  SKIPPED       — prerequisites missing (no staging, no docker, ...)
#   4  INCONCLUSIVE  — the scenario's own control failed; proves nothing
#   2  REFUSED       — pointed at production
#
# STAGING ONLY. Usage: ./FANG-13.sh [base_url]

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

# ------------------------------------------------------------- prod guard --
case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. Staging is :4201." >&2; exit 2 ;;
esac
[ "$CONTAINER" = "openfang-openfang-1" ] && { echo "REFUSING: $CONTAINER is production." >&2; exit 2; }

# ----------------------------------------------------------- prerequisites --
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

# say AGENT_ID TEXT -> prints "<http_status>\n<body>"
say() {
  local id="$1" text="$2" body
  curl -sS -m 120 -X POST -H 'Content-Type: application/json' "${AUTH[@]}" -d '{}' \
       "$BASE_URL/api/agents/$id/session/reset" >/dev/null
  body="$(python3 -c "import json,sys;print(json.dumps({'message': sys.argv[1]}))" "$text")"
  curl -sS -m 300 -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
       "${AUTH[@]}" -d "$body" "$BASE_URL/api/agents/$id/message"
}

cleanup() { "$RIG" down >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

echo "=== FANG-13 — empty provider content returns HTTP 200 ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"
echo

"$RIG" down >/dev/null 2>&1
"$RIG" up empty-content-success >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools memory_store 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
echo "probe agent: fangrig-probe / $AID"
echo

echo "--- turn 1: provider answers a tool_call, then an EMPTY text ---"
OUT1="$(say "$AID" "Store the value and then summarise.")"
S1="$(printf '%s' "$OUT1" | tail -1)"
B1="$(printf '%s' "$OUT1" | sed '$d')"
echo "HTTP $S1"
echo "$B1"
echo

echo "--- turn 2: provider answers EMPTY text twice, no tools ---"
OUT2="$(say "$AID" "Just answer in words.")"
S2="$(printf '%s' "$OUT2" | tail -1)"
B2="$(printf '%s' "$OUT2" | sed '$d')"
echo "HTTP $S2"
echo "$B2"
echo

echo "--- what the provider actually served (stub journal rollup) ---"
"$RIG" journal --rollup 2>/dev/null
echo

# ------------------------------------------------------------- assertions --
# The stub served nothing but a tool_call and empty strings; the journal is
# the control that says so. If it does not, the rig is at fault and the run
# proves nothing.
ROLL="$("$RIG" journal --rollup 2>/dev/null)"
SERVED="$(printf '%s' "$ROLL" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(0); sys.exit()
print(d.get('counts',{}).get('primary',0))
")"
NON200="$(printf '%s' "$ROLL" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(-1); sys.exit()
print(sum(v for k,v in d.get('statuses',{}).items() if k!='200'))
")"
if [ "$SERVED" != "4" ] || [ "$NON200" != "0" ]; then
  echo "RESULT: INCONCLUSIVE — the stub did not serve the scripted 4x HTTP 200"
  echo "        (served=$SERVED non-200=$NON200). Nothing is proven about the product."
  exit 4
fi

has() { printf '%s' "$1" | grep -qF "$2"; }

RED=0
if [ "$S1" = "200" ] && has "$B1" "Task completed"; then
  echo "RED  (turn 1): HTTP 200 and the runtime asserts '[Task completed ...]'"
  echo "              on a turn whose final provider response had no content."
  RED=1
else
  echo "GREEN (turn 1): HTTP $S1, body does not claim completion."
fi
if [ "$S2" = "200" ] && has "$B2" "The model returned an empty response"; then
  echo "RED  (turn 2): HTTP 200 with a runtime-authored sentence standing in"
  echo "              for an answer the provider never gave."
  RED=1
else
  echo "GREEN (turn 2): HTTP $S2, empty provider content is not a 200."
fi
echo
echo "FANG13_TURN1_STATUS=$S1   # want: not 2xx, or a body that flags the failure"
echo "FANG13_TURN2_STATUS=$S2   # want: not 2xx, or a body that flags the failure"

if [ "$RED" = 1 ]; then
  echo "RESULT: RED — an empty provider response is indistinguishable from a real answer"
  exit 0
fi
echo "RESULT: GREEN — empty provider content no longer passes as a successful turn"
exit 1
