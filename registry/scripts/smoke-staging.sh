#!/usr/bin/env bash
# Smoke test of a deployed registry. Prompts for the operator name and
# password; prints only status codes and reasons, never the password or tokens.
set -euo pipefail
B=${1:-https://fedimintcard-registry-staging.old-cell-30e7.workers.dev}
if [ -n "${SMOKE_NAME:-}" ]; then NAME=$SMOKE_NAME; PASS=$SMOKE_PASS  # local wrangler dev only
else
  [ -t 0 ] || { echo 'run this in a terminal: it prompts for the password' >&2; exit 2; }
  read -rp 'operator name: ' NAME
  read -rsp 'password: ' PASS; echo
fi
J() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
call() { # method path token [body] -> prints "code body"
  local args=(-s -o /tmp/smoke.$$ -w '%{http_code}' -X "$1" "$B$2" -H 'content-type: application/json')
  [ -n "$3" ] && args+=(-H "authorization: Bearer $3")
  [ -n "${4:-}" ] && args+=(-d "$4")
  local code; code=$(curl "${args[@]}"); echo "$code $(cat /tmp/smoke.$$)"; }
check() { # label want "code body"
  local code=${3%% *} body=${3#* }
  local reason; reason=$(echo "$body" | python3 -c "import json,sys; print(json.load(sys.stdin).get('reason',''))" 2>/dev/null || true)
  if [ "$code" = "$2" ]; then echo "ok   $1 [$code] $reason"; else echo "FAIL $1 [$code, want $2] $reason"; FAILED=1; fi; }
FAILED=0
LOGIN=$(python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"password":sys.argv[2]}))' "$NAME" "$PASS")
unset PASS
t0=$(python3 -c 'import time; print(time.time())')
R=$(call POST /auth/operator/session "" "$LOGIN"); unset LOGIN
t1=$(python3 -c 'import time; print(time.time())')
check "sign in ($(python3 -c "print(round($t1-$t0,2))")s)" 200 "$R"
TOK=$(echo "${R#* }" | J "d['token']")
check "list batches" 200 "$(call GET /registry/batches "$TOK")"
R=$(call POST /registry/batches "$TOK" "{\"federation_id\":\"$(printf 'b%.0s' {1..64})\",\"count\":1,\"pin_policy\":\"batch\",\"load_sha256\":\"$(printf 'a%.0s' {1..64})\"}")
check "create batch" 200 "$R"; BID=$(echo "${R#* }" | J "d['batch']['batch_id']")
SID=$(python3 -c 'import secrets; print(secrets.token_hex(4))')
CARD=$(python3 - "$SID" "$BID" <<'PY'
import json,sys,secrets
sid,bid=sys.argv[1],int(sys.argv[2])
keys=[secrets.token_hex(28)+sid]+[secrets.token_hex(32) for _ in range(7)]
print(json.dumps({"short_id":sid,"batch_id":bid,"installed_at":1760000000000,"load_sha256":"a"*64,
 "pin_set":True,"pin_policy":"batch","notes_issued":0,"slot_pubkeys":keys}))
PY
)
check "post card $SID" 200 "$(call POST /registry/cards "$TOK" "$CARD")"
check "post card again (idempotent)" 200 "$(call POST /registry/cards "$TOK" "$CARD")"
check "list cards" 200 "$(call GET "/registry/cards?batch_id=$BID" "$TOK")"
R=$(call POST /tills "$TOK" '{"name":"smoke till"}'); check "create till" 200 "$R"
TILL=$(echo "${R#* }" | J "d['token']"); TID=$(echo "${R#* }" | J "d.get('till_id', d.get('id'))")
R=$(call GET "/cards/$SID/state" "$TILL"); check "till reads card state" 200 "$R"
VER=$(echo "${R#* }" | J "d['version']"); OLD=$(echo "${R#* }" | J "[s['pubkey'] for s in d['slots'] if s['slot']==7][0]")
NEW=$(python3 -c 'import secrets; print(secrets.token_hex(32))')
# Bodies built in python: bash brace-expands a literal {..,..} inside $(...).
renew() { python3 -c 'import json,sys; print(json.dumps({"version":int(sys.argv[1]),"old_pubkey":sys.argv[2],"pubkey":sys.argv[3]}))' "$@"; }
BODY=$(renew "$VER" "$OLD" "$NEW")
check "till renews slot 7" 200 "$(call PUT "/cards/$SID/slots/7" "$TILL" "$BODY")"
BODY=$(renew "$VER" "$NEW" "$OLD")
check "stale version refused" 409 "$(call PUT "/cards/$SID/slots/7" "$TILL" "$BODY")"
check "till cannot list batches" 403 "$(call GET /registry/batches "$TILL")"
check "operator cannot renew slots" 403 "$(call PUT "/cards/$SID/slots/7" "$TOK" '{}')"
check "revoke till" 200 "$(call DELETE "/tills/$TID" "$TOK")"
check "revoked till refused" 401 "$(call GET "/cards/$SID/state" "$TILL")"
check "sign out" 200 "$(call POST /auth/operator/signout "$TOK")"
check "signed-out token refused" 401 "$(call GET /registry/batches "$TOK")"
rm -f /tmp/smoke.$$
[ "$FAILED" = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
