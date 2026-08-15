#!/usr/bin/env bash
# refstand — bring up the v0.6.9 reference stand on demand, poke it, tear it down.
#
# FANG-53. Staging used to lag production, and that lag was the comparison
# point: "does this also happen on the old build?" was answerable by asking
# staging. Once staging is promoted to the production image the lag is gone,
# and keeping a permanently-running stale stand just to preserve it costs a
# container, a port and a maintenance burden nobody pays.
#
# The comparison point is instead frozen in two artefacts that already exist
# on this box:
#
#   image  openfang-openfang:v0.6.9-pristine     upstream v0.6.9, no fork patches
#   volume openfang-staging-premigration          staging's /data before the
#                                                 sprint-3 schema migration
#
# This script boots them together on a spare port, waits for /api/health,
# and removes the container again on `down`.
#
# Two invariants it enforces, because getting either wrong destroys the
# reference:
#
#   1. The reference volume is NEVER mounted read-write. `up` copies it into
#      a scratch volume (default openfang-refstand-data) and mounts the copy.
#      A v0.6.9 daemon opening the premigration DB would run migrations on it
#      and it would stop being premigration. The source is mounted :ro even
#      during that copy.
#   2. `down` refuses to delete anything whose name is not the scratch
#      volume, and every subcommand refuses to touch the production or
#      staging container/port.
#
# The scratch clone gets a freshly generated api_key at `up` time: the
# reference volume's own config.toml still carries the key that leaked into
# the public git history (commit bf289dd), and a stand that answers to a
# published key is not something to bind to a port, even on loopback.
# `refstand.sh key` prints the ephemeral one; `refstand.sh api` uses it.
#
# Usage: refstand.sh --help

set -uo pipefail

REF_IMAGE="${REFSTAND_IMAGE:-openfang-openfang:v0.6.9-pristine}"
REF_VOLUME="${REFSTAND_REF_VOLUME:-openfang-staging-premigration}"
RUN_CONTAINER="${REFSTAND_CONTAINER:-openfang-refstand}"
RUN_VOLUME="${REFSTAND_VOLUME:-openfang-refstand-data}"
PORT="${REFSTAND_PORT:-4202}"
BIND="${REFSTAND_BIND:-127.0.0.1}"

PROD_CONTAINER=openfang-openfang-1
STAGING_CONTAINER=openfang-staging

log() { printf '%s\n' "$*" >&2; }
die() { log "refstand: $1"; exit "${2:-1}"; }

usage() {
  cat <<'EOF'
refstand — the v0.6.9 comparison point, up on demand (FANG-53)

USAGE
  refstand.sh up [--port N]     clone the reference volume, start the stand,
                                wait for /api/health, print what it is
  refstand.sh status            container / image / volume / port / health
  refstand.sh key               print the running stand's ephemeral api_key
  refstand.sh api M PATH [BODY] authenticated call against the stand
  refstand.sh down              stop + remove the container and the CLONE
                                (never the reference volume)
  refstand.sh --help

WHAT IT BOOTS
  image  openfang-openfang:v0.6.9-pristine   (override: REFSTAND_IMAGE)
  volume openfang-staging-premigration       (override: REFSTAND_REF_VOLUME)
         -> copied into openfang-refstand-data, which is what gets mounted
  port   127.0.0.1:4202                      (override: REFSTAND_PORT/BIND)

EXAMPLE
  tests/fang/refstand.sh up
  tests/fang/refstand.sh api GET /api/health
  tests/fang/refstand.sh api GET /api/agents
  tests/fang/refstand.sh down

NOTES
  * The reference volume is mounted read-only, and only during the copy.
    Booting a daemon directly on it would migrate its schema and burn the
    reference.
  * The clone is throwaway: `down` deletes it, `up` makes a fresh one. Any
    state you create on the stand dies with it, by design.
  * 4200 (production) and 4201 (staging) are rejected as the port.
EOF
}

# ------------------------------------------------------------------ guards --
guard_names() {
  [ "$RUN_CONTAINER" = "$PROD_CONTAINER" ] &&
    die "REFUSING: container name is production ($PROD_CONTAINER)." 2
  [ "$RUN_CONTAINER" = "$STAGING_CONTAINER" ] &&
    die "REFUSING: container name is staging ($STAGING_CONTAINER)." 2
  [ "$RUN_VOLUME" = "$REF_VOLUME" ] &&
    die "REFUSING: scratch volume == reference volume ($REF_VOLUME)." 2
  case "$RUN_VOLUME" in
    openfang_openfang-data|openfang-staging-data|*premigration*|*backup*)
      die "REFUSING: '$RUN_VOLUME' is not a scratch volume name." 2 ;;
  esac
  return 0
}

