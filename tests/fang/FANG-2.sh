#!/usr/bin/env bash
# FANG-2 — acceptance for scripts/ofrelease, run end to end on a disposable stand.
#
# NOT registered in run.sh: that registry holds repros of defects on `ours`,
# each with a witness that a specific bug happened. This is the acceptance
# test for a new tool. Mixing the two would make "12 repros, all witnessed"
# stop meaning what it means.
#
# WHAT IT PROVES, IN ORDER
#   T1  the fingerprint is stable while a live daemon idles — without that,
#       "--dry-run changed nothing" is unprovable
#   T2  promote --dry-run leaves the fingerprint, the ledger and the snapshot
#       directory exactly as they were
#   T3  a real promote moves the image AND migrates the schema (8 -> 9), and
#       records a rollback point
#   T4  promoting the same image again is a no-op: same container id, no new
#       ledger lines, exit 0
#   T5  the three production guards refuse, each proved by its own call
#   T6  rollback --with-volume puts back the container object, the image and
#       the data, and PROVES it field by field against the pre-promote record
#   T7  a promote to an image that never becomes healthy fails and, with
#       --auto-rollback, restores the previous release (also exercises --build)
#
# THE STAND
#   container ofrelease-stand      volume ofrelease-stand-data   port 4203
#   image     openfang-openfang:v0.6.9-pristine, on a COPY of
#             openfang-staging-premigration (mounted :ro during the copy).
#   The premigration volume is at PRAGMA user_version 8 and the fork images
#   are at 9, which is what makes a data rollback observable rather than
#   merely asserted.
#
# Nothing here touches production: the stand is created and destroyed by this
# script, and T5 spends its whole existence being refused.
#
# Usage: tests/fang/FANG-2.sh [--keep]     (--keep leaves the stand up)
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OFRELEASE="${OFRELEASE:-$HERE/../../scripts/ofrelease}"

STAND=ofrelease-stand
STAND_VOL=ofrelease-stand-data
STAND_PORT=4203
REF_VOL="${FANG2_REF_VOLUME:-openfang-staging-premigration}"
OLD_IMAGE="${FANG2_OLD_IMAGE:-openfang-openfang:v0.6.9-pristine}"
NEW_IMAGE="${FANG2_NEW_IMAGE:-openfang:sprint3}"
BROKEN_IMAGE=ofrelease-broken:test

PROD_CONTAINER=openfang-openfang-1
PROD_VOLUME=openfang_openfang-data
PROD_PORT=4200

KEEP=''
[ "${1:-}" = "--keep" ] && KEEP=1

export OFRELEASE_STATE_DIR="${OFRELEASE_STATE_DIR:-/var/lib/ofrelease}"
export OFRELEASE_BACKUP_ROOT="${OFRELEASE_BACKUP_ROOT:-/srv/backups/ofrelease}"

PASS=0; FAIL=0
ok()   { printf 'PASS  %s\n' "$*"; PASS=$((PASS + 1)); }
bad()  { printf 'FAIL  %s\n' "$*"; FAIL=$((FAIL + 1)); }
note() { printf '      %s\n' "$*"; }
head_() { printf '\n=== %s\n' "$*"; }
eq()   { if [ "$2" = "$3" ]; then ok "$1 ($2)"; else bad "$1: expected '$2', got '$3'"; fi; }
ne()   { if [ "$2" != "$3" ]; then ok "$1 ($2 != $3)"; else bad "$1: both are '$2'"; fi; }

fp()      { "$OFRELEASE" fingerprint --container "$STAND"; }
st()      { "$OFRELEASE" status --container "$STAND" --json; }
stfield() { st | python3 -c 'import json,sys; print(json.load(sys.stdin).get(sys.argv[1],""))' "$1"; }
ledger()  { printf '%s/%s.jsonl' "$OFRELEASE_STATE_DIR" "$STAND"; }
ledger_lines() { [ -f "$(ledger)" ] && wc -l < "$(ledger)" | tr -d ' ' || echo 0; }
snap_count()   { ls -1 "$OFRELEASE_BACKUP_ROOT/$STAND" 2>/dev/null | wc -l | tr -d ' '; }

