#!/usr/bin/env bash
# Exercise the actual release image offline. Docker creates only disposable volumes.
set -euo pipefail
image=${1:?usage: smoke-till-image.sh IMAGE}
pwdfile=$(mktemp)
wallet=$(docker volume create)
restored=$(docker volume create)
container=''
cleanup() {
  if [[ -n "$container" ]]; then docker rm -f "$container" >/dev/null || true; fi
  docker volume rm "$wallet" "$restored" >/dev/null
  rm -f "$pwdfile"
}
trap cleanup EXIT
openssl rand -hex 32 > "$pwdfile"
password=$(cat "$pwdfile")
start() {
  container=$(docker run -d --network none --mount "type=volume,src=$1,dst=/data" \
    -e TILL_ADMIN_PASSWORD="$password" "$image")
  for _ in {1..45}; do
    if docker exec "$container" curl -fsS http://127.0.0.1:9878/healthz >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  docker logs "$container" >&2
  return 1
}
start "$wallet"
[[ $(docker exec "$container" id -u) == 1000 ]]
[[ $(docker exec "$container" curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:9878/admin/status) == 401 ]]
docker exec "$container" curl -fsS -H "Authorization: Bearer $password" http://127.0.0.1:9878/admin/status | jq -e '.joined == false' >/dev/null
docker exec "$container" curl -fsS http://127.0.0.1:9878/ | grep -i '<html' >/dev/null
# Prove the complete stopped-volume archive restores the bytes and permissions
# in a second volume. This marker contains disposable test data.
docker exec "$container" sh -ec 'umask 077; echo restore-ok > /data/volume-check'
docker stop -t 30 "$container" >/dev/null
docker rm "$container" >/dev/null
container=''
docker run --rm --network none --mount "type=volume,src=$wallet,dst=/data,readonly" \
  --entrypoint tar "$image" -C /data -cf - . | \
  docker run --rm -i --network none --mount "type=volume,src=$restored,dst=/data" \
  --entrypoint tar "$image" -C /data -xpf -
start "$restored"
[[ $(docker exec "$container" cat /data/volume-check) == restore-ok ]]
[[ $(docker exec "$container" stat -c %a /data/volume-check) == 600 ]]
echo 'Image startup, authentication, non-root storage and stopped-volume restore passed.'
