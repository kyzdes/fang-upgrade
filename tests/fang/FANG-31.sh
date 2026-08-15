#!/usr/bin/env bash
# FANG-31 — Telegram 409 Conflict: the polling task is never cancelled explicitly,
# so every channel hot-reload abandons an in-flight getUpdates long-poll and the
# freshly started poller collides with it on the Telegram side.
#
# What this script measures — one number, and it must be 0 after the fix:
#   OF31_409_COUNT   — number of "Telegram 409 Conflict" WARNs caused by ONE
#                      `POST /api/channels/reload` while a long-poll is in flight
#
# Two earlier attempts to also say something about the channel's state *after*
# the reload have been removed rather than repaired:
#
#   OF31_DEAF_SECS   was the difference between two unrelated stub timestamps,
#                    and when the fix works there are no 409s at all, so its
#                    second term defaulted to 0 and the "duration" printed was
#                    the absolute age of the stub.
#   OF31_REATTACHED  ("did the stub accept another getUpdates afterwards") was
#                    yes on openfang:sprint3 as well: the abandoned long-poll
#                    times out after 30 s and the new poller attaches anyway.
#                    It never distinguished the fixed daemon from the broken
#                    one, so it was not evidence of anything.
#
# What this script therefore does NOT check: that the channel is alive in any
# sense beyond the absence of 409s. A change that stopped the Telegram adapter
# on reload and never started it again would print GREEN here. The stub view it
# prints shows every getUpdates, so an operator can see for themselves; the run
# that actually fails on a poller that does not come back after a reload is
# FANG-40-sigterm.sh ("poller did not restart after the reload").
#
# How it works: a stub Telegram Bot API is started inside the target daemon's
# network namespace and enforces Telegram's single-reader rule (409 while another
# getUpdates is in flight). No real Telegram token and no api.telegram.org traffic
# is involved — nothing can steal the queue of a live bot.
#
# Usage:  ./FANG-31.sh [base_url]           # default: the container's own port
# Env:    OF_CONTAINER (default openfang-staging) — everything else is derived
#         from it; OPENFANG_URL, OF_CONFIG, OF_STUB_PORT override explicitly.
#
# Idempotent: restores the original config.toml, kills the stub and removes its
# work directory on every exit path, and can be run repeatedly.

set -uo pipefail

CONTAINER="${OF_CONTAINER:-openfang-staging}"
STUB_PORT="${OF_STUB_PORT:-8098}"
FAKE_TOKEN="111111:AAAA-fake-token-for-count-probe"

# ---------------------------------------------------------------- prod guard --
if [ "$CONTAINER" = "openfang-openfang-1" ]; then
  echo "REFUSING: $CONTAINER is production." >&2; exit 2
fi
for bin in docker nsenter python3 curl; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 3; }
done
docker inspect "$CONTAINER" >/dev/null 2>&1 || { echo "no such container: $CONTAINER" >&2; exit 3; }

# The config file this script rewrites and the daemon it reloads are both read
# off the container. They used to be independent: the config path was hardcoded
# to openfang-staging-data, so `OF_CONTAINER=mine ./FANG-31.sh` rewrote the
# shared stand's config.toml and secrets.env while probing something else.
DATA_DIR="$(docker inspect \
  -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$CONTAINER")"
HOST_PORT="$(docker inspect \
  -f '{{with index .NetworkSettings.Ports "4200/tcp"}}{{(index . 0).HostPort}}{{end}}' "$CONTAINER")"
CONFIG="${OF_CONFIG:-${DATA_DIR:+$DATA_DIR/config.toml}}"
BASE_URL="${1:-${OPENFANG_URL:-${HOST_PORT:+http://127.0.0.1:$HOST_PORT}}}"
[ -n "$CONFIG" ] || { echo "no /data mount on $CONTAINER — set OF_CONFIG" >&2; exit 3; }
[ -n "$BASE_URL" ] || { echo "no published 4200/tcp on $CONTAINER — set OPENFANG_URL" >&2; exit 3; }

case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. FANG-31 restarts the" \
                "Telegram channel and must only run against a staging copy." >&2; exit 2;;
esac
[ -f "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 3; }

# Created after the guards, so a refused run leaves nothing behind at all.
WORK="$(mktemp -d /tmp/fang31.XXXXXX)"

API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" | head -1)"
NETPID="$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")" || exit 3
SECRETS="$(dirname "$CONFIG")/secrets.env"

api() { # api METHOD PATH [BODY]
  local m="$1" p="$2" b="${3:-}"
  if [ -n "$b" ]; then
    curl -sS -m 120 -X "$m" -H 'Content-Type: application/json' \
         ${API_KEY:+-H "Authorization: Bearer $API_KEY"} -d "$b" "$BASE_URL$p"
  else
    curl -sS -m 120 -X "$m" ${API_KEY:+-H "Authorization: Bearer $API_KEY"} "$BASE_URL$p"
  fi
}