# ------------------------------------------------------------ safety first --
# This script creates and destroys containers and volumes. Before it does any
# of that it proves that the names it will act on are not production's — the
# same discipline the tool under test is being tested for.
for n in "$STAND" "$STAND_VOL"; do
  case "$n" in
    "$PROD_CONTAINER"|"$PROD_VOLUME") echo "FANG-2: stand name collides with production ($n)" >&2; exit 2 ;;
  esac
done
[ "$STAND_PORT" = "$PROD_PORT" ] && { echo "FANG-2: stand port is production's" >&2; exit 2; }
[ -x "$OFRELEASE" ] || { echo "FANG-2: no executable ofrelease at $OFRELEASE" >&2; exit 3; }

teardown() {
  [ -n "$KEEP" ] && { printf '\n(--keep: stand left up on %s)\n' "$STAND_PORT"; return 0; }
  printf '\n--- teardown\n'
  docker ps -a --format '{{.Names}}' | grep -E "^${STAND}(--prev-.*)?$" | while read -r c; do
    docker rm -f "$c" >/dev/null 2>&1 && echo "removed container $c"
  done
  for c in ofr-guard-volume ofr-guard-port; do
    docker inspect "$c" >/dev/null 2>&1 && docker rm -f "$c" >/dev/null 2>&1 && echo "removed decoy $c"
  done
  for v in "$STAND_VOL" ofr-guard-scratch; do
    docker volume inspect "$v" >/dev/null 2>&1 && docker volume rm "$v" >/dev/null 2>&1 && echo "removed volume $v"
  done
  docker image inspect "$BROKEN_IMAGE" >/dev/null 2>&1 && docker rmi "$BROKEN_IMAGE" >/dev/null 2>&1 && echo "removed image $BROKEN_IMAGE"
  rm -f "$(ledger)"
  rm -rf "${OFRELEASE_BACKUP_ROOT:?}/$STAND"
  echo "removed ledger and snapshots for $STAND"
  # The reference volume must survive, always.
  docker volume inspect "$REF_VOL" >/dev/null 2>&1 &&
    echo "reference volume $REF_VOL: intact" || echo "WARNING: $REF_VOL is MISSING"
}
trap teardown EXIT INT TERM HUP

# ------------------------------------------------------------------ stand ---
head_ "stand: $STAND on 127.0.0.1:$STAND_PORT from $OLD_IMAGE"
df -h / | tail -1
docker image inspect "$OLD_IMAGE" >/dev/null 2>&1 || { echo "missing image $OLD_IMAGE" >&2; exit 3; }
docker image inspect "$NEW_IMAGE" >/dev/null 2>&1 || { echo "missing image $NEW_IMAGE" >&2; exit 3; }
docker volume inspect "$REF_VOL"  >/dev/null 2>&1 || { echo "missing volume $REF_VOL" >&2; exit 3; }

docker ps -a --format '{{.Names}}' | grep -E "^${STAND}(--prev-.*)?$" | while read -r c; do
  docker rm -f "$c" >/dev/null 2>&1 && echo "cleared leftover container $c"
done
docker volume rm "$STAND_VOL" >/dev/null 2>&1
docker volume create "$STAND_VOL" >/dev/null
# :ro on the source. A daemon booted on the reference would migrate it and the
# comparison point would be gone.
docker run --rm -v "$REF_VOL":/from:ro -v "$STAND_VOL":/to alpine:latest sh -c 'cp -a /from/. /to/' ||
  { echo "clone failed" >&2; exit 1; }
# The reference volume's config.toml carries the api_key that leaked into the
# public git history; the clone gets a fresh one before it is bound to a port.
docker run --rm -v "$STAND_VOL":/data --entrypoint python3 "$OLD_IMAGE" -c '
import re, secrets, sys
p = "/data/config.toml"; text = open(p, encoding="utf-8").read()
f = re.findall(r"^api_key = \"([^\"]*)\"$", text, re.M)
if len(f) != 1: sys.exit("expected exactly one top-level api_key, found %d" % len(f))
new = "of-" + secrets.token_hex(24)
out = text.replace("api_key = \"%s\"" % f[0], "api_key = \"%s\"" % new, 1)
if out.count(new) != 1 or len(out.splitlines()) != len(text.splitlines()):
    sys.exit("key rotation did not land cleanly")
