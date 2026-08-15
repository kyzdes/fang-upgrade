#!/usr/bin/env bash
# tests/fang/run.sh — run every FANG reproduction in this directory and say,
# in one place and without rounding, what each one actually proved.
#
# WHY THIS IS NOT JUST `for f in *.sh; do "$f"; done`
#
#   The scripts in here do not agree on how to report themselves, and two of
#   them do not report themselves at all. Pretending otherwise is how a
#   directory of eight probes turns into "all green":
#
#     * A-2.sh answers in its EXIT CODE, and inverted at that: 0 means the
#       defect reproduced, 1 means it did not.
#     * FANG-31.sh / FANG-43.sh answer in a "RESULT: RED|GREEN|INCONCLUSIVE"
#       line on stdout; their exit code is whatever the last command in the
#       script happened to return and means nothing.
#     * A-1.sh, A-4.sh, A-6.sh, A-7.sh and FANG-45.sh print measurements and
#       a written-out red criterion, and leave the verdict to a reader. There
#       is no honest way to machine-grade them, so this runner does not
#       pretend to: they come back as REVIEW, which is not a pass.
#     * The four sprint-4 scenario repros (FANG-9/10/13/47) were written to a
#       fixed convention — see MODE exit4 below — including a distinct code
#       for "my own control failed, so I proved nothing".
#
#   And five of them spend real money: A-4, A-6, A-7, FANG-43 and FANG-45 all
#   drive real agents against real providers on the staging box. They are
#   gated behind --costly and reported as SKIPPED, with the reason, when the
#   gate is closed. A skip is never a pass here.
#
# WHY A VERDICT IS NOT BELIEVED ON ITS OWN  (the false-green this runner had)
#
#   Every probe here reports on the DIFFERENCE between two observations. A
#   probe that could not make either observation still computes a difference,
#   and that difference is zero, and zero is the shape of "no defect". Run
#   A-2.sh with a wrong api_key and it says, in full:
#
#       step 1: POST /api/models/custom -> HTTP 401 "Invalid API key"
#       step 2: DELETE .../drift-probe  -> HTTP 401 "Invalid API key"
#       RESULT: drift after delete = 0 model(s)
#       VERDICT: GREEN — counter consistent
#
#   It created nothing, deleted nothing, measured 0-0, exited 1, and this
#   runner used to print PASS and exit 0 — on a build with the fix AND on
#   v0.6.9-pristine, which does not have it. A verdict from a probe that was
#   never able to work is not a verdict, and it is emphatically not a pass.
#
#   Two barriers stand in front of every GRADE — PASS, FAIL and REVIEW alike,
#   in every mode:
#
#     1. PREFLIGHT. One credential is resolved here — $OPENFANG_API_KEY if it
#        is set, otherwise the top-level api_key in the matched container's
#        config.toml — and asked of the target before anything runs (see AUTH
#        GATE). Accepted means the target ANSWERED it, a 2xx. If the target
#        rejects it, nothing is graded: every selected repro is reported
#        INCONCLUSIVE and the run fails. If the target answers neither yes nor
#        no (a 404, a 500, no answer at all), the credential is UNVERIFIED and
#        the report says so instead of calling it accepted.
#
#        This is the key the probes send, and that is now a property of the
#        probes rather than a hope: A-1/A-2/A-4 go through ofctl, which reads
#        the environment first; the other nine did the environment-blind thing
#        — `sed -n 's/^api_key…' "$CONFIG"` — so `OPENFANG_API_KEY=<key>
#        ./run.sh` proved one credential and sent another. They resolve it the
#        same way now; `grep -n 'OPENFANG_API_KEY' tests/fang/*.sh
#        tests/fang/harness/lib.sh` is the whole of the claim.
#     2. AUTHENTICITY SCREEN. After each repro, its log is checked for
#        (a) authentication/transport failures — the signatures of a probe
#        that could not reach or could not enter the stand, and
#        (b) its WITNESS: a pattern the log must contain to show the probe
#        really created or changed the thing it then measured. Either check
#        failing turns the verdict into INCONCLUSIVE, whatever the mode said,
#        including REVIEW and including a would-be PASS.
#
#   Both barriers only ever turn a verdict INTO INCONCLUSIVE. Neither can
#   manufacture a pass, so a bug in them costs a false alarm, never a false
#   all-clear.
#
#   SKIPPED is the one verdict the screen leaves alone, and deliberately. A
#   skip is not a grade — it is a probe saying "I did not run, and here is
#   why". It is already not a pass; demoting it to INCONCLUSIVE would replace
#   the reason it gave ("no fangrig stub on this host") with a reason it never
#   gave ("no evidence the probe did its work") and turn the run's honest
#   "nothing ran" exit 4 into a failure exit 1. The operator would be told
#   something that did not happen.
#
# ONE STAND, AND WHY THE PAIRING STILL MATTERS  (the rm -rf this runner had)
#
#   A stand is a URL, a container, a data directory and a key. `--base-url`
#   names the first, `--container` names the second, and NOTHING used to check
#   that the two were the same machine. The data directory was then derived
#   from the container while every id, agent and verdict came from the URL —
#   and the clean-up at the end did `rm -rf "$DATA_DIR/agents/$name"` with a
#   name it had read from the OTHER stand's API. Point --base-url at one stand
#   and --container at another and the run deleted files, as root, on a stand
#   it never so much as sent a request to, then reported "removed agent X" and
#   "the stand carries no probe agent now" — both about the wrong machine.
#
#   The clean-up here no longer derives a path at all: it deletes through the
#   API, at $BASE_URL, the one channel that cannot address the wrong machine,
#   and whatever is left on disk is listed by name and left alone. But the
#   pairing is still load-bearing, because the runner hands the data directory
#   to the probes, and the probes write:
#
#     OPENFANG_VOLUME     -> A-1.sh:69      rm -rf "$VOL/agents/test-a1-probe"
#     OPENFANG_HOME_HOST  -> A-6.sh:159,
#                            A-7.sh:150     rm -rf "$DATA/workspaces/test-a{6,7}-*"
#     OPENFANG_CONFIG /
#     OF_CONFIG           -> fangrig:193    rewrites [channels.telegram] in
#                            fangrig:275    that config.toml, and rm -rf's
#                                           "$(dirname CONFIG)/workspaces/fangrig-*"
#
#   Those variables are how a probe is TOLD which volume belongs to $BASE_URL;
#   unset, A-1 and A-7 decline to touch disk at all and say so. So exporting
#   them is an authorisation, and it is only honest to hand it out when the
#   pair is actually established.
#
#   WHAT THE CHECK ESTABLISHES, AND WHAT IT CANNOT. It asks docker which host
#   ports $CONTAINER publishes and looks for a TCP publication of BASE_URL's
#   port on an address that covers BASE_URL's host. That establishes the pair
#   only when BASE_URL names THIS machine — a published host port belongs to
#   one container at a time on the host that publishes it, and that is the
#   whole of the argument. It says nothing about a URL pointing somewhere else:
#   a container published on 0.0.0.0 matches the port of any host you care to
#   name, and a run whose requests go to another box would have got the local
#   container's data directory. Measured, not reasoned: with $CONTAINER
#   publishing 4218/UDP and a different container serving 4218/TCP, this file
#   printed "PROVEN", exported the first one's volume, and A-1.sh deleted
#   agents/test-a1-probe on it while every request went to the second.
#
#   So the pair counts as established only when all three hold — a TCP
#   publication, the port, and a BASE_URL host that resolves to an address of
#   this machine. Anything else: exit 2, nothing exported, nothing touched.
#   The price is that a stand reached through a proxy, or one on --network
#   host with no published port, cannot be run from here even when it is
#   plainly the same box. That is a real cost, and it buys the one thing above
#   that a wrong answer would spend: root-owned deletes on someone else's data.
#
#   The data directory is then taken from that container's /data mount; if it
#   has none, the run refuses rather than falling back to a default volume
#   that belongs to a different stand.
#
# VERDICT MODES
#   exit4    0=RED(reproduced) 1=GREEN(not reproduced) 2=REFUSED 3=SKIPPED
#            4=INCONCLUSIVE(the script's own control failed)
#   a2       A-2.sh's own convention: 0=RED 1=GREEN 3=SKIPPED 4=INCONCLUSIVE
#   result   last "RESULT: RED|GREEN|INCONCLUSIVE" line on stdout wins;
#            the exit code is recorded but not believed
#   manual   no machine-readable verdict exists -> REVIEW
#
# OUTCOMES
#   PASS          the verdict matched what this build is expected to produce,
#                 AND the probe demonstrably did its work
#   FAIL          it did not (a fixed defect came back, or a repro rotted)
#   INCONCLUSIVE  the repro ran but disqualified itself, or could not prove it
#                 did any work -> counted as failure
#   REVIEW        ran, produced evidence, needs a human -> not a pass
#   SKIPPED       did not run, with a reason -> never a pass
#   ERROR         crashed, timed out, or returned a code its mode cannot read
#
# EXIT CODE
#   0  at least one repro was GRADED, and every graded repro reached the
#      verdict this build should produce. The closing line says how many were
#      graded.
#   1  at least one FAIL or INCONCLUSIVE
#   2  at least one ERROR, a usage error, the preflight could not establish
#      that the probes can talk to the target, or the target could not be
#      resolved to one stand (harness problem, not a verdict)
#   3  refused: pointed at production
#   4  nothing was graded — every selected repro was skipped, or ran in a mode
#      that produces no machine-readable verdict (REVIEW), or both. Deliberately
#      not 0: "no failures" out of a set nothing was checked against is the
#      exact shape of a report that reads as an all-clear. A run of only
#      REVIEW rows is that set — the scripts measured, and a human has not
#      read the measurements yet.
#
# Usage:
#   ./run.sh [--costly] [--only id[,id...]] [--list] [--timeout SECS]
#            [--out DIR] [--base-url URL] [--container NAME] [--keep-probes]

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_URL="${OPENFANG_URL:-http://127.0.0.1:4201}"
CONTAINER="${OF_CONTAINER:-openfang-staging}"
TIMEOUT_SECS=900
RUN_COSTLY=0
ONLY=""
ONLY_GIVEN=0
OUT_DIR=""
LIST_ONLY=0
KEEP_PROBES=0

