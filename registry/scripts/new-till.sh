#!/usr/bin/env bash
# Issue a till token for the signed-in operator. Prompts for the operator name
# and password; prints the till token once (only its hash is kept), then signs out.
set -euo pipefail
B=${1:-https://registry.fedimintcard.org}
TILL_NAME=${2:-mac till}
[ -t 0 ] || { echo 'run this in a terminal: it prompts for the password' >&2; exit 2; }
read -rp 'operator name: ' NAME
read -rsp 'password: ' PASS; echo
BODY=$(python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1],"password":sys.argv[2]}))' "$NAME" "$PASS")
unset PASS
TOK=$(curl -sf -X POST "$B/auth/operator/session" -H 'content-type: application/json' -d "$BODY" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])') || { echo 'sign-in failed' >&2; exit 1; }
unset BODY
TILL_BODY=$(python3 -c 'import json,sys; print(json.dumps({"name":sys.argv[1]}))' "$TILL_NAME")
curl -sf -X POST "$B/tills" -H 'content-type: application/json' -H "authorization: Bearer $TOK" -d "$TILL_BODY" \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(f"till {d[\"till_id\"]} ({d[\"name\"]}): {d[\"token\"]}")'
curl -sf -o /dev/null -X POST "$B/auth/operator/signout" -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{}'
echo 'Shown once. Start the till with OPS_URL='"$B"' OPS_TILL_TOKEN=<token>.'
