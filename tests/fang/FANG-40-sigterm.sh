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
# ---------------------------------------------------------------------------
# Why this script asserts more than "the container exited 0"
#
# Its first version checked exactly one thing: OF40S_STOP_RC = 0.  That is
# trivially true on an image where the reload never drains anything — the
# reload returns in milliseconds, there is nothing in flight when the SIGTERM
# lands, and the daemon exits cleanly because it was never in the trap.  Run
# against openfang:sprint3, which predates the whole fix, it printed
# "RESULT: GREEN" (tests/fang/baseline/FANG-40-sigterm-tautology.txt).  A guard
# that passes on the unfixed image guards nothing.
#
# So the run now establishes the trap before testing the escape:
#
#   OF40S_DRAIN_CODE   HTTP status of a reload issued with a long-poll
#                      outstanding.  Must be 200.
#   OF40S_DRAIN_SECS   wall time of that same reload, measured with nothing
#                      else happening.  This is the drain: on an image whose
#                      reload stops the adapter it is the remainder of the
#                      long-poll (~25-30 s of a 30 s poll); on an image that
#                      abandons the poller instead it is milliseconds.  Must be
#                      at least OF40S_DRAIN_MIN, or the SIGTERM below would not
#                      be landing in a drain at all.
#   OF40S_RELOAD_INFLIGHT
#                      whether the second reload was still running when the
#                      SIGTERM was sent.  "yes" or the experiment is void.
#   OF40S_STOP_SECS    wall time of `docker stop -t 10` fired 3 s into that
#                      second reload.
#   OF40S_STOP_RC      the container's exit code.  0 = the process reached its
#                      own exit path.  137 = SIGKILL, i.e. the grace ran out.
#   OF40S_EXIT_PATH    whether "OpenFang daemon stopped" appears in the log
#                      *after* "Received SIGTERM" — the shutdown work ran, as
#                      opposed to the process merely dying quietly.
#   OF40S_PLAIN_STOP_SECS / _RC
#                      control: same stop, no reload in flight, poller parked
#                      in the same 30 s long-poll.
#
# GREEN requires every one of those.  Exit status is 0 for GREEN, 1 for RED, so
# the script can be used as a gate and not only read.
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
# A reload that drains waits out the rest of the long-poll. A third of the poll
# is well above the milliseconds a non-draining reload takes and well below the
# ~28 s a draining one takes, so the threshold does not need to be tight.
DRAIN_MIN="$(awk -v p="$POLL_SECS" 'BEGIN{printf "%.2f", p/3}')"
FAKE_TOKEN="111111:AAAA-fake-token-for-sigterm-probe"
WORK="$(mktemp -d /tmp/fang40s.XXXXXX)"

# ---------------------------------------------------------------- prod guard --
case "$CONTAINER" in
  openfang-openfang-1|openfang-staging)
    echo "REFUSING: $CONTAINER is not a throwaway container." >&2; exit 2;;
esac
case "$PORT" in
  4200|4201|4202) echo "REFUSING: port $PORT is prod/staging/refstand." >&2; exit 2;;
esac
# The volume is the thing this script DESTROYS (`docker volume rm` below), and
# it was the one input the guard never looked at. Checking the container name
# while deleting whatever OF_VOLUME happens to say is guarding the wrong door:
# OF_VOLUME=openfang-staging-data with any throwaway container name wiped the
# stand. Two rules, both on the name that is actually acted on.
case "$VOLUME" in
  openfang_openfang-data|openfang-staging-data|openfang-staging-premigration|openfang-prod-backup-*|openfang-refstand-data)
    echo "REFUSING: $VOLUME is a real volume, not a throwaway." >&2; exit 2;;
esac
case "$VOLUME" in
  *-sigterm-data|openfang-sigterm-*) ;;
  *) echo "REFUSING: $VOLUME is not a scratch volume name for this probe." >&2
     echo "  expected something matching '*-sigterm-data' or 'openfang-sigterm-*'." >&2
     exit 2;;
esac
# A scratch name that already exists is someone else's run, not stale state of
# ours: recreating it would delete their data mid-flight.
if docker volume inspect "$VOLUME" >/dev/null 2>&1; then
  if [ "${OF_REUSE_VOLUME:-0}" != 1 ]; then
    echo "REFUSING: volume $VOLUME already exists — another run may be using it." >&2
    echo "  remove it yourself, pick another OF_VOLUME, or set OF_REUSE_VOLUME=1." >&2
    exit 2
  fi
fi
for bin in docker nsenter python3 curl awk; do
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

