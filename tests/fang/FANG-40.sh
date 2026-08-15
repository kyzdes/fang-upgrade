#!/usr/bin/env bash
# FANG-40 — a channel deleted from config.toml never stops polling.
#
# `start_channel_bridge_with_config` inserts every started adapter into
# `kernel.channel_adapters` and only ever removes one when its start() failed.
# Delete a channel from config.toml and reload: the bridge that owned it is
# stopped, but the map keeps the Arc alive, `ChannelAdapter::stop()` is never
# called, and the adapter's own polling task keeps talking to the platform for
# the life of the process.
#
# What this script measures:
#   OF40_POLLS_AFTER_DELETE — requests the stub receives in the 20 s after the
#                             channel was deleted from config and reloaded.
#                             Want 0. It does not decay: the poller is immortal.
#
# A stub Telegram Bot API runs inside the target daemon's network namespace, so
# no real token and no api.telegram.org traffic is involved.
#
# Usage:  ./FANG-40.sh [base_url]           # default: $OPENFANG_URL or staging
# Env:    OPENFANG_URL, OF_CONTAINER, OF_CONFIG, OF_STUB_PORT
#
# Idempotent: restores config.toml and kills the stub on every exit path.

set -uo pipefail

CONTAINER="${OF_CONTAINER:-openfang-staging}"
STUB_PORT="${OF_STUB_PORT:-8097}"
FAKE_TOKEN="111111:AAAA-fake-token-for-evict-probe"

# ---------------------------------------------------------------- prod guard --
if [ "$CONTAINER" = "openfang-openfang-1" ]; then
  echo "REFUSING: $CONTAINER is production." >&2; exit 2
fi
for bin in docker nsenter python3 curl; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 3; }
done
docker inspect "$CONTAINER" >/dev/null 2>&1 || { echo "no such container: $CONTAINER" >&2; exit 3; }

# Same correction FANG-31.sh got in this commit, and for the same reason: the
# config path was hardcoded to openfang-staging-data while the container was a
# variable, so `OF_CONTAINER=mine ./FANG-40.sh` rewrote the shared stand's
# config.toml while probing something else. Both are read off the container now.
DATA_DIR="$(docker inspect \
  -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$CONTAINER")"
HOST_PORT="$(docker inspect \
  -f '{{with index .NetworkSettings.Ports "4200/tcp"}}{{(index . 0).HostPort}}{{end}}' "$CONTAINER")"
CONFIG="${OF_CONFIG:-${DATA_DIR:+$DATA_DIR/config.toml}}"
BASE_URL="${1:-${OPENFANG_URL:-${HOST_PORT:+http://127.0.0.1:$HOST_PORT}}}"
[ -n "$CONFIG" ]   || { echo "no /data mount on $CONTAINER — set OF_CONFIG" >&2; exit 3; }
[ -n "$BASE_URL" ] || { echo "no published 4200/tcp on $CONTAINER — set OPENFANG_URL" >&2; exit 3; }
case "$BASE_URL" in
  *:4200*) echo "REFUSING: $BASE_URL looks like production. FANG-40 rewrites the" \
                "channel config and must only run against the staging copy." >&2; exit 2;;
esac
[ -f "$CONFIG" ] || { echo "no config at $CONFIG" >&2; exit 3; }

# After the guards, so a refused run leaves no /tmp/fang40.* behind.
WORK="$(mktemp -d /tmp/fang40.XXXXXX)"

API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" | head -1)"
NETPID="$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")" || exit 3
SECRETS="$(dirname "$CONFIG")/secrets.env"
LOG="$WORK/stub.log"

reload() {
  curl -sS -m 120 -X POST -H 'Content-Type: application/json' \
       ${API_KEY:+-H "Authorization: Bearer $API_KEY"} -d '{}' \
       "$BASE_URL/api/channels/reload"
  echo
}

