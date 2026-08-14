#!/usr/bin/env bash
# A-2: remove_custom_model does not recompute provider.model_count
#
# Deterministic, no LLM. Idempotent: creates its own test- model, deletes it,
# and cleans up on exit. Never touches existing custom models.
#
# Usage: A-2.sh [base_url]        (default: $OPENFANG_URL, else http://127.0.0.1:4201)
#
# Exit 0 = defect REPRODUCED (counter drift observed after DELETE)
# Exit 1 = no drift (defect absent / patched)
# Exit 4 = INCONCLUSIVE — this probe could not do its own work, so the drift it
#          measured is not evidence of anything (see THE CONTROL below).
#
# THE CONTROL, and why it is not optional
#   This script's verdict is a subtraction: model_count minus the real number
#   of models. Both terms come from the API, and both are perfectly readable
#   when nothing at all has happened — in which case the subtraction is 0−0=0
#   and 0 is exactly what "the defect is fixed" looks like. Run this against a
#   stand that rejects the api_key and it prints two HTTP 401s and then
#   "VERDICT: GREEN — counter consistent", on builds that have the fix and on
#   v0.6.9-pristine alike, which is how a runner came to report PASS for both.
#
#   So before the subtraction is allowed to mean anything, three things must
#   be true, and they are checked, not assumed:
#     * every counter reading is a number (not "NA", not an empty string left
#       behind by a failed request — bash arithmetic reads both as 0);
#     * /api/models really GREW by exactly one after the POST;
#     * and really shrank back to where it started after the DELETE.
#   If any of those fails the probe never had hold of the system: exit 4.
set -uo pipefail

BASE="${1:-${OPENFANG_URL:-http://127.0.0.1:4201}}"
export OPENFANG_URL="$BASE"
export PATH="/root/.claude/skills/openfang/scripts:$PATH"

PROVIDER="${A2_PROVIDER:-hyperfusion}"
MODEL_ID="${A2_MODEL_ID:-test-a2/drift-probe}"

say() { printf '%s\n' "$*"; }

# model_count as reported by /api/providers for $PROVIDER
counter() {
  ofctl GET /api/providers | python3 -c "
import json,sys
d=json.load(sys.stdin)
p=[x for x in d['providers'] if x['id']=='$PROVIDER']
print(p[0]['model_count'] if p else 'NA')
"
}

# actual number of models of $PROVIDER in /api/models
factual() {
  ofctl GET /api/models | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(sum(1 for m in d['models'] if m['provider']=='$PROVIDER'))
"
}

cleanup() {
  # best-effort removal of our probe model (wildcard route: slash goes literally)
  ofctl DELETE "/api/models/custom/${MODEL_ID}" >/dev/null 2>&1
}

say "== A-2 baseline probe =="
say "date:     $(date -Is)"
say "base_url: $BASE"
say "version:  $(ofctl -x version GET /api/health)"
say "provider: $PROVIDER"
say "probe id: $MODEL_ID"
say ""

cleanup   # start from a clean slate (idempotency)

C0=$(counter); F0=$(factual)
say "step 0 (initial):        providers.model_count=$C0  actual=/api/models=$F0  drift=$((C0-F0))"

say ""
say "step 1: POST /api/models/custom"
ofctl POST /api/models/custom "{\"id\":\"$MODEL_ID\",\"provider\":\"$PROVIDER\",\"display_name\":\"A-2 drift probe\"}"
C1=$(counter); F1=$(factual)
say "step 1 (after create):   providers.model_count=$C1  actual=$F1  drift=$((C1-F1))"

say ""
say "step 2: DELETE /api/models/custom/$MODEL_ID"
ofctl DELETE "/api/models/custom/${MODEL_ID}"
C2=$(counter); F2=$(factual)
say "step 2 (after delete):   providers.model_count=$C2  actual=$F2  drift=$((C2-F2))"

trap cleanup EXIT

# ------------------------------------------------------------- THE CONTROL --
say ""
say "--- control: did this probe actually change anything? ---"
is_num() { case "${1:-}" in ''|*[!0-9-]*) return 1 ;; *) return 0 ;; esac; }
BAD=""
for pair in "C0=$C0" "F0=$F0" "C1=$C1" "F1=$F1" "C2=$C2" "F2=$F2"; do
  is_num "${pair#*=}" || BAD="$BAD ${pair%%=*}=${pair#*=}"
done
if [ -n "$BAD" ]; then
  say "  reading(s) that are not a number:$BAD"
  say "  A reading like 'NA' or an empty one is what a refused or unparsable"
  say "  request leaves behind, and bash arithmetic silently calls it 0."
  say "VERDICT: INCONCLUSIVE — the counters were never readable; nothing measured."
  exit 4
fi
say "  models of $PROVIDER: before=$F0  after POST=$F1  after DELETE=$F2"
if [ "$F1" -ne $((F0 + 1)) ]; then
  say "  the POST did not add a model (expected $((F0 + 1)), got $F1)."
  say "VERDICT: INCONCLUSIVE — nothing was created, so nothing was removed, so"
  say "         the drift below is 0 for want of an experiment, not for want of"
  say "         a defect. Check the log above for HTTP 401/4xx on the POST."
  exit 4
fi
if [ "$F2" -ne "$F0" ]; then
  say "  the DELETE did not remove the model (expected $F0, got $F2)."
  say "VERDICT: INCONCLUSIVE — the removal this probe exists to observe never"
  say "         happened, so model_count was never asked to recount."
  exit 4
fi
say "  control OK: the model was really created and really removed."

DRIFT=$((C2-F2))
say ""
say "RESULT: drift after delete = $DRIFT model(s)"
say "  expected if defect present: drift=1 (counter stuck at $C1, actual back to $F0)"
say "  expected if fixed:          drift=0"

if [ "$DRIFT" -ne 0 ]; then
  say "VERDICT: RED — remove_custom_model did not recompute model_count"
  exit 0
else
  say "VERDICT: GREEN — counter consistent"
  exit 1
fi