usage_error() { echo "run.sh: $*" >&2; echo "run.sh: try --help" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --costly)     RUN_COSTLY=1; shift ;;
    --only)       [ $# -ge 2 ] || usage_error "--only needs a value"; ONLY="$2"; ONLY_GIVEN=1; shift 2 ;;
    --list)       LIST_ONLY=1; shift ;;
    --timeout)    [ $# -ge 2 ] || usage_error "--timeout needs a value"; TIMEOUT_SECS="$2"; shift 2 ;;
    --out)        [ $# -ge 2 ] || usage_error "--out needs a value"; OUT_DIR="$2"; shift 2 ;;
    --base-url)   [ $# -ge 2 ] || usage_error "--base-url needs a value"; BASE_URL="$2"; shift 2 ;;
    --container)  [ $# -ge 2 ] || usage_error "--container needs a value"; CONTAINER="$2"; shift 2 ;;
    --keep-probes) KEEP_PROBES=1; shift ;;
    # Print the header comment, and only it: stop at the first line that is
    # not a comment, so this never drifts out of date when the header grows.
    -h|--help)    sed -n '2,${/^[^#]/q;p;}' "$0"; exit 0 ;;
    *) usage_error "unknown flag: $1" ;;
  esac
done

# ---------------------------------------------------------------- registry --
# id | script | mode | expect | cost | witness | covers
#
# `expect` is what THIS branch should produce, and it is a claim about the
# code, not a wish: RED for defects with no fix commit on `ours`, GREEN for
# defects a fix commit claims to have closed. If a GREEN entry comes back RED
# the fix regressed; if a RED entry comes back GREEN either someone fixed it
# or the repro stopped reproducing — both are FAIL, both need a human.
#
# `witness` is the authenticity precondition: an ERE (several joined by `&&`)
# that the repro's log MUST contain before its verdict is graded at all. It
# names the moment the probe demonstrably DID something to the stand — the
# agent it created, the model the API confirmed it added, the control turn the
# stand actually served. Not "the script ran"; "the script got its hands on
# the system under test". `-` means no witness could be grounded in the
# script's own output; those rows are listed as such in the report rather than
# quietly enjoying the benefit of the doubt. There are none left: A-7 used to
# be one, which made the claim at the top of this file false for it — a probe
# that did nothing at all still came back REVIEW and the run still exited 0.
#
# WHERE THE PATTERNS COME FROM. Every one is a literal line its script prints,
# and two of them are lines it prints only when the stand ALSO answered well:
# A-6's `turns that returned a response : <nonzero>` is its own count of turns
# the stand actually answered, and A-7's
# `test-a7-control 200 answered` needs the control agent to have replied.
#
# A-6's witness used to be `DETERMINABLE: YES`, which was a tautology: the
# script's `judge_selftest` prints that exact string for a hand-made body before
# it has touched the stand at all (A-6.sh:272), so the pattern was satisfied by
# a probe that never made a request. The summary count cannot be reached that
# way — it is printed once, at the end, from turns that came back. A
# stand that takes the request and then answers badly therefore reads here as
# "no evidence the probe did its work", when the truer sentence is "the probe
# worked and the stand did not". That is a false alarm, never a false pass, and
# it is what grounding a witness in the script's own output costs.
# Whether a RECORDED run also shows it is a separate question, and not one to
# answer in a comment that can rot: the report computes it at the end of every
# run by grepping baseline/, after*/ and evidence-run*/live-<ID>.log, and it
# prints which witnesses a recorded run confirms and which rest on the script
# source alone. Some baselines here are written-up recon documents rather than
# captured stdout, which is how a pattern can be right and still unconfirmed;
# the report names those, so this comment does not have to and cannot be wrong.
REPROS=(
  "A-1|A-1.sh|manual|-|cheap|^agent_id: [0-9a-f]{8}|PUT /api/agents/{id}/update was a 200-returning no-op (fixes b86e65b, 6cb20be)"
  "A-2|A-2.sh|a2|GREEN|cheap|\"status\": \"added\"&&\"status\": \"removed\"|remove_custom_model did not recompute provider.model_count (fix 5891b2b)"
  "A-4|A-4.sh|manual|-|costly|^agent_id: +[0-9a-f]{8}|'## Current Date' and other service prompt sections leaked into agent output (fix cdd70de)"
  "A-6|A-6.sh|manual|-|costly|turns that returned a response *: *[1-9]|a fallback-served turn never disclosed which model actually answered (fix cbb0660)"
  "A-7|A-7.sh|manual|-|costly|^--- spawn: HTTP 201 +agent_id=[0-9a-f]{8}&&^test-a7-control +200 +answered|manifest [[fallback_models]] inherited [default_model].base_url (fix 366d62f)"
  "FANG-31|FANG-31.sh|result|RED|cheap|poller attached, long-poll in flight|Telegram 409: channel reload abandons an in-flight getUpdates (NO FIX ON ours)"
  "FANG-43|FANG-43.sh|result|GREEN|costly|^--- session file: |the Telegram bot token reached the LLM prompt and the on-disk session (fix acc85d7)"
  "FANG-45|FANG-45.sh|manual|-|costly|^session file: |file_read truncated silently with no way to page the rest (fix 8b502f9)"
  "FANG-9|FANG-9.sh|exit4|RED|cheap|^probe agent: fangrig-probe / [0-9a-f]{8}|agent reports a write it never performed; the phantom-action guard covers channels only (NO FIX)"
  "FANG-10|FANG-10.sh|exit4|RED|cheap|^probe agent: fangrig-probe / [0-9a-f]{8}|max_iterations exceeded -> HTTP 500 carrying none of the turn's work (NO FIX)"
  "FANG-13|FANG-13.sh|exit4|RED|cheap|^probe agent: fangrig-probe / [0-9a-f]{8}|an empty-but-valid provider response is returned as a successful turn (NO FIX)"
  "FANG-47|FANG-47.sh|exit4|RED|cheap|^probe agent: fangrig-probe / [0-9a-f]{8}|the max-iterations exit discards the whole turn's accounting (NO FIX)"
)