cleanup() {
  [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null
  if [ -f "$WORK/config.toml.orig" ]; then
    cp "$WORK/config.toml.orig" "$CONFIG"
    reload >/dev/null 2>&1
    echo "restored $CONFIG"
  fi
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- the stub --
cat > "$WORK/stub.py" <<'PY'
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
T0 = time.time(); LOG = open(sys.argv[2], "a", buffering=1)
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def handle_one(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n: self.rfile.read(n)
        # Never log self.path — it carries the bot token.
        method = self.path.rsplit("/", 1)[-1].split("?")[0]
        LOG.write("%.3f %s\n" % (time.time() - T0, method))
        if "getMe" in self.path:
            o = {"ok": True, "result": {"id": 1, "is_bot": True,
                                        "username": "evict_probe_bot", "first_name": "E"}}
        elif "getUpdates" in self.path:
            time.sleep(2)                      # short poll keeps the probe quick
            o = {"ok": True, "result": []}
        else:
            o = {"ok": True, "result": True}
        b = json.dumps(o).encode(); self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers()
        self.wfile.write(b)
    do_GET = do_POST = handle_one
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

echo "=== FANG-40 · adapter for a deleted channel keeps polling ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "target            : $BASE_URL  (container $CONTAINER)"
echo "daemon version    : $(curl -sS -m 10 ${API_KEY:+-H "Authorization: Bearer $API_KEY"} \
                             "$BASE_URL/api/health" \
                             | python3 -c 'import sys,json;print(json.load(sys.stdin).get("version","?"))' 2>/dev/null)"
echo "image             : $(docker inspect -f '{{.Image}}' "$CONTAINER" | cut -c1-19)"

cp "$CONFIG" "$WORK/config.toml.orig"
grep -q '^TEST_FAKE_TG_TOKEN=' "$SECRETS" 2>/dev/null || \
  echo "TEST_FAKE_TG_TOKEN=$FAKE_TOKEN" >> "$SECRETS"

set_channel() { # set_channel present|absent
  python3 - "$CONFIG" "$STUB_PORT" "$1" <<'PY'
import re, sys
path, port, mode = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(path).read()
s = re.sub(r"(?ms)^\[channels\.telegram\].*?(?=^\[|\Z)", "", s)
if mode == "present":
    s += ('\n[channels.telegram]\nbot_token_env = "TEST_FAKE_TG_TOKEN"\n'
          'poll_interval_secs = 1\napi_url = "http://127.0.0.1:%s"\n' % port)
open(path, "w").write(s)
PY
}

set_channel present
setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" "$LOG" \
  >"$WORK/stub.err" 2>&1 < /dev/null &
STUB_PID=$!
sleep 2

echo "--- reload with the channel present ---"
reload
sleep 8
BEFORE="$(grep -c getUpdates "$LOG" 2>/dev/null || echo 0)"
echo "getUpdates while the channel exists: $BEFORE"
[ "$BEFORE" -gt 0 ] || { echo "FAIL: poller never attached to the stub"; exit 1; }

# ------------------------------------------------------------- the experiment --
set_channel absent
echo "--- [channels.telegram] deleted from config, reloading ---"
reload
MARK="$(grep -c getUpdates "$LOG" 2>/dev/null || echo 0)"
sleep 20
AFTER="$(grep -c getUpdates "$LOG" 2>/dev/null || echo 0)"
OF40_POLLS_AFTER_DELETE=$((AFTER - MARK))

echo
echo "--- daemon log ---"
docker logs --timestamps --since 2m "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E "Unregistered channel adapter|polling loop stopped|Shutting down channel adapter|hot-reload complete"
echo
echo "OF40_POLLS_AFTER_DELETE=$OF40_POLLS_AFTER_DELETE      # stub requests 20s after the delete (want 0)"
# Exit 1 on RED, 0 on GREEN, like FANG-31.sh and FANG-40-sigterm.sh. This used
# to return 0 either way, so in a pipeline the RED verdict passed silently —
# the reader saw the word, a gate saw success.
if [ "$OF40_POLLS_AFTER_DELETE" -gt 0 ]; then
  echo "RESULT: RED — the deleted channel's poller is still running"
  exit 1
else
  echo "RESULT: GREEN — the adapter was stopped and unregistered"
  exit 0
fi