guard_port() {
  case "$PORT" in
    ''|*[!0-9]*) die "port must be a number, got '$PORT'" 2 ;;
    4200) die "REFUSING: 4200 is production." 2 ;;
    4201) die "REFUSING: 4201 is staging." 2 ;;
  esac
  return 0
}

require_docker() {
  command -v docker >/dev/null 2>&1 || die "docker not found on PATH" 3
  docker info >/dev/null 2>&1 || die "cannot talk to the docker daemon" 3
}

exists_container() { docker inspect "$1" >/dev/null 2>&1; }
exists_volume()    { docker volume inspect "$1" >/dev/null 2>&1; }

# Copy the reference volume into the clone. A tiny image with a shell does
# the work: alpine if it is already pulled (it is, on this box), otherwise
# the reference image itself — whose ENTRYPOINT is the openfang binary, so
# it has to be overridden, and --entrypoint has to precede the image name.
# `cp -a`, not `tar | tar`: the exit status of a pipeline is the exit status
# of its last command, so a failing source-side tar would report success.
clone_volume() {
  if docker image inspect alpine:latest >/dev/null 2>&1; then
    docker run --rm -v "$REF_VOLUME":/from:ro -v "$RUN_VOLUME":/to \
      alpine:latest sh -c 'cp -a /from/. /to/'
  else
    docker run --rm -v "$REF_VOLUME":/from:ro -v "$RUN_VOLUME":/to \
      --entrypoint /bin/sh "$REF_IMAGE" -c 'cp -a /from/. /to/'
  fi
}

base_url() { printf 'http://%s:%s' "$BIND" "$PORT"; }

# --------------------------------------------------------------------- up --
cmd_up() {
  require_docker; guard_names; guard_port

  docker image inspect "$REF_IMAGE" >/dev/null 2>&1 ||
    die "reference image '$REF_IMAGE' not found — nothing to compare against" 4
  exists_volume "$REF_VOLUME" ||
    die "reference volume '$REF_VOLUME' not found — nothing to compare against" 4

  if exists_container "$RUN_CONTAINER"; then
    die "container '$RUN_CONTAINER' already exists — run 'refstand.sh down' first"
  fi

  # Fresh clone every time: the stand is disposable, the reference is not.
  if exists_volume "$RUN_VOLUME"; then
    log "removing stale clone volume $RUN_VOLUME"
    docker volume rm "$RUN_VOLUME" >/dev/null || die "cannot remove stale $RUN_VOLUME"
  fi
  docker volume create "$RUN_VOLUME" >/dev/null || die "cannot create $RUN_VOLUME"

  log "cloning $REF_VOLUME (read-only) -> $RUN_VOLUME"
  clone_volume || die "volume clone failed"

  # Freshly generated key in the CLONE only. python3 ships in the openfang
  # image; this never rewrites the reference volume.
  log "issuing an ephemeral api_key for the clone"
  docker run --rm -v "$RUN_VOLUME":/data --entrypoint python3 "$REF_IMAGE" -c '
import re, secrets, sys
p = "/data/config.toml"
try:
    text = open(p, encoding="utf-8").read()
except FileNotFoundError:
    sys.exit("no config.toml in the cloned volume")
found = re.findall(r"^api_key = \"([^\"]*)\"$", text, re.M)
if len(found) != 1:
    sys.exit("expected exactly one top-level api_key line, found %d" % len(found))
old, new = found[0], "of-" + secrets.token_hex(24)
out = text.replace("api_key = \"%s\"" % old, "api_key = \"%s\"" % new, 1)
before, after = text.splitlines(), out.splitlines()
if len(before) != len(after):
    sys.exit("line count changed: %d -> %d" % (len(before), len(after)))
changed = [i for i, (a, b) in enumerate(zip(before, after)) if a != b]
if len(changed) != 1:
    sys.exit("expected exactly one changed line, got %r" % changed)
if out.count(new) != 1 or out.count(old) != 0:
    sys.exit("replacement did not land cleanly")
open(p, "w", encoding="utf-8").write(out)
print("api_key rotated in clone (%d lines, line %d changed)" % (len(after), changed[0] + 1))
' || die "could not issue a key for the clone"

  log "starting $RUN_CONTAINER from $REF_IMAGE on $(base_url)"
  docker run -d --name "$RUN_CONTAINER" \
    -e OPENFANG_LISTEN=0.0.0.0:4200 -e OPENFANG_HOME=/data \
    -p "$BIND":"$PORT":4200 \
    -v "$RUN_VOLUME":/data \
    "$REF_IMAGE" >/dev/null || die "docker run failed"

  local i health=""
  for i in $(seq 1 60); do
    health="$(curl -sS -m 5 "$(base_url)/api/health" 2>/dev/null)"
    case "$health" in *'"status":"ok"'*) break ;; esac
    health=""
    sleep 1
  done
  if [ -z "$health" ]; then
    log "stand did not answer /api/health in 60s; last 20 log lines:"
    docker logs --tail 20 "$RUN_CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' >&2
    die "up failed"
  fi

  log "up."
  cmd_status
}