# Signatures of a probe that never got into the stand. Deliberately narrow:
# they match the harness's OWN clients complaining (ofctl's stderr line, the
# `HTTP <code>` line the FANG-* probes print for their own API calls, curl's
# transport errors), NOT any mention of a status code in passing. A-7, for
# one, legitimately reports a PROVIDER's 401 inside an agent's answer; that
# must not disqualify anything.
#
# Product-level 4xx/5xx are NOT here on purpose: A-1 expects HTTP 501 from
# PUT /update, FANG-10 expects HTTP 500 from the max-iterations exit. Those
# are the findings, not failures to reach the system.
DISQUALIFY_RE='Invalid API key|Missing Authorization|ofctl: HTTP 40[13] |ofctl: no response from|ofctl: cannot read|^HTTP 40[13]([^0-9]|$)|^ *HTTP 40[13]([^0-9]|$)|curl: \([0-9]+\)|Connection refused|Failed to connect to|Could not resolve host'

field() { printf '%s' "$1" | cut -d'|' -f"$2"; }

ALL_IDS=()
for r in "${REPROS[@]}"; do ALL_IDS+=("$(field "$r" 1)"); done

# in_list NEEDLE HAYSTACK... — exact string membership. Not `grep "\b$id"`:
# ids share prefixes and `-` is a word boundary, so a pattern match would let
# a near-miss select a real repro, which is the opposite of what --only is for.
in_list() {
  local needle="$1"; shift
  local x
  for x in "$@"; do [ "$x" = "$needle" ] && return 0; done
  return 1
}

if [ "$LIST_ONLY" = 1 ]; then
  printf '%-8s %-13s %-7s %-7s %-7s %-9s %s\n' ID SCRIPT MODE EXPECT COST WITNESS COVERS
  for r in "${REPROS[@]}"; do
    w="$(field "$r" 6)"; [ "$w" = "-" ] && w="none" || w="yes"
    printf '%-8s %-13s %-7s %-7s %-7s %-9s %s\n' \
      "$(field "$r" 1)" "$(field "$r" 2)" "$(field "$r" 3)" \
      "$(field "$r" 4)" "$(field "$r" 5)" "$w" "$(field "$r" 7)"
  done
  exit 0
fi

# ------------------------------------------------------------- --only sanity --
# A misspelt id used to select nothing, and selecting nothing used to look
# exactly like an honest run in which every repro was skipped. It is a usage
# error: say which id is wrong, list the real ones, and refuse to run.
#
# `--only ''` is the same usage error. It used to be indistinguishable from
# --only never being passed, i.e. it quietly ran all twelve — the exact
# outcome ("selecting nothing looks like an honest run") this block exists to
# prevent, reached through the flag itself.
SELECTED=()
if [ "$ONLY_GIVEN" = 1 ]; then
  # ',,' and '  ' select nothing just as '' does.
  if [ -z "${ONLY//[, ]/}" ]; then
    echo "run.sh: --only was given an empty selection ('$ONLY')." >&2
    echo "run.sh: to run everything, pass no --only at all." >&2
    echo "run.sh: known ids: ${ALL_IDS[*]}" >&2
    exit 2
  fi
  IFS=',' read -ra want <<<"$ONLY"
  bad=""
  for w in "${want[@]}"; do
    [ -n "$w" ] || continue
    if in_list "$w" "${ALL_IDS[@]}"; then
      SELECTED+=("$w")
    else
      bad="$bad $w"
    fi
  done
  if [ -n "$bad" ]; then
    echo "run.sh: --only names no such reproduction:$bad" >&2
    echo "run.sh: known ids: ${ALL_IDS[*]}" >&2
    exit 2
  fi
  [ "${#SELECTED[@]}" -gt 0 ] || { echo "run.sh: --only was given but selected nothing" >&2; exit 2; }
fi

# -------------------------------------------------------------- prod guard --
case "$BASE_URL" in
  *:4200*) echo "REFUSING: BASE_URL=$BASE_URL looks like production. Staging is :4201." >&2; exit 3 ;;
esac
if [ "$CONTAINER" = "openfang-openfang-1" ]; then
  echo "REFUSING: OF_CONTAINER=$CONTAINER is the production container." >&2
  exit 3
fi

# --------------------------------------------------------------- staging? --
STAGING_UP=0
STAGING_WHY="ok"
if ! command -v docker >/dev/null 2>&1; then
  STAGING_WHY="docker not on PATH"
elif ! docker inspect "$CONTAINER" >/dev/null 2>&1; then
  STAGING_WHY="container '$CONTAINER' does not exist"
elif [ "$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null)" != "true" ]; then
  STAGING_WHY="container '$CONTAINER' is not running"
elif ! python3 - "$BASE_URL" <<'PY'
import sys, urllib.request
try:
    urllib.request.urlopen(sys.argv[1] + "/api/health", timeout=5)
except Exception:
    sys.exit(1)
PY
then
  STAGING_WHY="API at $BASE_URL did not answer /api/health"
else
  STAGING_UP=1
fi

# ------------------------------------------------------------ ONE TARGET --
# The stand is a URL, a container, a data directory and a key, and every one
# of them has to name the SAME stand. They used not to. `--base-url` reached
# the five A-*.sh (which `export OPENFANG_URL`) and the report header, and
# stopped there: FANG-9/10/13/47 never exported it, so harness/lib.sh:16 fell
# back to its own default and fangrig edited the config.toml of, and spawned
# agents on, whatever stand happened to sit on :4201. The header named one
# machine, the work happened on another.
#
# Resolving it all from the container was half the answer, and the dangerous
# half on its own: it made the data directory agree with --container while
# every id and verdict still came from --base-url, so the two names could
# still point at different machines — and the probes deleted files at the path
# derived from the loser. Match them first.
#
# THE MATCH, and its exact reach. A host port is exclusive: on the machine
# that publishes it, one container holds it at a time. So "this container
# publishes 127.0.0.1:4211/tcp, and the URL asks THIS machine for 4211" says
# who answers the URL. Three things have to hold for that sentence to be true,
# and all three used to be waved through:
#
#   * the publication is TCP. `-p 127.0.0.1:4218:4200/udp` used to satisfy a
#     TCP URL — a real, measured cross-stand delete (see the header).
#   * the address covers the URL's host: a container on 127.0.0.1:P does not
#     answer for http://10.0.0.5:P; only a wildcard bind (0.0.0.0 / ::) covers
#     every address OF THIS MACHINE.
#   * the URL's host IS this machine. Docker's publication table is a fact
#     about the local host and nothing else, so against a URL that resolves
#     somewhere else it is not weak evidence, it is no evidence.
local_addrs() { ip -o addr show 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | sort -u; }

url_facts() { # URL -> "host port is_local resolved_ips"
  python3 - "$1" "$(local_addrs | tr '\n' ' ')" <<'PY'
import ipaddress, socket, sys
from urllib.parse import urlsplit
u = urlsplit(sys.argv[1])
host = (u.hostname or "").lower()
port = u.port or (443 if u.scheme == "https" else 80)
mine = set(sys.argv[2].split())
ips = set()
try:
    for info in socket.getaddrinfo(host, port, proto=socket.IPPROTO_TCP):
        ips.add(info[4][0].split("%")[0])
except Exception:
    pass
def local(ip):
    try:
        return ipaddress.ip_address(ip).is_loopback or ip in mine
    except ValueError:
        return False
# every address this name can reach has to be one of ours: if one of them is
# somewhere else, the request can land there, and then nothing local is known.
is_local = 1 if ips and all(local(i) for i in ips) else 0
print("%s %s %d %s" % (host, port, is_local, ",".join(sorted(ips)) or "-"))
PY
}

