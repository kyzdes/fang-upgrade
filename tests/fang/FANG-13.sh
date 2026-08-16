#!/usr/bin/env bash
# FANG-13 — a structurally valid provider response that carries no content at
# all is handed to the caller as a completed turn, with a sentence the runtime
# wrote itself standing in for the answer.
#
# Root cause (source of truth: the code, not the ticket text):
#
#   1. crates/openfang-runtime/src/drivers/openai.rs
#          if let Some(text) = choice.message.content {
#              if !text.is_empty() { ... push ContentBlock::Text ... }
#          }
#      An empty-string content is silently dropped. Note what this is NOT:
#      an empty `choices` ARRAY is caught earlier in the same function
#      (`LlmError::Parse("No choices in response")`) and becomes a hard
#      error. That is the shape harness/scenarios/no-choices.json already
#      covers, and it is a different defect — an error, not a success.
#      FANG-13 is the shape where the response parses, has a choice, has
#      finish_reason=stop, and is simply empty.
#
#   2. agent_loop.rs, one-shot retry: empty text + no tool calls + no content,
#      on iteration 0 (or when usage is 0/0), re-prompts once. It does not
#      surface anything. Hence two provider calls per empty turn.
#
#   3. agent_loop.rs, the two exits that turn nothing into something:
#        a. the empty-final-message guard —
#              if any_tools_executed {
#                  "[Task completed — the agent executed tools but did not
#                    produce a text summary.]"
#              } else {
#                  "[The model returned an empty response. ...]"
#              }
#        b. the StopReason::MaxTokens arm, once the continuation budget
#           (MAX_CONTINUATIONS = 5) is spent —
#              "[Partial response — token limit reached with no text output.]"
#      Both then return Ok(AgentLoopResult): HTTP 200, no error field, and in
#      the first branch of (a) the runtime states that the task COMPLETED on a
#      turn where the provider said nothing at all.
#
# WHAT THIS SCRIPT DECIDES ON, AND WHY IT DOES NOT GREP FOR THOSE SENTENCES
#
#   The three strings above are what the defect looked like on the day it was
#   found, not what it is. A guard that greps for them goes green the moment
#   someone rewords the fabrication, which is the one change that fixes
#   nothing. So each surface below is classified by BEHAVIOUR, blind to
#   wording, into exactly one of:
#
#     ANSWER-INVENTED   the surface handed back text as the agent's answer.
#                       The provider sent the empty string, so every character
#                       of it was written by the runtime. This is the defect.
#     FAILURE-REPORTED  the surface told the caller the turn failed (non-2xx,
#                       or a WebSocket `error` event).
#     SUCCESS-CLAIMED   the surface signalled ordinary completion without
#                       inventing text (finish_reason=stop, a `done` event,
#                       `silent_complete`). Not the defect this ticket names,
#                       and not a report of the failure either.
#
#   RED  — any surface is ANSWER-INVENTED.
#   GREEN — no surface invents an answer, AND every surface that has somewhere
#           to put a verdict reports the failure: all three REST turns (two in
#           phase A, one in phase B), /v1 non-streaming, and the WebSocket.
#   Surfaces left at SUCCESS-CLAIMED are printed as named gaps. They are the
#   two SSE ones, they were never fixed, and the run says so out loud rather
#   than letting a green line imply otherwise.
#
# MAKING SURE THE IMAGE UNDER TEST IS THE IMAGE YOU BUILT
#
#   The review of the first FANG-13 patch reports two rebuilds that produced an
#   image without the patch while saying nothing about it: the build succeeded,
#   the container came back, and the probe reported the defect still present.
#   `docker build` finishing is not evidence; a tag pointing somewhere is not
#   evidence; a container that was restarted rather than recreated is still
#   running the image it was created from. The binary is the evidence.
#
#   So every run of this script greps the openfang executable
#   INSIDE THE CONTAINER IT IS ABOUT TO DRIVE for three marker strings and
#   prints how many lines of it match each — zero means absent, anything else
#   means present (a binary has few newlines, so the count itself carries no
#   meaning beyond that):
#
#     "The turn produced no text:"        present only WITH this patch
#     "[Task completed — the agent ..."   present only WITHOUT it
#     "[Partial response — token ..."     present only WITHOUT its second half
#
#   Reproduce by hand with:
#     docker exec <container> grep -ac 'The turn produced no text:' \
#         /usr/local/bin/openfang
#   An image built from a stale context, or a container still running the
#   previous image because it was restarted rather than recreated, answers 0
#   there. The check is a diagnostic, not a gate: this script must be able to
#   run RED against an unpatched build.
#
#   ONE MECHANISM, CAUGHT IN THE ACT while writing this. The Dockerfile mounts
#   a BuildKit cache at /build/target, shared by every build on this box, and
#   cargo decides a crate is fresh by comparing mtimes. Build worktree A, then
#   build worktree B whose files were checked out EARLIER: cargo finds
#   artefacts newer than B's sources, rebuilds nothing — "Finished `release`
#   profile in 1.90s" — and the image tagged B contains A's binary. Measured:
#   an image built from `ours` immediately after one built from this branch had
#   the same sha256 for /usr/local/bin/openfang as the branch build, and carried
#   the patch marker. Nothing in that build's output says so; the marker check
#   is what says so. Build from an export with fresh mtimes to get a real one:
#
#     git archive <ref> | tar -x -C /tmp/src && find /tmp/src -exec touch {} +
#     docker build -t <tag> /tmp/src
#
#   Note what this is NOT: a docker layer-cache hit. In the run above the cargo
#   step was not reported CACHED — it executed, and found nothing to do.
#
# Exit codes (shared by the four sprint-4 scenario repros):
#   0  RED           — defect reproduced
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