# ----------------------------------------------------------------- status --
cmd_status() {
  require_docker
  if ! exists_container "$RUN_CONTAINER"; then
    printf 'refstand: down (no container %s)\n' "$RUN_CONTAINER"
    return 1
  fi
  printf 'container : %s (%s)\n' "$RUN_CONTAINER" \
    "$(docker inspect -f '{{.State.Status}}' "$RUN_CONTAINER")"
  printf 'image     : %s  %s\n' \
    "$(docker inspect -f '{{.Config.Image}}' "$RUN_CONTAINER")" \
    "$(docker inspect -f '{{.Image}}' "$RUN_CONTAINER")"
  printf 'volume    : %s (clone of %s)\n' "$RUN_VOLUME" "$REF_VOLUME"
  printf 'url       : %s\n' "$(base_url)"
  printf 'health    : %s\n' \
    "$(curl -sS -m 5 "$(base_url)/api/health" 2>/dev/null || echo '<no answer>')"
  printf 'schema    : PRAGMA user_version = %s\n' \
    "$(docker exec "$RUN_CONTAINER" python3 -c '
import sqlite3
try:
    print(sqlite3.connect("/data/data/openfang.db").execute("PRAGMA user_version").fetchone()[0])
except Exception as e:
    print("?", e)' 2>/dev/null || echo '?')"
}

# -------------------------------------------------------------------- key --
cmd_key() {
  require_docker
  exists_container "$RUN_CONTAINER" || die "refstand is down — 'refstand.sh up' first"
  docker exec "$RUN_CONTAINER" python3 -c '
import re, sys
text = open("/data/config.toml", encoding="utf-8").read()
found = re.findall(r"^api_key = \"([^\"]*)\"$", text, re.M)
sys.stdout.write(found[0] if len(found) == 1 else "")
' || die "could not read the stand key"
}

# -------------------------------------------------------------------- api --
cmd_api() {
  local method="${1:-}" path="${2:-}" body="${3:-}" key
  [ -n "$method" ] && [ -n "$path" ] || die "usage: refstand.sh api METHOD PATH [BODY]"
  key="$(cmd_key)" || exit 1
  if [ -n "$body" ]; then
    curl -sS -m 120 -X "$method" -H 'Content-Type: application/json' \
      -H "Authorization: Bearer $key" -d "$body" "$(base_url)$path"
  else
    curl -sS -m 120 -X "$method" -H "Authorization: Bearer $key" "$(base_url)$path"
  fi
}

# ------------------------------------------------------------------- down --
cmd_down() {
  require_docker; guard_names
  if exists_container "$RUN_CONTAINER"; then
    docker rm -f "$RUN_CONTAINER" >/dev/null && log "removed container $RUN_CONTAINER"
  else
    log "no container $RUN_CONTAINER"
  fi
  if exists_volume "$RUN_VOLUME"; then
    docker volume rm "$RUN_VOLUME" >/dev/null && log "removed clone volume $RUN_VOLUME"
  else
    log "no clone volume $RUN_VOLUME"
  fi
  exists_volume "$REF_VOLUME" &&
    log "reference volume $REF_VOLUME: intact" ||
    log "WARNING: reference volume $REF_VOLUME is MISSING"
  return 0
}

# ------------------------------------------------------------------- main --
cmd="${1:-}"
[ $# -gt 0 ] && shift
case "$cmd" in
  up)
    while [ $# -gt 0 ]; do
      case "$1" in
        --port) PORT="${2:-}"; shift 2 ;;
        --port=*) PORT="${1#--port=}"; shift ;;
        *) die "unknown argument for up: $1" ;;
      esac
    done
    cmd_up ;;
  status) cmd_status ;;
  key)    cmd_key; echo ;;
  api)    cmd_api "$@" ;;
  down)   cmd_down ;;
  -h|--help|help|'') usage ;;
  *) die "unknown command '$cmd' (try --help)" ;;
esac