BIND_STATE=unchecked
BIND_WHY="the pair was never checked: $STAGING_WHY"
URL_HOST=""; URL_PORT=""; URL_LOCAL=0; URL_IPS="-"
if [ "$STAGING_UP" = 1 ]; then
  read -r URL_HOST URL_PORT URL_LOCAL URL_IPS <<<"$(url_facts "$BASE_URL")"
  PUBLISHED="$(docker inspect \
      -f '{{range $p, $conf := .NetworkSettings.Ports}}{{range $conf}}{{$p}}|{{.HostIp}}|{{.HostPort}} {{end}}{{end}}' \
      "$CONTAINER" 2>/dev/null)"
  # "4200/udp|127.0.0.1|4218" reads as "127.0.0.1:4218/udp" to a human.
  PUB_HUMAN="$(printf '%s' "$PUBLISHED" | sed 's#[0-9]*/\([a-z]*\)|\([^|]*\)|\([0-9]*\)#\2:\3/\1#g' | sed 's/ *$//')"
  BIND_STATE=nomatch
  BIND_WHY="'$CONTAINER' publishes [${PUB_HUMAN:-nothing}], and none of that is a TCP publication of port $URL_PORT on an address $BASE_URL asks for"
  for entry in $PUBLISHED; do
    proto="${entry%%|*}"; rest="${entry#*|}"
    hip="${rest%%|*}"; hport="${rest##*|}"
    case "$proto" in */tcp) ;; *) continue ;; esac
    [ "$hport" = "$URL_PORT" ] || continue
    covers=0
    case "$hip" in
      0.0.0.0|::|"") covers=1 ;;
      *) case ",$URL_IPS," in *",$hip,"*) covers=1 ;; esac ;;
    esac
    [ "$covers" = 1 ] || continue
    if [ "$URL_LOCAL" = 1 ]; then
      BIND_STATE=matched
      BIND_WHY="'$CONTAINER' publishes $hip:$hport/tcp and $BASE_URL asks this machine for port $hport; a published host port belongs to one container at a time. Nothing else was compared."
    else
      BIND_STATE=notlocal
      BIND_WHY="'$CONTAINER' publishes $hip:$hport/tcp, but $BASE_URL names host $URL_HOST (resolved: $URL_IPS), which is not an address of this machine — so what docker publishes here says nothing about which stand answers that URL"
    fi
    break
  done
fi

# Refuse before a single variable is exported: an unestablished pair is how a
# run against one stand deleted files on another.
if [ "$STAGING_UP" = 1 ] && [ "$BIND_STATE" != matched ]; then
  echo "REFUSING: this run cannot establish that --container and --base-url" >&2
  echo "name the same stand." >&2
  echo "  base url : $BASE_URL" >&2
  echo "  container: $CONTAINER" >&2
  echo "  $BIND_WHY" >&2
  echo >&2
  echo "  The data directory below is not just read: it goes out as" >&2
  echo "  OPENFANG_VOLUME / OPENFANG_HOME_HOST / OPENFANG_CONFIG, which is how" >&2
  echo "  A-1, A-6, A-7 and fangrig are told they may rm -rf under it and" >&2
  echo "  rewrite the config.toml in it — while every agent id they act on" >&2
  echo "  comes from the URL. Handing that out on a pair nobody could match is" >&2
  echo "  how this runner once deleted files on a stand it never addressed." >&2
  echo "  Nothing was run and nothing was touched." >&2
  exit 2
fi

# Only now, and only from the matched container: its /data mount IS the data
# directory. There is deliberately no fallback — the old one defaulted to the
# shared staging volume, which is a different stand's data whenever the
# container is not staging.
DATA_DIR=""
DATA_DIR_SRC="not resolved"
if [ "$BIND_STATE" = matched ]; then
  DATA_DIR="$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$CONTAINER" 2>/dev/null)"
  if [ -n "$DATA_DIR" ]; then
    DATA_DIR_SRC="$CONTAINER:/data"
  else
    echo "REFUSING: '$CONTAINER' serves $BASE_URL but has nothing mounted at /data," >&2
    echo "  so this run cannot say where its data directory is. Every probe here" >&2
    echo "  reads or writes it. Guessing a volume is how files get deleted on the" >&2
    echo "  wrong stand; nothing was run and nothing was touched." >&2
    exit 2
  fi
fi
CONFIG=""
[ -n "$DATA_DIR" ] && CONFIG="$DATA_DIR/config.toml"

# The key: the environment wins, exactly as ofctl resolves it, so that
# `OPENFANG_API_KEY=<rubbish> ./run.sh` is a question this runner can be
# asked and must answer honestly.
KEY_SRC=""
API_KEY=""
if [ -n "${OPENFANG_API_KEY:-}" ]; then
  API_KEY="$OPENFANG_API_KEY"; KEY_SRC='$OPENFANG_API_KEY (environment)'
elif [ -n "$CONFIG" ]; then
  API_KEY="$(sed -n 's/^api_key *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)"
  KEY_SRC="$CONFIG"
else
  KEY_SRC="nowhere — the target was never resolved"
fi

export OPENFANG_URL="$BASE_URL"
export OF_CONTAINER="$CONTAINER"
export OPENFANG_API_KEY="$API_KEY"
# The path exports go out ONLY when a matched container produced them. Left
# unset otherwise: each probe defaults an unset OPENFANG_VOLUME to the shared
# staging volume, so exporting an empty string here would aim their own
# `rm -rf` at a stand nobody named. Nothing runs in that state anyway.
if [ -n "$DATA_DIR" ]; then
  export OPENFANG_CONFIG="$CONFIG"
  export OF_CONFIG="$CONFIG"
  export OPENFANG_VOLUME="$DATA_DIR"       # A-1.sh
  export OPENFANG_HOME_HOST="$DATA_DIR"    # A-4.sh / A-6.sh / A-7.sh
fi

api_status() { # api_status METHOD PATH KEY -> http code
  local auth=()
  [ -n "${3:-}" ] && auth=(-H "Authorization: Bearer $3")
  curl -sS -o /dev/null -w '%{http_code}' -m 20 -X "$1" \
       "${auth[@]}" "$BASE_URL$2" 2>/dev/null
}
AUTH_HDR=()
[ -n "$API_KEY" ] && AUTH_HDR=(-H "Authorization: Bearer $API_KEY")

# reads /api/agents, prints "id name" for probe-shaped names only.
# Defined here rather than in the clean-up block because the sweep needs to
# compare against a reading taken BEFORE anything ran.
probe_names() {
  curl -sS -m 30 "${AUTH_HDR[@]}" "$BASE_URL/api/agents" 2>/dev/null | python3 -c '
import json, re, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit()
agents = d if isinstance(d, list) else d.get("agents", [])
pat = re.compile(r"^(test-a[0-9]+[a-z0-9-]*|fangrig-[a-z0-9-]+)$")
for a in agents:
    if isinstance(a, dict) and pat.match(str(a.get("name", ""))):
        print("%s %s" % (a.get("id"), a.get("name")))
' 2>/dev/null
}

# WHAT THIS RUN IS ALLOWED TO DELETE.
#
# The sweep used to delete every probe-shaped name it found. On a stand shared
# with a parallel worker that is someone else's live agent: `fangrig-*` is the
# name a run in flight is using right now. Matching a shape is not evidence that
# this run created the thing.
#
# So take the reading before anything runs. Anything already standing here is
# not ours, whatever it is called, and the sweep leaves it alone and says so.
#
# That covers the sweep and nothing else. The probes run first and do their own
# shape-matching: A-1.sh deletes any agent named test-a1-probe before creating
# its own, `fangrig down` uninstalls every fangrig-*. This runner cannot stop
# that from here, so it does the one thing left — the report names the
# pre-existing agents that are gone by the end, instead of letting them vanish
# quietly between two lines that both say "left alone".
# AND IT HAS TO FAIL CLOSED.
#
# `probe_names` swallows every error into an empty list: a timeout, a 500, a
# body that is not JSON all look exactly like "the stand is clean". An empty
# reading here would mark every agent found later as this run's own and delete
# other people's work — the same failure the reading was added to prevent, just
# reached through a network blip instead of a name pattern.
#
# So take the reading with the status attached, and if it cannot be taken, do
# not sweep at all. Not sweeping costs the next run a stale agent. Sweeping on
# an empty reading costs someone else theirs.
PRE_PROBE_OK=0
PRE_PROBE_LIST=""
_pp_raw="$(curl -sS -m 30 -w '\n%{http_code}' "${AUTH_HDR[@]}" "$BASE_URL/api/agents" 2>/dev/null)"
_pp_code="$(printf '%s' "$_pp_raw" | tail -n1)"
_pp_body="$(printf '%s' "$_pp_raw" | sed '$d')"
case "$_pp_code" in
  2*)
    PRE_PROBE_LIST="$(printf '%s' "$_pp_body" | python3 -c '