WORK="$(mktemp -d /tmp/fang13.XXXXXX)"
cleanup() { "$RIG" down >/dev/null 2>&1; rm -rf "$WORK"; }
trap cleanup EXIT INT TERM

HOSTPORT="${BASE_URL#http://}"; HOSTPORT="${HOSTPORT#https://}"; HOSTPORT="${HOSTPORT%%/*}"
WS_HOST="${HOSTPORT%%:*}"; WS_PORT="${HOSTPORT##*:}"
[ "$WS_PORT" = "$WS_HOST" ] && WS_PORT=80

echo "=== FANG-13 — an empty provider response, across every caller-facing surface ==="
echo "target: $BASE_URL   container: $CONTAINER"
echo "image : $(docker inspect -f '{{.Config.Image}}' "$CONTAINER")"

# ------------------------------------------------- which binary is running --
# See "MAKING SURE THE IMAGE UNDER TEST IS THE IMAGE YOU BUILT" above.
BIN="$(docker exec "$CONTAINER" sh -c 'command -v openfang || echo /usr/local/bin/openfang' 2>/dev/null | tr -d '\r')"
mark() {
  local n
  n="$(docker exec "$CONTAINER" grep -ac "$1" "$BIN" 2>/dev/null | tr -d '\r')"
  case "$n" in ''|*[!0-9]*) n=0 ;; esac
  printf '  %-45s matching lines: %-4s %s\n' "\"$1\"" "$n" "$2"
}
echo "binary: $BIN   (0 matching lines = the string is not in this build)"
mark 'The turn produced no text:'                 'present only WITH the FANG-13 patch'
mark 'Task completed — the agent executed tools'  'present only WITHOUT it'
mark 'Partial response — token limit reached'     'present only WITHOUT its second half'
echo

# ============================================================== phase A ====
# Scenario: every response is finish_reason=stop with an empty content string.
"$RIG" down >/dev/null 2>&1
"$RIG" up empty-content-success >/dev/null 2>&1 || { echo "SKIPPED: fangrig up failed"; exit 3; }
AID="$("$RIG" agent probe --tools memory_store 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent"; exit 3; }
echo "probe agent: fangrig-probe / $AID"
echo

reset_session() {
  curl -sS -m 60 -X POST -H 'Content-Type: application/json' "${AUTH[@]}" -d '{}' \
       "$BASE_URL/api/agents/$AID/session/reset" >/dev/null 2>&1
}
jbody() { python3 -c "import json,sys;print(json.dumps({'message': sys.argv[1]}))" "$1"; }

# rest_turn NAME TEXT — POST /api/agents/{id}/message
rest_turn() {
  reset_session
  curl -sS -m 300 -o "$WORK/$1.body" -w '%{http_code}' -X POST \
       -H 'Content-Type: application/json' "${AUTH[@]}" -d "$(jbody "$2")" \
       "$BASE_URL/api/agents/$AID/message" > "$WORK/$1.status" 2>/dev/null
}

