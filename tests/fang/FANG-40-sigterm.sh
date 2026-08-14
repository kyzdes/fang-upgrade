#!/usr/bin/env bash
# FANG-40 follow-up — SIGTERM that lands while a channel hot-reload is in flight.
#
# The FANG-40 fix made `BridgeManager::stop()` drain Telegram's in-flight
# `getUpdates` before starting the new poller, so `POST /api/channels/reload`
# now holds its HTTP connection open for the remainder of that long-poll
# (~30 s).  The daemon serves axum through `with_graceful_shutdown`, which waits
# for every in-flight request before `axum::serve(..).await` returns — and every
# line of shutdown work (`stop_fast()`, `kernel.shutdown()`, removing
# daemon.json) lives *after* that await.  So a SIGTERM delivered during a reload
# is absorbed: the container burns its whole stop grace waiting for the reload
# and the supervisor SIGKILLs it.
#
# What this script measures, on whatever image it is pointed at:
#
#   OF40S_RELOAD_SECS        wall time of POST /api/channels/reload with a
#                            30 s long-poll outstanding.  Informational: it is
#                            the length of the window the next number lives in.
#   OF40S_STOP_SECS          wall time of `docker stop -t 10` fired 3 s into
#                            that reload.
#   OF40S_STOP_RC            the container's exit code.  0 = the process
#                            reached its own exit path.  137 = SIGKILL, i.e.
#                            the grace period ran out.
#   OF40S_PLAIN_STOP_SECS    control: same stop, no reload in flight, poller
#                            parked in the same 30 s long-poll.
#   OF40S_PLAIN_STOP_RC      control exit code.
#
# Acceptance: OF40S_STOP_RC = 0 and OF40S_STOP_SECS well inside the 10 s grace.
#
# The daemon under test is a throwaway container on its own volume; the stub
# Telegram Bot API runs inside its network namespace, so no real token and no
# api.telegram.org traffic is involved.  Production (:4200,
# openfang-openfang-1) is refused outright and the shared staging container is
# never touched.
#
# Usage:  OF_IMAGE=openfang:fang40 ./FANG-40-sigterm.sh
# Env:    OF_IMAGE (required), OF_CONTAINER, OF_VOLUME, OF_PORT, OF_STUB_PORT,
#         OF_POLL_SECS (stub long-poll length, default 30)

set -uo pipefail

IMAGE="${OF_IMAGE:?set OF_IMAGE to the image under test}"
CONTAINER="${OF_CONTAINER:-openfang-sigterm}"
VOLUME="${OF_VOLUME:-openfang-sigterm-data}"
PORT="${OF_PORT:-4207}"
STUB_PORT="${OF_STUB_PORT:-8098}"
POLL_SECS="${OF_POLL_SECS:-30}"
GRACE=10
FAKE_TOKEN="111111:AAAA-fake-token-for-sigterm-probe"
WORK="$(mktemp -d /tmp/fang40s.XXXXXX)"

# ---------------------------------------------------------------- prod guard --
case "$CONTAINER" in
  openfang-openfang-1|openfang-staging)
    echo "REFUSING: $CONTAINER is not a throwaway container." >&2; exit 2;;
esac
case "$PORT" in 4200|4201) echo "REFUSING: port $PORT is prod/staging." >&2; exit 2;; esac
for bin in docker nsenter python3 curl; do
  command -v "$bin" >/dev/null || { echo "missing dependency: $bin" >&2; exit 3; }
done

BASE_URL="http://127.0.0.1:$PORT"
STUB_PID=""

cleanup() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null
  docker rm -f "$CONTAINER" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

# ------------------------------------------------------------------- the stub --
cat > "$WORK/stub.py" <<'PY'
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
T0 = time.time(); LOG = open(sys.argv[2], "a", buffering=1)
POLL = float(sys.argv[3])
class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def handle_one(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n: self.rfile.read(n)
        # Never log self.path — it carries the bot token (FANG-43).
        method = self.path.rsplit("/", 1)[-1].split("?")[0]
        LOG.write("%.3f %s\n" % (time.time() - T0, method))
        if "getMe" in self.path:
            o = {"ok": True, "result": {"id": 1, "is_bot": True,
                                        "username": "sigterm_probe_bot", "first_name": "S"}}
        elif "getUpdates" in self.path:
            time.sleep(POLL)               # emulate Telegram's long-poll
            o = {"ok": True, "result": []}
        else:
            o = {"ok": True, "result": True}
        b = json.dumps(o).encode()
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(b))); self.end_headers()
            self.wfile.write(b)
        except Exception:
            pass                            # client hung up mid-poll; fine
    do_GET = do_POST = handle_one
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

start_daemon() { # start_daemon -> sets NETPID
  docker rm -f "$CONTAINER" >/dev/null 2>&1
  docker run -d --name "$CONTAINER" \
    -e OPENFANG_LISTEN=0.0.0.0:4200 -e OPENFANG_HOME=/data \
    -v "$VOLUME:/data" -p "127.0.0.1:$PORT:4200" \
    "$IMAGE" start >/dev/null || return 1
  local ok=1
  for _ in $(seq 60); do
    curl -sS -m 2 -H "Authorization: Bearer $API_KEY" "$BASE_URL/api/health" \
      >/dev/null 2>&1 && { ok=0; break; }
    sleep 0.5
  done
  NETPID="$(docker inspect -f '{{.State.Pid}}' "$CONTAINER")"
  return $ok
}