import json, re, sys
d = json.load(sys.stdin)                      # no try/except: a parse error must
agents = d if isinstance(d, list) else d.get("agents", [])   # fail, not read as
pat = re.compile(r"^(test-a[0-9]+[a-z0-9-]*|fangrig-[a-z0-9-]+)$")   # "empty"
for a in agents:
    if isinstance(a, dict) and pat.match(str(a.get("name", ""))):
        print("%s %s" % (a.get("id"), a.get("name")))
' 2>/dev/null)" && PRE_PROBE_OK=1
    ;;
esac
if [ "$PRE_PROBE_OK" != 1 ]; then
  PRE_PROBE_WHY="GET \$BASE_URL/api/agents answered ${_pp_code:-no response} before the run"
else
  PRE_PROBE_WHY=""
fi
unset _pp_raw _pp_code _pp_body
PRE_PROBE_IDS=" $(printf '%s' "$PRE_PROBE_LIST" | awk '{print $1}' | tr '\n' ' ') "

# ---------------------------------------------------------------- AUTH GATE --
# Prove the credential before trusting anything measured with it.
#
# Method: find a route that actually discriminates — one that answers 401/403
# to a key we know is wrong. (Most GETs on this build are unauthenticated, so
# "our key got a 200" proves nothing on its own; it has to be a route that
# would have said no.) Then ask the same route with the real key. Read-only,
# no side effects, works on v0.6.9-pristine and on `ours`.
AUTH_STATE=unchecked
AUTH_WHY="not run (staging down)"
AUTH_ROUTE=""
if [ "$STAGING_UP" = 1 ]; then
  BOGUS="of-run-sh-preflight-deliberately-invalid-key"
  AUTH_STATE=undecidable
  AUTH_WHY="no route on this build rejects a known-bad key, so the credential cannot be tested"
  for route in /api/usage/by-model /api/usage /api/models/custom; do
    bad="$(api_status GET "$route" "$BOGUS")"
    case "$bad" in
      401|403)
        AUTH_ROUTE="$route"
        good="$(api_status GET "$route" "$API_KEY")"
        # Only an answer counts as acceptance. "Not a 401" is not the same
        # thing as "yes": a 404 says the route moved, a 500 says the stand
        # broke before it got round to the credential, and both used to be
        # read here as ACCEPTED and printed as such in the header.
        case "$good" in
          2??)     AUTH_STATE=accepted
                   AUTH_WHY="GET $route: bad key -> $bad, this key -> $good" ;;
          401|403) AUTH_STATE=rejected
                   AUTH_WHY="$BASE_URL answered HTTP $good to GET $route with the key from $KEY_SRC" ;;
          000|"")  AUTH_STATE=undecidable
                   AUTH_WHY="GET $route gave no answer at all with the real key" ;;
          *)       AUTH_STATE=undecidable
                   AUTH_WHY="GET $route answered HTTP $good to this key (bad key -> $bad): neither an acceptance nor a rejection, so the credential is unproven" ;;
        esac
        break ;;
    esac
  done
fi

TS="$(date -u '+%Y%m%d-%H%M%S')"
[ -n "$OUT_DIR" ] || OUT_DIR="/tmp/fang-run/$TS"
mkdir -p "$OUT_DIR" || { echo "cannot create $OUT_DIR" >&2; exit 2; }

echo "======================================================================"
echo "tests/fang/run.sh — $TS"
echo "target      : $BASE_URL   container: $CONTAINER"
case "$BIND_STATE" in
  matched) echo "same stand? : yes — $BIND_WHY" ;;
  *)       echo "same stand? : not established — $BIND_WHY" ;;
esac
echo "data dir    : ${DATA_DIR:-(none)}   (from $DATA_DIR_SRC)"
echo "config      : ${CONFIG:-(none)}"
# Length only, never the key itself.
echo "api key     : $([ -n "$API_KEY" ] && printf '%s chars' "${#API_KEY}" || printf '(none)') from $KEY_SRC"
if [ "$STAGING_UP" = 1 ]; then
  echo "staging     : up (image $(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null))"
else
  echo "staging     : DOWN — $STAGING_WHY"
  echo "              every repro below needs it; they will be SKIPPED, not passed."
fi
case "$AUTH_STATE" in
  accepted)    echo "credential  : ACCEPTED — $AUTH_WHY" ;;
  rejected)    echo "credential  : REJECTED — $AUTH_WHY" ;;
  undecidable) echo "credential  : UNVERIFIED — $AUTH_WHY" ;;
  *)           echo "credential  : not checked — $AUTH_WHY" ;;
esac
echo "costly gate : $([ "$RUN_COSTLY" = 1 ] && echo "OPEN (--costly): repros that bill real providers WILL run" \
                                            || echo "closed: repros that bill real providers are SKIPPED (pass --costly)")"
echo "logs        : $OUT_DIR"
echo "======================================================================"
echo

N_PASS=0; N_FAIL=0; N_SKIP=0; N_REVIEW=0; N_ERROR=0
SUMMARY=()

# ------------------------------------------------------- preflight refusal --
# A rejected credential is not a reason to run the probes and read their
# tea leaves; it is a reason not to grade anything they say. Say so once, per
# selected repro, and fail.
if [ "$AUTH_STATE" = rejected ]; then
  echo "PREFLIGHT FAILED — the stand rejects the key the probes would use."
  echo "  $AUTH_WHY"
  echo
  echo "Nothing below was run. Every probe here measures a difference it makes"
  echo "itself; with no credential it makes no difference, measures zero, and"
  echo "zero reads as 'no defect'. That is the false green this gate exists to"
  echo "stop, so these are INCONCLUSIVE and not one of them is a pass."
  echo
fi