open(p, "w", encoding="utf-8").write(out); print("api_key rotated in the clone")' || exit 1

docker run -d --name "$STAND" -e OPENFANG_LISTEN=0.0.0.0:4200 \
  -p 127.0.0.1:"$STAND_PORT":4200 -v "$STAND_VOL":/data "$OLD_IMAGE" >/dev/null || exit 1

for i in $(seq 1 60); do
  curl -sS -m 5 "http://127.0.0.1:$STAND_PORT/api/health" 2>/dev/null | grep -q '"status":"ok"' && break
  sleep 1
done
rm -f "$(ledger)"; rm -rf "${OFRELEASE_BACKUP_ROOT:?}/$STAND"
"$OFRELEASE" status --container "$STAND"

BASE_UV="$(stfield user_version)"; BASE_AGENTS="$(stfield agents)"
BASE_IMAGE="$(stfield image_id)"; BASE_ID="$(stfield id)"
eq "stand starts at the pre-migration schema" 8 "$BASE_UV"
eq "stand guards pass" pass "$(stfield guards)"

# ------------------------------------------------------------------- T1 -----
head_ "T1  the fingerprint does not drift while the daemon idles"
F1="$(fp)"; sleep 6; F2="$(fp)"
eq "fingerprint stable over 6s of live traffic-free operation" "$F1" "$F2"

# ------------------------------------------------------------------- T2 -----
head_ "T2  promote --dry-run changes nothing"
L0="$(ledger_lines)"; S0="$(snap_count)"; FPB="$(fp)"
"$OFRELEASE" promote --container "$STAND" --image "$NEW_IMAGE" --dry-run >/tmp/fang2-dry.out 2>&1
eq "dry-run exits 0" 0 "$?"
grep -q 'DRY RUN' /tmp/fang2-dry.out && ok "dry-run printed a plan" || bad "dry-run printed no plan"
FPA="$(fp)"
eq "fingerprint before == after" "$FPB" "$FPA"
eq "no ledger line was written" "$L0" "$(ledger_lines)"
eq "no snapshot was taken" "$S0" "$(snap_count)"
eq "container id unchanged" "$BASE_ID" "$(stfield id)"
eq "image unchanged" "$BASE_IMAGE" "$(stfield image_id)"
note "fingerprint: $FPB"

# ------------------------------------------------------------------- T3 -----
head_ "T3  promote $OLD_IMAGE -> $NEW_IMAGE"
"$OFRELEASE" promote --container "$STAND" --image "$NEW_IMAGE" >/tmp/fang2-promote.out 2>&1
PRC=$?
sed 's/^/      | /' /tmp/fang2-promote.out
eq "promote exits 0" 0 "$PRC"
NEW_ID="$(docker image inspect -f '{{.Id}}' "$NEW_IMAGE")"
eq "target now runs the new image" "$NEW_ID" "$(stfield image_id)"
ne "container object was replaced" "$BASE_ID" "$(stfield id)"
eq "the new image migrated the schema" 9 "$(stfield user_version)"
eq "agents survived the promote" "$BASE_AGENTS" "$(stfield agents)"
grep -q '"status":"ok"' <<<"$(stfield health)" && ok "health is ok after promote" || bad "health after promote: $(stfield health)"
grep -q '^integrity_check : ok$' /tmp/fang2-promote.out && ok "the snapshot was integrity_check'd" || bad "no integrity_check in the promote log"
SNAP="$(sed -n 's/^snapshot   : //p' /tmp/fang2-promote.out | tail -1)"
[ -d "$SNAP" ] && ok "snapshot exists on disk: $SNAP" || bad "snapshot directory missing: '$SNAP'"
PREV_CT="$(docker ps -a --format '{{.Names}}' | grep "^${STAND}--prev-" | head -1)"
[ -n "$PREV_CT" ] && ok "previous container kept as $PREV_CT" || bad "no --prev- container was kept"
eq "the kept container still is the old image" \
   "$(docker image inspect -f '{{.Id}}' "$OLD_IMAGE")" "$(docker inspect -f '{{.Image}}' "$PREV_CT" 2>/dev/null)"
