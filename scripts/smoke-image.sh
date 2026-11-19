#!/bin/sh
# Runs a built chappe-bus image and checks that it works: the health check answers,
# a token is accepted, a missing token is refused, the page opens and the database
# is written to the data volume (a fresh volume, a read-only root, no capabilities).
#   scripts/smoke-image.sh chappe-bus:dev
set -eu

image="${1:?usage: smoke-image.sh IMAGE}"
name="chappe-smoke-$$"
dir="$(mktemp -d)"
port="${SMOKE_PORT:-18090}"
token="smoke-token-0123456789abcdef0123456789abcdef"

cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker volume rm "$name-data" >/dev/null 2>&1 || true
  rm -rf "$dir"
}
trap cleanup EXIT

mkdir -p "$dir/tokens"
printf '%s' "$token" > "$dir/tokens/laptop.claude"
chmod 0444 "$dir/tokens/laptop.claude"

# A fresh named volume and a read-only root, as in the Compose file and the chart.
docker run -d --name "$name" -p "127.0.0.1:$port:8080" \
  --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  -v "$dir/tokens:/etc/chappe/tokens:ro" -v "$name-data:/data" \
  -e CHAPPE_TOKENS_DIR=/etc/chappe/tokens "$image" >/dev/null

code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

i=0
until [ "$(code "http://127.0.0.1:$port/healthz" || true)" = "200" ]; do
  i=$((i + 1))
  if [ "$i" -gt 30 ]; then
    echo "FAIL: /healthz did not answer 200" >&2
    docker logs "$name" >&2 || true
    exit 1
  fi
  sleep 1
done

fail() { echo "FAIL: $1" >&2; docker logs "$name" >&2 || true; exit 1; }

who="$(curl -s -H "Authorization: Bearer $token" "http://127.0.0.1:$port/v1/whoami")"
[ "$who" = '{"machine":"laptop","tool":"claude"}' ] || fail "whoami with a token said: $who"
[ "$(code "http://127.0.0.1:$port/v1/whoami")" = "401" ] || fail "a request without a token was not refused"
[ "$(code "http://127.0.0.1:$port/")" = "200" ] || fail "the page did not open"
docker cp "$name:/data/chappe-bus.db" "$dir/chappe-bus.db" >/dev/null 2>&1 || fail "no database in the data volume"
[ -s "$dir/chappe-bus.db" ] || fail "the database is empty"
echo "ok: $image"