for r in "${REPROS[@]}"; do
  id="$(field "$r" 1)"; script="$(field "$r" 2)"; mode="$(field "$r" 3)"
  expect="$(field "$r" 4)"; cost="$(field "$r" 5)"; witness="$(field "$r" 6)"
  covers="$(field "$r" 7)"

  if [ "$ONLY_GIVEN" = 1 ] && ! in_list "$id" "${SELECTED[@]}"; then
    continue
  fi

  if [ "$AUTH_STATE" = rejected ]; then
    printf '%-8s INCONCLUSIVE  credential rejected by the target — not run\n' "$id"
    SUMMARY+=("INCONCLUSIVE|$id|not run: the target rejected the api_key from $KEY_SRC|$covers")
    N_FAIL=$((N_FAIL + 1))
    continue
  fi

  log="$OUT_DIR/$id.log"
  path="$HERE/$script"

  # ---- gates, each with its own reason -----------------------------------
  reason=""
  if [ ! -x "$path" ]; then
    reason="no executable $script in $HERE"
  elif [ "$STAGING_UP" != 1 ]; then
    reason="staging is down: $STAGING_WHY"
  elif [ "$cost" = costly ] && [ "$RUN_COSTLY" != 1 ]; then
    reason="makes real, billed LLM calls against live providers; pass --costly to run it"
  fi
  if [ -n "$reason" ]; then
    printf '%-8s SKIPPED   %s\n' "$id" "$reason"
    SUMMARY+=("SKIPPED|$id|$reason|$covers")
    N_SKIP=$((N_SKIP + 1))
    continue
  fi

  # ---- run it ------------------------------------------------------------
  # Protocol note: NOT a pipeline. `cmd | tee` would hand us tee's exit code,
  # and every verdict in `exit4`/`a2` mode lives in cmd's. Redirect to a file,
  # capture $?, and grep the file afterwards as a separate statement.
  #
  # </dev/null is not decoration either: these scripts reach into the staging
  # container with `docker exec -i`, which forwards whatever stdin it was
  # given. Handed a terminal or an open pipe, a bodyless request inside can
  # sit on stdin.read() forever — a silent hang with no output at all. Give
  # every repro a stdin that is already at EOF.
  printf '%-8s running   (%s, mode=%s, expect=%s) ... ' "$id" "$script" "$mode" "$expect"
  start=$(date +%s)
  timeout --signal=TERM --kill-after=30 "$TIMEOUT_SECS" "$path" "$BASE_URL" >"$log" 2>&1 </dev/null
  rc=$?
  took=$(( $(date +%s) - start ))

  # ---- read the verdict --------------------------------------------------
  verdict=""; note=""
  if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
    verdict=ERROR; note="timed out after ${TIMEOUT_SECS}s"
  else
    case "$mode" in
      exit4)
        case "$rc" in
          0) verdict=RED ;;
          1) verdict=GREEN ;;
          2) verdict=REFUSED; note="script refused the target" ;;
          3) verdict=SKIPPED; note="$(grep -m1 '^SKIPPED' "$log" 2>/dev/null || echo 'script reported missing prerequisites')" ;;
          4) verdict=INCONCLUSIVE; note="the script's own control failed" ;;
          *) verdict=ERROR; note="exit $rc is not part of the exit4 convention" ;;
        esac
        ;;
      a2)
        # A-2.sh's own convention, now with the two codes every other repro
        # here already had: it can say "I could not do my work" instead of
        # being forced to call a 0−0 subtraction a green.
        case "$rc" in
          0) verdict=RED ;;
          1) verdict=GREEN ;;
          3) verdict=SKIPPED; note="$(grep -m1 '^SKIPPED' "$log" 2>/dev/null || echo 'script reported missing prerequisites')" ;;
          4) verdict=INCONCLUSIVE; note="the script's own control failed" ;;
          *) verdict=ERROR; note="exit $rc; A-2.sh means 0=RED 1=GREEN 3=SKIPPED 4=INCONCLUSIVE" ;;
        esac
        ;;
      result)
        line="$(grep -E '^RESULT: (RED|GREEN|INCONCLUSIVE)' "$log" 2>/dev/null | tail -1)"
        if [ -z "$line" ]; then
          verdict=ERROR; note="no 'RESULT:' line in the output (exit was $rc)"
        else
          verdict="$(printf '%s' "$line" | awk '{print $2}')"
          note="exit code was $rc and is not used in this mode"
        fi
        ;;
      manual)
        verdict=REVIEW
        note="no machine-readable verdict; read $log against the red criterion in the script header (exit $rc)"
        ;;
      *)
        verdict=ERROR; note="unknown mode '$mode' in the registry"
        ;;
    esac
  fi

  # ---- authenticity screen ------------------------------------------------
  # Applied to every GRADE, in every mode, inherited scripts included. It can
  # only downgrade.
  #
  # SKIPPED is exempt, and that is not a hole: a skip is a probe declining to
  # run and saying why. Of course its log carries no witness — it did no work,
  # which is precisely what it reported. Screening it would overwrite the one
  # useful thing it said with "no evidence the probe did its work" and turn an
  # all-skipped run's exit 4 ("nothing ran, this proves nothing") into exit 1
  # ("something failed"). Neither is true, and SKIPPED was never a pass.
  case "$verdict" in
    ERROR|REFUSED|INCONCLUSIVE|SKIPPED) ;;   # already not a pass; nothing to demote
    *)
      hit="$(grep -m1 -E "$DISQUALIFY_RE" "$log" 2>/dev/null)"
      if [ -n "$hit" ]; then
        note="the probe could not reach or could not enter the stand: $(printf '%s' "$hit" | cut -c1-110)"
        verdict=INCONCLUSIVE
      elif [ "$witness" != "-" ]; then
        # `&&`-joined patterns: every one of them has to be there, and every
        # one that is not gets named. Keeping only the last miss told the
        # operator to go looking for one thing when two were absent.
        missing=""; n_missing=0
        rest="$witness"
        while [ -n "$rest" ]; do
          case "$rest" in
            *"&&"*) pat="${rest%%&&*}"; rest="${rest#*&&}" ;;
            *)      pat="$rest"; rest="" ;;
          esac
          if ! grep -qE -- "$pat" "$log" 2>/dev/null; then
            missing="${missing:+$missing and }/$pat/"
            n_missing=$((n_missing + 1))
          fi
        done
        if [ "$n_missing" -gt 0 ]; then
          note="no evidence the probe did its work: nothing in the log matches $missing"
          verdict=INCONCLUSIVE
        fi
      fi
      ;;
  esac

  # ---- grade it ----------------------------------------------------------
  case "$verdict" in
    REVIEW)       outcome=REVIEW;  N_REVIEW=$((N_REVIEW + 1)) ;;
    ERROR)        outcome=ERROR;   N_ERROR=$((N_ERROR + 1)) ;;
    REFUSED)      outcome=ERROR;   N_ERROR=$((N_ERROR + 1)) ;;
    SKIPPED)      outcome=SKIPPED; N_SKIP=$((N_SKIP + 1)) ;;
    INCONCLUSIVE) outcome=INCONCLUSIVE; N_FAIL=$((N_FAIL + 1)) ;;
    "$expect")    outcome=PASS;    N_PASS=$((N_PASS + 1)) ;;
    *)            outcome=FAIL;    N_FAIL=$((N_FAIL + 1))
                  note="got $verdict, expected $expect${note:+ ($note)}" ;;
  esac

  printf '%s (%ss)\n' "$outcome" "$took"
  [ -n "$note" ] && printf '         %s\n' "$note"
  # SUMMARY rows are split on '|' when printed. A note quoting a log line can
  # contain one (a regex alternation, a table rule), and that used to lop the
  # detail off in the very table a reader trusts. Neutralise it here.
  SUMMARY+=("$outcome|$id|$(printf '%s' "${verdict}${note:+ — $note}" | tr '|' '/')|$covers")
done

# ------------------------------------------------------------------ report --
echo
echo "======================================================================"
echo "RESULTS"
echo "======================================================================"
printf '%-14s %-8s %s\n' OUTCOME ID DETAIL
for s in "${SUMMARY[@]}"; do
  printf '%-14s %-8s %s\n' "$(field "$s" 1)" "$(field "$s" 2)" "$(field "$s" 3)"
done
echo
echo "PASS=$N_PASS  FAIL=$N_FAIL  REVIEW=$N_REVIEW  SKIPPED=$N_SKIP  ERROR=$N_ERROR"
if [ "$N_REVIEW" -gt 0 ]; then
  echo
  echo "REVIEW is not PASS. Those scripts measure; the verdict is a human's."
fi
if [ "$N_SKIP" -gt 0 ]; then
  echo "SKIPPED is not PASS either. Each skip above carries its reason."
fi

# --------------------------------------------------- authenticity coverage --
echo
echo "======================================================================"
echo "AUTHENTICITY — what each repro had to show before it was graded"
echo "======================================================================"
echo "credential: $AUTH_STATE${AUTH_ROUTE:+ (tested on $AUTH_ROUTE)} — $AUTH_WHY"
NOWIT=""
for r in "${REPROS[@]}"; do
  [ "$(field "$r" 6)" = "-" ] && NOWIT="$NOWIT $(field "$r" 1)"
done
if [ -n "$NOWIT" ]; then
  echo "repros with NO witness pattern — their verdicts rest on the credential"
  echo "check and the disqualifier scan alone:$NOWIT"
else
  echo "every repro in the registry carries a witness pattern."
fi
echo
# Provenance, computed rather than claimed. A witness is a line its script
# prints; whether a run recorded on disk ALSO shows it is checked here, every
# run, against baseline/ and after*/ — so this cannot drift out of date the
# way a sentence in a comment can. A pattern no recorded run confirms is not
# wrong, but it has never been seen to fire, and the reader is told which.
CONFIRMED=""; UNCONFIRMED=""
for r in "${REPROS[@]}"; do
  wid="$(field "$r" 1)"; wpat="$(field "$r" 6)"
  [ "$wpat" = "-" ] && continue
  seen=0
  for f in "$HERE/baseline/$wid.txt" "$HERE/after/$wid.txt" "$HERE/after-v2/$wid.txt" \
           "$HERE"/evidence-run*/live-"$wid".log; do
    [ -f "$f" ] || continue
    all=1; wrest="$wpat"
    while [ -n "$wrest" ]; do
      case "$wrest" in
        *"&&"*) wp="${wrest%%&&*}"; wrest="${wrest#*&&}" ;;
        *)      wp="$wrest"; wrest="" ;;
      esac
      grep -qE -- "$wp" "$f" 2>/dev/null || all=0
    done
    [ "$all" = 1 ] && { seen=1; break; }
  done
  if [ "$seen" = 1 ]; then CONFIRMED="$CONFIRMED $wid"; else UNCONFIRMED="$UNCONFIRMED $wid"; fi