python3 - "$(ledger)" <<'PY'
import json, sys
recs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
p = [r for r in recs if r.get("event") == "promote"]
res = [r for r in recs if r.get("event") == "promote-result"]
assert p, "no promote record"
assert res and res[-1]["status"] == "ok", "no ok promote-result"
assert p[-1]["pre"]["user_version"] == "8", p[-1]["pre"]
print("      ledger: %s pre=%s snapshot=%s" % (p[-1]["id"], p[-1]["pre"], p[-1]["snapshot"]))
PY
[ $? -eq 0 ] && ok "the ledger holds a complete, parseable rollback point" || bad "ledger record is missing or malformed"

# ------------------------------------------------------------------- T4 -----
head_ "T4  promoting the same image again is a no-op"
ID_BEFORE="$(stfield id)"; L_BEFORE="$(ledger_lines)"; S_BEFORE="$(snap_count)"; FP_BEFORE="$(fp)"
"$OFRELEASE" promote --container "$STAND" --image "$NEW_IMAGE" >/tmp/fang2-idem.out 2>&1
eq "repeat promote exits 0" 0 "$?"
grep -q 'idempotent' /tmp/fang2-idem.out && ok "it says so instead of redeploying" || bad "no idempotence message"
eq "container id untouched" "$ID_BEFORE" "$(stfield id)"
eq "no new ledger line" "$L_BEFORE" "$(ledger_lines)"
eq "no new snapshot" "$S_BEFORE" "$(snap_count)"
eq "fingerprint untouched" "$FP_BEFORE" "$(fp)"

# ------------------------------------------------------------------- T5 -----
head_ "T5  the three production guards, one call each"

printf '\n--- T5a  container %s\n' "$PROD_CONTAINER"
"$OFRELEASE" promote --container "$PROD_CONTAINER" --image "$NEW_IMAGE" --dry-run >/tmp/fang2-g1.out 2>&1
G1=$?
sed 's/^/      | /' /tmp/fang2-g1.out
eq "refused with exit 2" 2 "$G1"
grep -q "REFUSING \[guard: container\]" /tmp/fang2-g1.out && ok "the container guard named itself" || bad "no container-guard line"

printf '\n--- T5b  a container that mounts %s\n' "$PROD_VOLUME"
# Created, never started, and the production volume is mounted READ-ONLY:
# the decoy exists only to be refused.
docker rm -f ofr-guard-volume >/dev/null 2>&1
docker create --name ofr-guard-volume -e OPENFANG_HOME=/data \
  -p 127.0.0.1:4299:4200 -v "$PROD_VOLUME":/data:ro alpine:latest true >/dev/null || exit 1
"$OFRELEASE" promote --container ofr-guard-volume --image "$NEW_IMAGE" --dry-run >/tmp/fang2-g2.out 2>&1
G2=$?
sed 's/^/      | /' /tmp/fang2-g2.out
eq "refused with exit 2" 2 "$G2"
grep -q "REFUSING \[guard: volume\]" /tmp/fang2-g2.out && ok "the volume guard fired on its own" || bad "no volume-guard line"
grep -q "REFUSING \[guard: container\]" /tmp/fang2-g2.out && bad "the container guard also fired — the checks are not independent" || ok "the container guard stayed quiet: the checks are independent"
docker rm -f ofr-guard-volume >/dev/null
eq "the production volume still exists after the decoy is gone" 0 "$(docker volume inspect "$PROD_VOLUME" >/dev/null 2>&1; echo $?)"

printf '\n--- T5c  a container that publishes host port %s\n' "$PROD_PORT"
docker rm -f ofr-guard-port >/dev/null 2>&1
docker volume create ofr-guard-scratch >/dev/null
# `create`, never `start`: the binding is declared, never taken, so production
# keeps its port throughout.
docker create --name ofr-guard-port -e OPENFANG_HOME=/data \
  -p 127.0.0.1:"$PROD_PORT":4200 -v ofr-guard-scratch:/data alpine:latest true >/dev/null || exit 1