# v1_turn NAME TEXT [stream] — POST /v1/chat/completions
v1_turn() {
  reset_session
  local stream="${3:-false}" body
  body="$(python3 -c "
import json,sys
print(json.dumps({'model': sys.argv[1],
                  'messages': [{'role': 'user', 'content': sys.argv[2]}],
                  'stream': sys.argv[3] == 'true'}))" "$AID" "$2" "$stream")"
  curl -sSN -m 300 -o "$WORK/$1.body" -w '%{http_code}' -X POST \
       -H 'Content-Type: application/json' "${AUTH[@]}" -d "$body" \
       "$BASE_URL/v1/chat/completions" > "$WORK/$1.status" 2>/dev/null
}

# sse_turn NAME TEXT — POST /api/agents/{id}/message/stream
sse_turn() {
  reset_session
  curl -sSN -m 300 -o "$WORK/$1.body" -w '%{http_code}' -X POST \
       -H 'Content-Type: application/json' "${AUTH[@]}" -d "$(jbody "$2")" \
       "$BASE_URL/api/agents/$AID/message/stream" > "$WORK/$1.status" 2>/dev/null
}

# ws_turn NAME TEXT — GET /api/agents/{id}/ws, one message, collect events.
# The client below is stdlib-only: this box has neither `websockets` nor
# `websocket-client`, and the harness rule is no new dependencies.
ws_turn() {
  reset_session
  python3 "$WORK/ws_probe.py" "$WS_HOST" "$WS_PORT" "/api/agents/$AID/ws" \
          "$API_KEY" "$2" > "$WORK/$1.body" 2>"$WORK/$1.err"
  echo "$?" > "$WORK/$1.rc"
}

cat > "$WORK/ws_probe.py" <<'PY'
import base64, json, os, socket, sys

host, port, path, key, text = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]

sock = socket.create_connection((host, port), timeout=300)
nonce = base64.b64encode(os.urandom(16)).decode()
req = (
    f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
    f"Connection: Upgrade\r\nSec-WebSocket-Key: {nonce}\r\n"
    f"Sec-WebSocket-Version: 13\r\n"
)
if key:
    req += f"Authorization: Bearer {key}\r\n"
req += "\r\n"
sock.sendall(req.encode())

buf = bytearray()
while b"\r\n\r\n" not in buf:
    chunk = sock.recv(4096)
    if not chunk:
        print(json.dumps({"type": "__handshake_failed__", "content": "connection closed"}))
        sys.exit(1)
    buf += chunk
head, _, rest = bytes(buf).partition(b"\r\n\r\n")
buf = bytearray(rest)
status = head.split(b"\r\n", 1)[0].decode(errors="replace")
if "101" not in status:
    print(json.dumps({"type": "__handshake_failed__", "content": status}))
    sys.exit(1)


def want(n):
    while len(buf) < n:
        chunk = sock.recv(65536)
        if not chunk:
            raise EOFError
        buf.extend(chunk)
    out = bytes(buf[:n])
    del buf[:n]
    return out


def send(opcode, payload):
    hdr = bytearray([0x80 | opcode])
    n = len(payload)
    if n < 126:
        hdr.append(0x80 | n)
    elif n < 65536:
        hdr.append(0x80 | 126)
        hdr += n.to_bytes(2, "big")
    else:
        hdr.append(0x80 | 127)
        hdr += n.to_bytes(8, "big")
    mask = os.urandom(4)
    hdr += mask
    sock.sendall(bytes(hdr) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))


def read_frame():
    b0, b1 = want(2)
    opcode = b0 & 0x0F
    n = b1 & 0x7F
    if n == 126:
        n = int.from_bytes(want(2), "big")
    elif n == 127:
        n = int.from_bytes(want(8), "big")
    mask = want(4) if b1 & 0x80 else None
    data = want(n) if n else b""
    if mask:
        data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
    return b0 & 0x80, opcode, data


send(0x1, json.dumps({"type": "message", "content": text}).encode())

frag, fragop = bytearray(), None
try:
    while True:
        fin, opcode, data = read_frame()
        if opcode == 0x8:
            break
        if opcode == 0x9:
            send(0xA, data)
            continue
        if opcode == 0xA:
            continue
        if opcode == 0x0:
            frag += data
        else:
            frag, fragop = bytearray(data), opcode
        if not fin:
            continue
        payload, frag = bytes(frag), bytearray()
        if fragop != 0x1:
            continue
        print(payload.decode(errors="replace"), flush=True)
        try:
            ev = json.loads(payload)
        except Exception:
            continue
        # Terminal for one turn: the socket has said how the turn ended.
        if ev.get("type") in ("response", "silent_complete", "error"):
            break