done
echo "witnesses confirmed by a run recorded on disk :$CONFIRMED"
if [ -n "$UNCONFIRMED" ]; then
  echo "witnesses no recorded run confirms            :$UNCONFIRMED"
  echo "  (the pattern is a line the script prints, but nothing captured on disk"
  echo "   shows it — usually because the file under baseline/ for that repro is"
  echo "   a written-up recon document rather than the script's stdout. Not"
  echo "   wrong; never yet seen to fire. Capture one as evidence-run*/live-<ID>.log"
  echo "   and this line will shorten by itself.)"
fi

# ---------------------------------------------------------- probe clean-up --
# A-1 ends with "probe agent left in place"; fangrig removes its own agents in
# `down`, but only if the script got that far. A run that leaves test agents
# behind changes the stand for the next run, so sweep, and — whether anything
# was found or not — say what the stand looks like afterwards.
#
# THE SWEEP DOES NOT TOUCH THE FILESYSTEM. It deletes through the API, at
# $BASE_URL — the same channel it read the ids from, and the only one that
# cannot address a machine nobody named. It used to follow each DELETE with
# `rm -rf "$DATA_DIR/agents/$name"`, a path built from --container, on a name
# read from --base-url; with the two pointing at different stands that is an
# rm -rf, as root, on a stand this run never sent a request to. The pairing
# check above would now stop that particular mismatch — and the rm is still
# gone, because a sweep that cannot construct a path cannot construct a wrong
# one, whatever the check does or does not catch. Whatever the API leaves on disk is listed by name below and left
# where it is. A named leftover costs the next run a stale directory; the
# alternative cost someone else's data.
echo
echo "======================================================================"
echo "CLEAN-UP — probe entities on $BASE_URL"
echo "======================================================================"
if [ "$KEEP_PROBES" = 1 ]; then
  echo "--keep-probes: left in place on purpose. Whatever the probes created is"
  echo "still on the stand; the next run starts from it."
elif [ "$STAGING_UP" != 1 ]; then
  echo "staging was down; nothing to sweep."
elif [ "$AUTH_STATE" = rejected ]; then
  echo "the credential was rejected, so nothing ran and nothing could be deleted"
  echo "either. Whatever was on the stand before is still on it."
elif [ "$PRE_PROBE_OK" != 1 ]; then
  # No before-reading means no way to tell this run's agents from anyone else's.
  # Deleting on that basis is exactly the mistake the before-reading exists to
  # prevent, so the sweep stands down and says why.
  echo "  NOT SWEEPING: $PRE_PROBE_WHY,"
  echo "  so there is no way to tell which probe agents this run created and"
  echo "  which were already here. Whatever the probes made is still on the"
  echo "  stand; remove it by hand if it is in the way."
else
  # Only names this directory's probes are known to create. Nothing else is
  # touched, and the pattern is anchored so an agent merely CONTAINING the
  # word "test" is safe.
  removed=0
  probe_list="$(probe_names)"
  swept_names=""
  kept_names=""
  if [ -n "$probe_list" ]; then
    while read -r pid pname; do
      [ -n "$pid" ] || continue
      # Was it already standing here before this run? Then it is not ours.
      case "$PRE_PROBE_IDS" in
        *" $pid "*)
          echo "  left alone: $pname ($pid) — was on the stand before this run started"
          kept_names="$kept_names $pname"
          continue ;;
      esac
      code="$(curl -sS -o /dev/null -w '%{http_code}' -m 60 -X DELETE \
              "${AUTH_HDR[@]}" "$BASE_URL/api/agents/$pid" 2>/dev/null)"
      case "$code" in
        2*) echo "  removed agent $pname ($pid) via DELETE $BASE_URL/api/agents/$pid"
            removed=$((removed + 1)); swept_names="$swept_names $pname" ;;
        *)  echo "  COULD NOT remove agent $pname ($pid): HTTP $code — still on the stand"
            swept_names="$swept_names $pname" ;;
      esac
    done <<<"$probe_list"
  fi
  # A-2's custom model: its own trap normally gets it, but not if it was killed.
  mcode="$(curl -sS -o /dev/null -w '%{http_code}' -m 30 -X DELETE \
           "${AUTH_HDR[@]}" "$BASE_URL/api/models/custom/test-a2/drift-probe" 2>/dev/null)"
  case "$mcode" in
    2*) echo "  removed custom model test-a2/drift-probe"; removed=$((removed + 1)) ;;
  esac
  # Ask the stand again rather than assuming the DELETEs worked.
  leftover="$(probe_names | awk '{print $2}' | tr '\n' ' ')"
  if [ "$removed" = 0 ]; then
    echo "  nothing to remove — no probe entity was left on the stand."
  fi
  leftover="$(printf '%s' "$leftover" | sed 's/ *$//')"
  kept_names="$(printf '%s' "$kept_names" | sed 's/^ *//; s/ *$//')"
  # Anything deliberately spared is not a failure of the sweep, so say which is
  # which. Reporting a spared agent as "still present" would train the reader to
  # ignore the line that matters.
  if [ -n "$kept_names" ]; then
    echo "  NOT OURS, left standing on purpose: $kept_names"
  fi
  still=""
  for lname in $leftover; do
    case " $kept_names " in *" $lname "*) continue;; esac
    still="$still $lname"
  done
  still="$(printf '%s' "$still" | sed 's/^ *//')"
  if [ -n "$still" ]; then
    echo "  STILL PRESENT after the sweep, per $BASE_URL/api/agents: $still"
  else
    echo "  $BASE_URL/api/agents lists no probe agent of this run's making now."
  fi
  # What this sweep did NOT protect. It spares what stood here before it — but
  # the probes run first and sweep by name shape on their own: A-1.sh deletes
  # any agent called test-a1-probe before making its own, and `fangrig down`
  # uninstalls every fangrig-* it finds. Both happen long before this block
  # reads the stand, so the honest thing left is to name what went missing
  # rather than let a foreign agent disappear in silence.
  now_ids=" $(probe_names | awk '{print $1}' | tr '\n' ' ') "
  gone=""
  while read -r opid opname; do
    [ -n "$opid" ] || continue
    case "$now_ids" in *" $opid "*) continue ;; esac
    gone="$gone $opname"
  done <<<"$PRE_PROBE_LIST"
  if [ -n "$gone" ]; then
    echo "  GONE, and not by this sweep:$gone"
    echo "  Those were on the stand before this run started. The sweep spares"
    echo "  such agents; the probes themselves do not — they delete their own"
    echo "  fixed names (test-a1-probe, fangrig-*) on sight. If one of these was"
    echo "  someone else's, this run took it, and no check here can give it back."
  fi
  # And on disk, read-only: name what is still there instead of removing it.
  # Only this run's own probes: a directory belonging to the agent that was
  # already standing here is not something this run "left", and saying it was
  # is the same mistake as sweeping it.
  if [ -n "$DATA_DIR" ] && [ -d "$DATA_DIR/agents" ]; then
    on_disk=""
    for pname in $(printf '%s %s' "$swept_names" "$still" | tr ' ' '\n' | sort -u); do
      [ -d "$DATA_DIR/agents/$pname" ] && on_disk="$on_disk $pname"
    done
    if [ -n "$on_disk" ]; then
      echo "  LEFT ON DISK, not deleted, under $DATA_DIR/agents:$on_disk"
      echo "  This sweep does not remove directories. Delete them by hand if you"
      echo "  want them gone — and check the path before you do."
    else
      echo "  nothing of the probes' left under $DATA_DIR/agents."
    fi
  fi