"$OFRELEASE" promote --container ofr-guard-port --image "$NEW_IMAGE" --dry-run >/tmp/fang2-g3.out 2>&1
G3=$?
sed 's/^/      | /' /tmp/fang2-g3.out
eq "refused with exit 2" 2 "$G3"
grep -q "REFUSING \[guard: port\]" /tmp/fang2-g3.out && ok "the port guard fired on its own" || bad "no port-guard line"
grep -qE "REFUSING \[guard: (container|volume)\]" /tmp/fang2-g3.out && bad "another guard also fired" || ok "only the port guard fired"
docker rm -f ofr-guard-port >/dev/null; docker volume rm ofr-guard-scratch >/dev/null
curl -sS -m 5 "http://127.0.0.1:$PROD_PORT/api/health" 2>/dev/null | grep -q '"status":"ok"' &&
  ok "production is still answering on $PROD_PORT" || bad "production is NOT answering on $PROD_PORT"

# ------------------------------------------------------------------- T6 -----
head_ "T6  rollback --with-volume, and the proof it demands"
"$OFRELEASE" rollback --container "$STAND" --with-volume >/tmp/fang2-rb.out 2>&1
RRC=$?
sed 's/^/      | /' /tmp/fang2-rb.out
eq "rollback exits 0" 0 "$RRC"
eq "image is back" "$(docker image inspect -f '{{.Id}}' "$OLD_IMAGE")" "$(stfield image_id)"
eq "the ORIGINAL container object is back" "$BASE_ID" "$(stfield id)"
eq "schema is back at 8" "$BASE_UV" "$(stfield user_version)"
eq "agent count is back" "$BASE_AGENTS" "$(stfield agents)"
grep -q '"status":"ok"' <<<"$(stfield health)" && ok "health is ok after rollback" || bad "health after rollback: $(stfield health)"
grep -q 'NO$' /tmp/fang2-rb.out && bad "the proof table contains a NO" || ok "every asserted field in the proof table matched"
grep -qE '^  image_id .*yes$' /tmp/fang2-rb.out && ok "rollback proved the image, it did not just claim it" || bad "no image_id proof row"
grep -qE '^  user_version .*yes$' /tmp/fang2-rb.out && ok "rollback proved the schema version" || bad "no user_version proof row"
grep -q 'integrity_check' /tmp/fang2-rb.out && ok "rollback took its own snapshot first" || bad "rollback did not snapshot before acting"

# ------------------------------------------------------------------- T7 -----
head_ "T7  a promote that never becomes healthy, with --auto-rollback (and --build)"
BUILD_CTX="$(mktemp -d)"
printf 'FROM alpine:latest\nCMD ["sleep", "600"]\n' > "$BUILD_CTX/Dockerfile"
GOOD_ID="$(stfield image_id)"; GOOD_UV="$(stfield user_version)"
OFRELEASE_HEALTH_TIMEOUT=12 "$OFRELEASE" promote --container "$STAND" --image "$BROKEN_IMAGE" \
  --build "$BUILD_CTX" --auto-rollback >/tmp/fang2-broken.out 2>&1
BRC=$?
sed 's/^/      | /' /tmp/fang2-broken.out
rm -rf "$BUILD_CTX"
ne "promote of an unhealthy image does not exit 0" 0 "$BRC"
grep -q 'auto-rollback' /tmp/fang2-broken.out && ok "--auto-rollback engaged" || bad "--auto-rollback did not engage"
eq "the stand is back on the working image" "$GOOD_ID" "$(stfield image_id)"
eq "schema untouched by the failed promote" "$GOOD_UV" "$(stfield user_version)"
grep -q '"status":"ok"' <<<"$(stfield health)" && ok "the stand is serving again" || bad "stand health: $(stfield health)"

head_ "history"
"$OFRELEASE" history --container "$STAND"

head_ "result"
printf 'passed %d, failed %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