except EOFError:
    pass
finally:
    try:
        sock.close()
    except Exception:
        pass
PY

echo "--- surface 1/6: REST POST /api/agents/{id}/message, tools ran first ---"
rest_turn rest_tools "Store the value and then summarise."
echo "HTTP $(cat "$WORK/rest_tools.status")"; cat "$WORK/rest_tools.body"; echo; echo

echo "--- surface 2/6: REST POST /api/agents/{id}/message, no tools ---"
rest_turn rest_plain "Just answer in words."
echo "HTTP $(cat "$WORK/rest_plain.status")"; cat "$WORK/rest_plain.body"; echo; echo

echo "--- surface 3/6: POST /v1/chat/completions (stream:false) ---"
v1_turn v1_plain "Just answer in words." false
echo "HTTP $(cat "$WORK/v1_plain.status")"; cat "$WORK/v1_plain.body"; echo; echo

echo "--- surface 4/6: WebSocket /api/agents/{id}/ws ---"
ws_turn ws_plain "Just answer in words."
cat "$WORK/ws_plain.body"; echo

echo "--- surface 5/6: SSE POST /api/agents/{id}/message/stream ---"
sse_turn sse_plain "Just answer in words."
echo "HTTP $(cat "$WORK/sse_plain.status")"; cat "$WORK/sse_plain.body"; echo

echo "--- surface 6/6: POST /v1/chat/completions (stream:true) ---"
v1_turn v1_stream "Just answer in words." true
echo "HTTP $(cat "$WORK/v1_stream.status")"; cat "$WORK/v1_stream.body"; echo

echo "--- what the provider actually served (stub journal rollup) ---"
ROLL_A="$("$RIG" journal --rollup 2>/dev/null)"
printf '%s\n' "$ROLL_A"
echo