cleanup() {
  [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null
  if [ -f "$WORK/config.toml.orig" ]; then
    cp "$WORK/config.toml.orig" "$CONFIG"
    api POST /api/channels/reload '{}' >/dev/null 2>&1
    echo "restored $CONFIG"
  fi
  # The stub log and the config backup are printed above by the time we get
  # here; leaving /tmp/fang31.* behind on a box with a 12 GB disk threshold is
  # how a probe becomes the problem it was written to find.
  case "$WORK" in /tmp/fang31.*) rm -rf "$WORK";; esac
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- the stub --
cat > "$WORK/stub.py" <<'PY'
import json, re, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
T0 = time.time(); LOCK = threading.Lock(); INFLIGHT = 0
LOG = open(sys.argv[2], "a", buffering=1)
def log(m): LOG.write("%.3f %s\n" % (time.time() - T0, m))
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def _json(self, o, code=200):
        b = json.dumps(o).encode(); self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers()
        self.wfile.write(b)
    def handle_one(self):
        global INFLIGHT
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n) if n else b""
        m = re.match(r"/bot([^/]+)/(\w+)", self.path)
        meth = m.group(2) if m else "?"
        if meth == "getMe":
            return self._json({"ok": True, "result": {"id": 1, "is_bot": True,
                    "username": "stub_probe_bot", "first_name": "Stub"}})
        if meth in ("deleteWebhook", "setMyCommands", "setWebhook"):
            return self._json({"ok": True, "result": True})
        if meth == "getUpdates":
            try: p = json.loads(body) if body else {}
            except Exception: p = {}
            with LOCK:
                INFLIGHT += 1; cur = INFLIGHT
            log("getUpdates START peer=%s:%s verb=%s params=%s inflight=%d"
                % (self.client_address[0], self.client_address[1], self.command,
                   json.dumps(p), cur))
            # Telegram allows exactly one reader of the update queue.
            if cur > 1:
                with LOCK: INFLIGHT -= 1
                log("getUpdates 409 peer=%s:%s" % self.client_address)
                return self._json({"ok": False, "error_code": 409,
                    "description": "Conflict: terminated by other getUpdates request; "
                                   "make sure that only one bot instance is running"}, 409)
            log("getUpdates ACCEPTED")
            try: t = float(p.get("timeout", 0))
            except Exception: t = 0.0
            time.sleep(min(t, 30.0))
            with LOCK: INFLIGHT -= 1
            return self._json({"ok": True, "result": []})
        return self._json({"ok": True, "result": []})
    do_GET = do_POST = handle_one
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

echo "=== FANG-31 · Telegram 409 on channel hot-reload ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "target            : $BASE_URL  (container $CONTAINER)"
echo "daemon version    : $(api GET /api/health | python3 -c 'import sys,json;print(json.load(sys.stdin).get("version","?"))' 2>/dev/null)"
echo "image             : $(docker inspect -f '{{.Image}}' "$CONTAINER" | cut -c1-19)"

cp "$CONFIG" "$WORK/config.toml.orig"
grep -q '^TEST_FAKE_TG_TOKEN=' "$SECRETS" 2>/dev/null || \
  echo "TEST_FAKE_TG_TOKEN=$FAKE_TOKEN" >> "$SECRETS"

python3 - "$CONFIG" "$STUB_PORT" <<'PY'
import re, sys
path, port = sys.argv[1], sys.argv[2]
s = open(path).read()
s = re.sub(r"(?ms)^\[channels\.telegram\].*?(?=^\[|\Z)", "", s)
s += ('\n[channels.telegram]\nbot_token_env = "TEST_FAKE_TG_TOKEN"\n'
      'poll_interval_secs = 1\napi_url = "http://127.0.0.1:%s"\n' % port)
open(path, "w").write(s)
PY

setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" "$WORK/stub.log" \
  >"$WORK/stub.err" 2>&1 < /dev/null &
STUB_PID=$!
sleep 2

# Attach the poller to the stub and wait until it holds a long-poll.
api POST /api/channels/reload '{}' >/dev/null
for _ in $(seq 40); do grep -q "getUpdates ACCEPTED" "$WORK/stub.log" 2>/dev/null && break; sleep 1; done
grep -q "getUpdates ACCEPTED" "$WORK/stub.log" || { echo "FAIL: poller never attached"; exit 1; }
echo "poller attached, long-poll in flight (timeout=30s)"

# ------------------------------------------------------------- the experiment --
BEFORE="$(docker logs "$CONTAINER" 2>&1 | grep -c '409 Conflict')"
echo "--- POST /api/channels/reload while the long-poll is in flight ---"
api POST /api/channels/reload '{}'; echo
sleep 45

AFTER="$(docker logs "$CONTAINER" 2>&1 | grep -c '409 Conflict')"
OF31_409_COUNT=$((AFTER - BEFORE))

echo
echo "--- stub view (one line per getUpdates) ---"
cat "$WORK/stub.log"
echo
echo "--- daemon log ---"
docker logs --timestamps --since 3m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E "Shutting down channel adapter|polling loop stopped|bot @|409 Conflict|hot-reload complete"
echo
echo "OF31_409_COUNT=$OF31_409_COUNT      # 409 WARNs caused by one reload (want 0)"
echo
if [ "$OF31_409_COUNT" -gt 0 ]; then
  echo "RESULT: RED — reload collides with the abandoned long-poll"
  exit 1
else
  echo "RESULT: GREEN — one reload, no 409 conflict. This says nothing about"
  echo "        whether the channel is still polling; see the stub view above."
fi