echo "=== FANG-40 follow-up · SIGTERM during a channel hot-reload ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "image             : $IMAGE ($(docker image inspect -f '{{.Id}}' "$IMAGE" | cut -c1-19))"
echo "container         : $CONTAINER on $BASE_URL, volume $VOLUME"
echo "stub long-poll    : ${POLL_SECS}s     docker stop grace: ${GRACE}s"
echo

# --- seed a throwaway volume: api_key + the telegram channel on the stub ----
# The daemon binds 0.0.0.0 inside the container, so requests arriving through
# the published port are not loopback and need a Bearer key. Seeding the volume
# before first boot is also what makes this run reproducible: no leftover state
# from an earlier run can change the numbers.
docker volume rm "$VOLUME" >/dev/null 2>&1
docker volume create "$VOLUME" >/dev/null || { echo "FAIL: cannot create $VOLUME"; exit 1; }
VOLDIR="$(docker volume inspect -f '{{.Mountpoint}}' "$VOLUME")"
API_KEY="of-sigterm-probe-$(date +%s)"
cat > "$VOLDIR/config.toml" <<EOF
api_key = "$API_KEY"

[channels.telegram]
bot_token_env = "TEST_FAKE_TG_TOKEN"
poll_interval_secs = 1
api_url = "http://127.0.0.1:$STUB_PORT"
EOF
printf 'TEST_FAKE_TG_TOKEN=%s\n' "$FAKE_TOKEN" > "$VOLDIR/secrets.env"
chmod 600 "$VOLDIR/config.toml" "$VOLDIR/secrets.env"

start_daemon || { echo "FAIL: $CONTAINER never answered /api/health"; exit 1; }

LOG="$WORK/stub.log"
setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" "$LOG" "$POLL_SECS" \
  >"$WORK/stub.err" 2>&1 < /dev/null &
STUB_PID=$!
sleep 1

reload_now() {
  curl -sS -m 180 -o "$WORK/reload.out" -X POST -H 'Content-Type: application/json' \
       -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload"
}

wait_for_inflight_poll() { # returns once a getUpdates is outstanding
  for _ in $(seq 60); do
    grep -q getUpdates "$LOG" 2>/dev/null && return 0
    sleep 0.5
  done
  return 1
}

timed_stop() { # timed_stop -> echoes "SECS RC"
  local t0 t1
  t0=$(date +%s.%N)
  docker stop -t "$GRACE" "$CONTAINER" >/dev/null 2>&1
  t1=$(date +%s.%N)
  local rc; rc="$(docker inspect -f '{{.State.ExitCode}}' "$CONTAINER")"
  echo "$(echo "$t1 $t0" | awk '{printf "%.2f", $1-$2}') $rc"
}

# ============================ control: stop with no reload in flight =========
echo "--- control: SIGTERM with a ${POLL_SECS}s long-poll outstanding, no reload ---"
# kick the poller
curl -sS -m 60 -X POST -H 'Content-Type: application/json' \
     -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload" \
     >/dev/null 2>&1
wait_for_inflight_poll || { echo "FAIL: poller never reached the stub"; exit 1; }
sleep 3
read -r PLAIN_SECS PLAIN_RC <<<"$(timed_stop)"
echo "docker stop: ${PLAIN_SECS}s  exit=$PLAIN_RC"
echo

# ============================ the experiment ================================
echo "--- experiment: SIGTERM 3 s into a reload that is draining the long-poll ---"
: > "$LOG"
start_daemon || { echo "FAIL: could not restart $CONTAINER"; exit 1; }
kill "$STUB_PID" 2>/dev/null
setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" "$LOG" "$POLL_SECS" \
  >>"$WORK/stub.err" 2>&1 < /dev/null &
STUB_PID=$!
sleep 1
# The daemon booted before the stub existed, so its poller is in connect-refused
# backoff. One reload restarts it against the now-listening stub.
curl -sS -m 60 -X POST -H 'Content-Type: application/json' \
     -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload" \
     >/dev/null 2>&1
wait_for_inflight_poll || { echo "FAIL: poller never reached the stub"; exit 1; }
sleep 2   # be sure we are inside the long-poll, not between two of them

R0=$(date +%s.%N)
reload_now >/dev/null 2>&1 &
RELOAD_JOB=$!
sleep 3
read -r STOP_SECS STOP_RC <<<"$(timed_stop)"
wait "$RELOAD_JOB" 2>/dev/null
R1=$(date +%s.%N)
RELOAD_SECS="$(echo "$R1 $R0" | awk '{printf "%.2f", $1-$2}')"

echo "docker stop: ${STOP_SECS}s  exit=$STOP_RC"
echo
echo "--- daemon log (tail) ---"
docker logs --timestamps "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E "SIGTERM|shutting down|hot-reload|polling loop|daemon stopped|drain" | tail -20
echo
echo "OF40S_RELOAD_SECS=$RELOAD_SECS"
echo "OF40S_STOP_SECS=$STOP_SECS"
echo "OF40S_STOP_RC=$STOP_RC              # 0 = clean exit, 137 = SIGKILL after the grace ran out"
echo "OF40S_PLAIN_STOP_SECS=$PLAIN_SECS"
echo "OF40S_PLAIN_STOP_RC=$PLAIN_RC"
echo
if [ "$STOP_RC" = "0" ]; then
  echo "RESULT: GREEN — the daemon reached its own exit path inside the ${GRACE}s grace"
else
  echo "RESULT: RED — SIGTERM during reload ends in SIGKILL (exit $STOP_RC)"
fi