start_stub() {
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null
  setsid nsenter -t "$NETPID" -n python3 "$WORK/stub.py" "$STUB_PORT" "$LOG" "$POLL_SECS" \
    >>"$WORK/stub.err" 2>&1 < /dev/null &
  STUB_PID=$!
  sleep 1
}

polls_seen() { # how many getUpdates the stub has seen so far
  local n
  n="$(grep -c getUpdates "$LOG" 2>/dev/null)"   # prints 0 and exits 1 when none
  echo "${n:-0}"
}

wait_for_new_poll() { # wait_for_new_poll <baseline> — a getUpdates started since
  local base="$1" now
  for _ in $(seq 90); do
    now="$(polls_seen)"
    [ "${now:-0}" -gt "$base" ] && return 0
    sleep 0.5
  done
  return 1
}

secs_between() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", b-a}'; }
ge() { awk -v a="$1" -v b="$2" 'BEGIN{exit !(a>=b)}'; }
lt() { awk -v a="$1" -v b="$2" 'BEGIN{exit !(a<b)}'; }

timed_reload() { # timed_reload -> echoes "SECS HTTPCODE"
  local t0 t1 code
  t0=$(date +%s.%N)
  code="$(curl -sS -m 180 -o "$WORK/reload.out" -w '%{http_code}' \
          -X POST -H 'Content-Type: application/json' \
          -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload" \
          2>/dev/null)"
  t1=$(date +%s.%N)
  echo "$(secs_between "$t0" "$t1") ${code:-000}"
}

timed_stop() { # timed_stop -> echoes "SECS RC"
  local t0 t1 rc
  t0=$(date +%s.%N)
  docker stop -t "$GRACE" "$CONTAINER" >/dev/null 2>&1
  t1=$(date +%s.%N)
  rc="$(docker inspect -f '{{.State.ExitCode}}' "$CONTAINER")"
  echo "$(secs_between "$t0" "$t1") $rc"
}

echo "=== FANG-40 follow-up · SIGTERM during a channel hot-reload ==="
echo "date              : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "image             : $IMAGE ($(docker image inspect -f '{{.Id}}' "$IMAGE" | cut -c1-19))"
echo "container         : $CONTAINER on $BASE_URL, volume $VOLUME"
echo "stub long-poll    : ${POLL_SECS}s     docker stop grace: ${GRACE}s"
echo "drain threshold   : ${DRAIN_MIN}s"
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

LOG="$WORK/stub.log"
: > "$LOG"

start_daemon || { echo "FAIL: $CONTAINER never answered /api/health"; exit 1; }
start_stub

# ============================ control: stop with no reload in flight =========
echo "--- control: SIGTERM with a ${POLL_SECS}s long-poll outstanding, no reload ---"
# The daemon booted before the stub existed, so its poller is in connect-refused
# backoff. One reload restarts it against the now-listening stub.
BASE="$(polls_seen)"
curl -sS -m 60 -X POST -H 'Content-Type: application/json' \
     -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload" \
     >/dev/null 2>&1
wait_for_new_poll "$BASE" || { echo "FAIL: poller never reached the stub"; exit 1; }
sleep 3
read -r PLAIN_SECS PLAIN_RC <<<"$(timed_stop)"
echo "docker stop: ${PLAIN_SECS}s  exit=$PLAIN_RC"
echo

# ============================ the drain itself ==============================
# Measured on its own, with no stop racing it: this is the window the SIGTERM
# below has to land inside, and on an image without the FANG-40 adapter stop
# there is no window at all.
echo "--- reload with a long-poll outstanding: how long does the drain take? ---"
: > "$LOG"
start_daemon || { echo "FAIL: could not restart $CONTAINER"; exit 1; }
start_stub
BASE="$(polls_seen)"
curl -sS -m 60 -X POST -H 'Content-Type: application/json' \
     -H "Authorization: Bearer $API_KEY" -d '{}' "$BASE_URL/api/channels/reload" \
     >/dev/null 2>&1
wait_for_new_poll "$BASE" || { echo "FAIL: poller never reached the stub"; exit 1; }
sleep 2   # be sure we are inside the long-poll, not between two of them
PRE_DRAIN="$(polls_seen)"
read -r DRAIN_SECS DRAIN_CODE <<<"$(timed_reload)"
echo "POST /api/channels/reload: ${DRAIN_SECS}s  HTTP $DRAIN_CODE"
echo