# --------------------------------------------------------- phase A control --
# Twelve scripted calls, two per surface, every one HTTP 200 and none of them
# falling through to `default_step`. If that did not happen the stand was not
# driven the way this script believes it was, and nothing below is evidence.
read -r SERVED_A NON200_A <<EOF
$(printf '%s' "$ROLL_A" | python3 -c "
import json,sys
try: d = json.load(sys.stdin)
except Exception: print(-1, -1); raise SystemExit
print(d.get('counts', {}).get('primary', 0),
      sum(v for k, v in d.get('statuses', {}).items() if k != '200'))
")
EOF
EXHAUSTED=0
grep -qs 'fangrig: steps exhausted' "$WORK"/*.body && EXHAUSTED=1
if [ "$SERVED_A" != "12" ] || [ "$NON200_A" != "0" ] || [ "$EXHAUSTED" != "0" ]; then
  echo "RESULT: INCONCLUSIVE — phase A: the stub did not serve the scripted 12x HTTP 200"
  echo "        (served=$SERVED_A non-200=$NON200_A default_step_seen=$EXHAUSTED)."
  echo "        Nothing is proven about the product."
  exit 4
fi

# ============================================================== phase B ====
# The other exit: finish_reason=length, empty content, MAX_CONTINUATIONS times.
echo "=== phase B: the continuation budget runs out without a character of text ==="
"$RIG" down >/dev/null 2>&1
"$RIG" up empty-truncated-length >/dev/null 2>&1 || { echo "SKIPPED: fangrig up (phase B) failed"; exit 3; }
AID="$("$RIG" agent probe 2>/dev/null | tail -1)"
[ -n "$AID" ] || { echo "SKIPPED: could not spawn probe agent for phase B"; exit 3; }
echo "probe agent: fangrig-probe / $AID"
echo
echo "--- REST POST /api/agents/{id}/message, 5x finish_reason=length ---"
rest_turn trunc "Write something long."
echo "HTTP $(cat "$WORK/trunc.status")"; cat "$WORK/trunc.body"; echo; echo

ROLL_B="$("$RIG" journal --rollup 2>/dev/null)"
printf '%s\n' "$ROLL_B"
echo
read -r SERVED_B NON200_B <<EOF
$(printf '%s' "$ROLL_B" | python3 -c "
import json,sys
try: d = json.load(sys.stdin)
except Exception: print(-1, -1); raise SystemExit
print(d.get('counts', {}).get('primary', 0),
      sum(v for k, v in d.get('statuses', {}).items() if k != '200'))
")
EOF
if [ "$SERVED_B" != "5" ] || [ "$NON200_B" != "0" ]; then
  echo "RESULT: INCONCLUSIVE — phase B: the stub did not serve the scripted 5x HTTP 200"
  echo "        (served=$SERVED_B non-200=$NON200_B). Nothing is proven about the product."
  exit 4
fi

# ------------------------------------------------------------- classifier --
# One place, six surfaces, no sentence literals. Reads what each surface
# returned and prints "<id>|<class>|<detail>" per line.
python3 - "$WORK" <<'PY' > "$WORK/verdicts"
import json, os, sys

W = sys.argv[1]


def read(name, ext):
    p = os.path.join(W, name + "." + ext)
    try:
        with open(p, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def status(name):
    s = read(name, "status").strip()
    return int(s) if s.isdigit() else 0


def sse_events(raw):
    """Yield the `data:` payloads of an SSE body, in order."""
    for block in raw.split("\n\n"):
        data = "".join(
            line[5:].strip() for line in block.splitlines() if line.startswith("data:")
        )
        if data:
            yield data


def http_surface(name, answer_of):
    """REST-shaped: 2xx + non-empty answer == invented; non-2xx == reported."""
    code = status(name)
    if code and not (200 <= code < 300):
        return "FAILURE-REPORTED", "HTTP %d" % code
    try:
        body = json.loads(read(name, "body"))
    except Exception:
        return "SUCCESS-CLAIMED", "HTTP %d, unparseable body" % code
    answer = answer_of(body) or ""
    if answer.strip():
        return "ANSWER-INVENTED", "HTTP %d, %d chars of answer the provider never sent: %r" % (
            code, len(answer), answer[:160])
    return "SUCCESS-CLAIMED", "HTTP %d, empty answer, no error field" % code


out = []

for name, label in (("rest_tools", "REST /api/agents/{id}/message (tools ran)"),
                    ("rest_plain", "REST /api/agents/{id}/message (no tools)"),
                    ("trunc", "REST /api/agents/{id}/message (finish_reason=length x5)")):
    cls, detail = http_surface(name, lambda b: b.get("response", ""))
    out.append((name, label, cls, detail))

cls, detail = http_surface("v1_plain",
                           lambda b: (b.get("choices") or [{}])[0]
                           .get("message", {}).get("content") or "")
out.append(("v1_plain", "/v1/chat/completions (stream:false)", cls, detail))

# WebSocket: the events themselves are the verdict.
ws_types, ws_answer, ws_err = [], "", ""
for line in read("ws_plain", "body").splitlines():
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    t = ev.get("type", "?")
    ws_types.append(t)
    if t == "response":
        ws_answer = ev.get("content") or ""
    if t == "error":
        ws_err = ev.get("content") or ""
seen = " ".join(ws_types) or "(no events)"
if ws_answer.strip():
    out.append(("ws_plain", "WS /api/agents/{id}/ws", "ANSWER-INVENTED",
                "events [%s]; %d chars of answer: %r" % (seen, len(ws_answer), ws_answer[:160])))
elif ws_err:
    out.append(("ws_plain", "WS /api/agents/{id}/ws", "FAILURE-REPORTED",
                "events [%s]; error: %r" % (seen, ws_err[:200])))
elif "__handshake_failed__" in ws_types:
    out.append(("ws_plain", "WS /api/agents/{id}/ws", "NO-CONTACT", "events [%s]" % seen))
else:
    out.append(("ws_plain", "WS /api/agents/{id}/ws", "SUCCESS-CLAIMED", "events [%s]" % seen))

# SSE /api/agents/{id}/message/stream: chunk deltas are the answer.
raw = read("sse_plain", "body")
text = ""
kinds = []
for line in raw.splitlines():
    if line.startswith("event:"):
        kinds.append(line[6:].strip())
for data in sse_events(raw):
    try:
        ev = json.loads(data)
    except Exception:
        continue
    if isinstance(ev, dict) and isinstance(ev.get("content"), str):
        text += ev["content"]
seen = " ".join(kinds) or "(no events)"
if text.strip():
    out.append(("sse_plain", "SSE /api/agents/{id}/message/stream", "ANSWER-INVENTED",
                "events [%s]; %d chars: %r" % (seen, len(text), text[:160])))
elif status("sse_plain") and not (200 <= status("sse_plain") < 300):
    out.append(("sse_plain", "SSE /api/agents/{id}/message/stream", "FAILURE-REPORTED",
                "HTTP %d" % status("sse_plain")))
else:
    out.append(("sse_plain", "SSE /api/agents/{id}/message/stream", "SUCCESS-CLAIMED",
                "HTTP %d, events [%s], no error event" % (status("sse_plain"), seen)))

# /v1 with stream:true: chunk deltas plus the terminal finish_reason.
raw = read("v1_stream", "body")
text, finishes, done = "", [], False
for data in sse_events(raw):
    if data == "[DONE]":
        done = True
        continue
    try:
        ev = json.loads(data)
    except Exception:
        continue
    for ch in ev.get("choices", []) or []:
        text += (ch.get("delta", {}) or {}).get("content") or ""
        if ch.get("finish_reason"):
            finishes.append(ch["finish_reason"])
tail = "finish_reason=%s%s" % (",".join(finishes) or "none", " [DONE]" if done else "")
if text.strip():
    out.append(("v1_stream", "/v1/chat/completions (stream:true)", "ANSWER-INVENTED",
                "%s; %d chars: %r" % (tail, len(text), text[:160])))
elif status("v1_stream") and not (200 <= status("v1_stream") < 300):
    out.append(("v1_stream", "/v1/chat/completions (stream:true)", "FAILURE-REPORTED",
                "HTTP %d" % status("v1_stream")))
else:
    out.append(("v1_stream", "/v1/chat/completions (stream:true)", "SUCCESS-CLAIMED",
                "HTTP %d, %s" % (status("v1_stream"), tail)))

for name, label, cls, detail in out:
    print("%s|%s|%s|%s" % (name, label, cls, detail))
PY

echo "=== how each surface reported a turn the provider never answered ==="
printf '%-52s %-17s %s\n' SURFACE CLASS DETAIL
while IFS='|' read -r _name label cls detail; do
  printf '%-52s %-17s %s\n' "$label" "$cls" "$detail"
done < "$WORK/verdicts"
echo

INVENTED="$(grep -c '|ANSWER-INVENTED|' "$WORK/verdicts")"
NOCONTACT="$(grep -c '|NO-CONTACT|' "$WORK/verdicts")"
verdict_of() { grep "^$1|" "$WORK/verdicts" | cut -d'|' -f3; }

if [ "$NOCONTACT" != "0" ]; then
  echo "RESULT: INCONCLUSIVE — a surface could not be reached at all (see NO-CONTACT above)."
  exit 4
fi

echo "FANG13_INVENTED_ANSWERS=$INVENTED   # want 0: nothing the provider did not send"
for s in rest_tools rest_plain v1_plain ws_plain trunc; do
  echo "FANG13_$(printf '%s' "$s" | tr 'a-z' 'A-Z')=$(verdict_of "$s")"
done
echo

if [ "$INVENTED" != "0" ]; then
  echo "RED — $INVENTED surface(s) handed back an answer the provider never sent."
  echo "      A turn where the provider said nothing is indistinguishable, to those"
  echo "      callers, from a turn it answered."
  echo "RESULT: RED"
  exit 0
fi

MISSING=""
for s in rest_tools rest_plain v1_plain trunc ws_plain; do
  [ "$(verdict_of "$s")" = "FAILURE-REPORTED" ] || MISSING="$MISSING $s"
done
if [ -n "$MISSING" ]; then
  echo "RED — no surface invents an answer any more, but these do not report the"
  echo "      failure either:$MISSING"
  echo "RESULT: RED"
  exit 0
fi

echo "GREEN — no surface invents an answer, and REST, /v1 non-streaming and the"
echo "        WebSocket all report the turn as failed."
echo "GAP (measured, not fixed here): the two SSE surfaces still report neither an"
echo "     answer nor the failure — both drop the agent loop's join handle"
echo "     (routes.rs send_message_stream, openai_compat.rs stream_response:"
echo "     'let (rx, _handle)'), so no loop failure of any kind has ever reached"
echo "     them, max-iterations included. Their classes above say so."
echo "RESULT: GREEN — an empty provider response no longer passes as an answer"
exit 1