fi

# ------------------------------------------------------- coverage of fixes --
# 14 fix commits on `ours` that are not on `main`:
#   git log --format='%h %s' ours ^main | grep '^[0-9a-f]* fix'
# The mapping fix -> repro is declared here (nothing on disk records it); the
# baseline/after columns are READ OFF THE FILESYSTEM, so a missing evidence
# file shows up as a hole instead of quietly vanishing from the table.
echo
echo "======================================================================"
echo "COVERAGE — 14 fix commits on 'ours', and what reproduces them"
echo "======================================================================"
COVERAGE=(
  "5891b2b|direct|A-2|fix(runtime): recount provider model_count on custom model removal"
  "366d62f|direct|A-7|fix(kernel): resolve fallback base_url per provider, not from default_model"
  "cdd70de|direct|A-4|fix(runtime): wrap runtime prompt data in tags, not ## headings"
  "63f9185|direct|-|fix(runtime): cap workspace context by whole file blocks"
  "b86e65b|direct|A-1|fix(api): PUT /agents/{id}/update returns 501 and names the working routes"
  "6cb20be|direct|A-1|fix(api): the 501 body was wrong about skills, mcp_servers and tools"
  "61dd183|direct|-|fix(runtime): drop a dead assignment clippy was right about"
  "bf0fee0|direct|-|fix(telegram): stop leaking the bot token into logs on network errors"
  "cbb0660|direct|A-6|fix(metering): meter usage per LLM call, not per agent turn"
  "308cd7a|direct|-|fix(kernel): report what hot-reload actually applied, not what it detected"
  "acc85d7|direct|FANG-43|fix(channels): stop token/secret leaks in Telegram file content and 6 adapters"
  "8b502f9|direct|FANG-45|fix(tools): make file_read disclose truncation and support offset/limit paging"
  "8a2f8d4|bundle|A-1,A-2,A-6,A-7|fix: four defects the adversarial review found in the sprint-2 patches"
  "d83abbf|bundle|FANG-43,FANG-45|fix: three defects the adversarial review found in the sprint-3 patches"
)
printf '%-9s %-7s %-18s %-10s %s\n' COMMIT KIND REPRO EVIDENCE SUBJECT
NO_REPRO=0; NO_GREEN=0; FULL=0; N_BUNDLE=0
for c in "${COVERAGE[@]}"; do
  sha="$(field "$c" 1)"; kind="$(field "$c" 2)"; repros="$(field "$c" 3)"; subj="$(field "$c" 4)"
  if [ "$repros" = "-" ]; then
    printf '%-9s %-7s %-18s %-10s %s\n' "$sha" "$kind" "none" "none" "$subj"
    NO_REPRO=$((NO_REPRO + 1))
    continue
  fi
  ev="" ; have_base=0; have_after=0
  IFS=',' read -ra rs <<<"$repros"
  for one in "${rs[@]}"; do
    [ -f "$HERE/baseline/$one.txt" ] && have_base=1
    { [ -f "$HERE/after/$one.txt" ] || [ -f "$HERE/after-v2/$one.txt" ]; } && have_after=1
  done
  if [ "$have_base" = 1 ] && [ "$have_after" = 1 ]; then ev="base+after"
  elif [ "$have_base" = 1 ]; then ev="base only"
  else ev="NONE"; fi
  printf '%-9s %-7s %-18s %-10s %s\n' "$sha" "$kind" "$repros" "$ev" "$subj"
  # Bundle commits amend patches that already have their own row above. They
  # inherit that coverage; counting them again would inflate the tally.
  if [ "$kind" = bundle ]; then
    N_BUNDLE=$((N_BUNDLE + 1))
  elif [ "$ev" = "base+after" ]; then FULL=$((FULL + 1))
  elif [ "$ev" = "base only" ]; then NO_GREEN=$((NO_GREEN + 1))
  else NO_REPRO=$((NO_REPRO + 1)); fi
done
echo
echo "Counted by fix commit (12 direct + 2 adversarial round-ups = 14):"
echo "  direct fixes with a baseline AND an after-the-fix run : $FULL"
echo "  direct fixes with a baseline but no green recorded    : $NO_GREEN"
echo "  direct fixes with no reproduction at all              : $NO_REPRO"
echo "  round-up commits inheriting the rows above            : $N_BUNDLE"
echo
echo "Counted by reproduction script (two commits can share one script, which"
echo "is why these numbers are smaller than the ones above):"
BR_FULL=""; BR_BASE=""; BR_NONE=""
for s in A-1 A-2 A-4 A-6 A-7 FANG-31 FANG-43 FANG-45 FANG-9 FANG-10 FANG-13 FANG-47; do
  [ -f "$HERE/$s.sh" ] || continue
  b=0; a=0
  [ -f "$HERE/baseline/$s.txt" ] && b=1
  { [ -f "$HERE/after/$s.txt" ] || [ -f "$HERE/after-v2/$s.txt" ]; } && a=1
  if   [ "$b" = 1 ] && [ "$a" = 1 ]; then BR_FULL="$BR_FULL $s"
  elif [ "$b" = 1 ];                 then BR_BASE="$BR_BASE $s"
  else                                    BR_NONE="$BR_NONE $s"; fi
done
echo "  baseline + after (a red run and a green run on record):$BR_FULL"
echo "  baseline only (red on record, no green yet)           :$BR_BASE"
echo "  no baseline on disk at all                            :$BR_NONE"
echo
echo "  Note on the 'baseline only' group: for FANG-43 and FANG-45 the missing"
echo "  green is a gap in the evidence. For FANG-9/10/13/31/47 it is not — those"
echo "  defects have no fix commit on 'ours', so red IS the current truth and"
echo "  there is nothing green to record until sprint 5."
echo
echo "Repros in this directory that are NOT tied to a fix commit — they"
echo "reproduce defects that are still open on 'ours', which is why their"
echo "expected verdict above is RED:"
for r in "${REPROS[@]}"; do
  case "$(field "$r" 7)" in
    *"NO FIX"*) printf '  %-8s %s\n' "$(field "$r" 1)" "$(field "$r" 7)" ;;
  esac
done
echo
echo "Baselines on disk:"
for f in "$HERE"/baseline/*.txt; do
  [ -f "$f" ] || continue
  printf '  %-28s %6s bytes\n' "$(basename "$f")" "$(wc -c <"$f" | tr -d ' ')"
done

echo
if [ "$AUTH_STATE" = rejected ]; then
  echo "run.sh: PREFLIGHT FAILED — the target rejected the api_key from $KEY_SRC,"
  echo "        so no verdict here is worth anything. Nothing ran; nothing passed."
  exit 2
fi
if [ "$N_ERROR" -gt 0 ]; then
  echo "run.sh: ERROR — $N_ERROR repro(s) could not produce a verdict at all."
  exit 2
fi
if [ "$N_FAIL" -gt 0 ]; then
  echo "run.sh: FAIL — $N_FAIL repro(s) did not reach the expected verdict."
  exit 1
fi
# Nothing left to fail at this point: N_FAIL and N_ERROR are both 0, so the
# graded repros are exactly the ones that passed. If there are none, this run
# checked nothing against what the build should do — whether because every
# repro was skipped or because every one of them came back REVIEW. Both are
# exit 4: a REVIEW row is a measurement nobody has read yet, and a run made of
# them must not answer 0 to `if ./run.sh; then`.
if [ "$N_PASS" -eq 0 ]; then
  echo "run.sh: NOTHING WAS GRADED — $N_SKIP skipped, $N_REVIEW awaiting a human,"
  echo "        and not one repro in this selection produced a machine-readable"
  echo "        verdict. Nothing here was checked against what this build is"
  echo "        expected to do, so this run proves nothing. Reasons are above."
  exit 4
fi
echo "run.sh: all $N_PASS graded repro(s) reached the verdict this build should produce."
if [ "$AUTH_STATE" = undecidable ]; then
  echo "        The credential could not be verified ($AUTH_WHY),"
  echo "        so anything above rests on each repro's own witness alone."
fi
echo "        ($N_SKIP skipped, $N_REVIEW awaiting a human — neither is a pass.)"
exit 0