# ============================ the experiment ================================
echo "--- experiment: SIGTERM 3 s into a reload that is draining the long-poll ---"
# The reload above left a fresh poller behind; wait for its long-poll to be the
# one outstanding, so the next reload has something to drain.
wait_for_new_poll "$PRE_DRAIN" || { echo "FAIL: poller did not restart after the reload"; exit 1; }
sleep 2

timed_reload > "$WORK/reload.timing" 2>/dev/null &
RELOAD_JOB=$!
sleep 3
if kill -0 "$RELOAD_JOB" 2>/dev/null; then RELOAD_INFLIGHT=yes; else RELOAD_INFLIGHT=no; fi
read -r STOP_SECS STOP_RC <<<"$(timed_stop)"
wait "$RELOAD_JOB" 2>/dev/null

echo "reload still in flight when SIGTERM was sent: $RELOAD_INFLIGHT"
echo "docker stop: ${STOP_SECS}s  exit=$STOP_RC"
echo

DLOG="$WORK/daemon.log"
docker logs --timestamps "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' > "$DLOG"
echo "--- daemon log (tail) ---"
grep -E "SIGTERM|shutting down|hot-reload|polling loop|daemon stopped|in flight|bridge to stop" \
  "$DLOG" | tail -20
echo

# The exit path ran at all: "OpenFang daemon stopped" is the last line of
# run_daemon(), printed after stop_fast() and kernel.shutdown().
SIG_LINE="$(grep -n "Received SIGTERM" "$DLOG" | tail -1 | cut -d: -f1)"
END_LINE="$(grep -n "OpenFang daemon stopped" "$DLOG" | tail -1 | cut -d: -f1)"
if [ -n "$SIG_LINE" ] && [ -n "$END_LINE" ] && [ "$END_LINE" -gt "$SIG_LINE" ]; then
  EXIT_PATH=yes
else
  EXIT_PATH=no
fi

echo "OF40S_DRAIN_SECS=$DRAIN_SECS"
echo "OF40S_DRAIN_CODE=$DRAIN_CODE"
echo "OF40S_DRAIN_MIN=$DRAIN_MIN"
echo "OF40S_RELOAD_INFLIGHT=$RELOAD_INFLIGHT"
echo "OF40S_STOP_SECS=$STOP_SECS"
echo "OF40S_STOP_RC=$STOP_RC              # 0 = clean exit, 137 = SIGKILL after the grace ran out"
echo "OF40S_EXIT_PATH=$EXIT_PATH"
echo "OF40S_PLAIN_STOP_SECS=$PLAIN_SECS"
echo "OF40S_PLAIN_STOP_RC=$PLAIN_RC"
echo

# ------------------------------------------------------------------ verdict --
FAILED=0
check() { # check <description> <ok?>
  if [ "$2" = 1 ]; then printf '  PASS  %s\n' "$1"
  else printf '  FAIL  %s\n' "$1"; FAILED=1; fi
}
echo "--- checks ---"
check "reload answers 200 with a long-poll outstanding (got $DRAIN_CODE)" \
      "$([ "$DRAIN_CODE" = "200" ] && echo 1 || echo 0)"
check "reload really drains: ${DRAIN_SECS}s >= ${DRAIN_MIN}s" \
      "$(ge "$DRAIN_SECS" "$DRAIN_MIN" && echo 1 || echo 0)"
check "SIGTERM landed while that reload was in flight ($RELOAD_INFLIGHT)" \
      "$([ "$RELOAD_INFLIGHT" = "yes" ] && echo 1 || echo 0)"
check "container exited 0, not SIGKILL (got $STOP_RC)" \
      "$([ "$STOP_RC" = "0" ] && echo 1 || echo 0)"
check "stop finished inside the ${GRACE}s grace (${STOP_SECS}s)" \
      "$(lt "$STOP_SECS" "$GRACE" && echo 1 || echo 0)"
check "shutdown path ran: 'daemon stopped' after 'Received SIGTERM' ($EXIT_PATH)" \
      "$([ "$EXIT_PATH" = "yes" ] && echo 1 || echo 0)"
check "control stop is clean too (rc=$PLAIN_RC)" \
      "$([ "$PLAIN_RC" = "0" ] && echo 1 || echo 0)"
echo

if [ "$FAILED" = 0 ]; then
  echo "RESULT: GREEN — the daemon reached its own exit path inside the ${GRACE}s grace,"
  echo "        with a real ${DRAIN_SECS}s drain in flight when the SIGTERM arrived"
  exit 0
else
  echo "RESULT: RED — see the failed checks above"
  exit 1
fi
