#!/usr/bin/env bash
# Rootless container smoke test over synthetic volumes (wave 16, agent 23).
#
# Builds the image for the native platform, creates a Docker named volume that holds the three
# roots under a single parent (/data), runs the image non-root with a read-only rootfs and no
# capabilities, then checks: healthcheck healthy, login page served, login + CSRF scan in dry run
# completes, hardlinks preserved, the tree untouched, runtime facts (uid, caps, rootfs).
#
# Only loopback (127.0.0.1) is contacted. No qBittorrent/ARR/Jellyfin is configured. Nothing is
# pushed. Base images are pulled by `docker build`. Everything created here is removed on exit.
#
#   crates/e2e-perf/scripts/container-smoke.sh            # prints a result table, exit 0 = pass
set -euo pipefail

cd "$(dirname "$0")/../../.."
TAG="removarr:e2e-smoke"
PFX="removarr-e2e-$$"
VOL="$PFX-data"; CFG="$PFX-config"; CT="$PFX-app"
PORT="${SMOKE_PORT:-18080}"
UIDGID="1000:1000"
PASSWORD="dummy-only-password-for-tests"
KEY="dummy-session-key-0123456789-0123456789-0123456789-0123456789-abcdef"
JAR="$(mktemp -d)/jar"
pass=0; fail=0
ok()   { printf 'PASS  %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf 'FAIL  %s\n' "$1"; fail=$((fail+1)); }
check() { local what="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi; }
cleanup() {
  docker rm -f "$CT" >/dev/null 2>&1 || true
  docker volume rm "$VOL" "$CFG" >/dev/null 2>&1 || true
  [ "${KEEP_IMAGE:-0}" = 1 ] || docker rmi "$TAG" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== build (native platform)"
t0=$(date +%s)
docker build -q -t "$TAG" . >/dev/null
echo "build time: $(( $(date +%s) - t0 )) s"
size=$(docker image inspect "$TAG" --format '{{.Size}}')
echo "image size: $((size / 1024 / 1024)) MiB (budget B8 250 MiB)"
[ "$size" -le $((250*1024*1024)) ] && ok "B8 image <= 250 MiB" || bad "B8 image <= 250 MiB"
bin=$(docker run --rm --entrypoint stat "$TAG" -c %s /usr/local/bin/removarr)
echo "binary size: $((bin / 1024 / 1024)) MiB (budget B8 30 MiB)"
[ "$bin" -le $((30*1024*1024)) ] && ok "B8 static musl binary <= 30 MiB" || bad "B8 static musl binary <= 30 MiB"

echo "== synthetic volumes (one parent, so hardlinks work)"
docker volume create "$VOL" >/dev/null; docker volume create "$CFG" >/dev/null
# A fresh named volume is root-owned: hand it to the application identity once (the only root step).
docker run --rm --user 0:0 --entrypoint chown -v "$VOL:/data" -v "$CFG:/config" "$TAG" "$UIDGID" /data /config
# The helper runs as the same non-root identity as the application.
HELPER=(docker run --rm --user "$UIDGID" --entrypoint sh -v "$VOL:/data" -v "$CFG:/config" "$TAG" -c)
"${HELPER[@]}" '
  set -e
  umask 022
  mkdir -p /data/completed /data/incomplete /data/medias/Movies
  i=0
  while [ $i -lt 300 ]; do
    n=$(printf "Movie.%04d.2020.1080p.WEB.x264-GRP" $i)
    mkdir -p "/data/completed/$n" "/data/medias/Movies/$n (2020)"
    printf "video-%s" "$i" > "/data/completed/$n/$n.mkv"
    printf "nfo" > "/data/completed/$n/$n.nfo"
    if [ $((i % 5)) -lt 3 ]; then ln "/data/completed/$n/$n.mkv" "/data/medias/Movies/$n (2020)/$n.mkv"; fi
    i=$((i+1))
  done
  mkdir -p /data/completed/stray
  j=0; while [ $j -lt 50 ]; do printf "j" > /data/completed/stray/junk-$j.txt; j=$((j+1)); done
  mkdir -p /data/incomplete && printf "partial" > /data/incomplete/Partial.mkv.!qB
  # Old files: recent ones are hidden from the issue lists by design.
  find /data -type f -exec touch -t 202501010000 {} +
'
fingerprint() {
  "${HELPER[@]}" 'find /data -exec stat -c "%n %i %h %s %Y %a %u" {} + | sort' 2>/dev/null | sha256sum | cut -d' ' -f1
}
before=$(fingerprint)
files=$("${HELPER[@]}" 'find /data -type f | wc -l' | tr -d ' ')
links=$("${HELPER[@]}" 'find /data/medias -type f -links +1 | wc -l' | tr -d ' ')
echo "synthetic files: $files, hardlinked media files: $links"
[ "$links" -eq 180 ] && ok "real hardlinks on the volume (180 expected)" || bad "real hardlinks on the volume ($links)"

echo "== hash a dummy password inside the image"
HASH=$(printf '%s\n' "$PASSWORD" | docker run --rm -i --user "$UIDGID" "$TAG" hash-password)
case "$HASH" in '$argon2id$v=19$m=19456,t=2,p=1$'*) ok "hash-password emits Argon2id with production parameters";; *) bad "hash-password output";; esac

echo "== run: non-root, read-only rootfs, no capabilities"
docker run -d --name "$CT" --user "$UIDGID" --read-only --tmpfs /tmp \
  --cap-drop ALL --security-opt no-new-privileges \
  -p "127.0.0.1:$PORT:8080" \
  -v "$VOL:/data" -v "$CFG:/config" \
  -e REMOVARR_COMPLETED_ROOT=/data/completed -e REMOVARR_INCOMPLETE_ROOT=/data/incomplete \
  -e REMOVARR_MEDIA_ROOTS=/data/medias -e REMOVARR_EXPECTED_OWNER="$UIDGID" \
  -e REMOVARR_PASSWORD_HASH="$HASH" -e REMOVARR_SESSION_KEY="$KEY" \
  -e REMOVARR_COOKIE_SECURE=false -e REMOVARR_DRY_RUN=true -e REMOVARR_AUTO_MODE=false \
  -e REMOVARR_VIDEO_EXTENSIONS=.mkv -e REMOVARR_SCAN_INTERVAL_MINUTES=0 -e REMOVARR_QBIT_REFRESH_MINUTES=0 \
  "$TAG" >/dev/null

echo "== health"
start=$(date +%s); status=starting
for _ in $(seq 1 100); do
  status=$(docker inspect "$CT" --format '{{.State.Health.Status}}')
  [ "$status" = healthy ] && break
  [ "$(docker inspect "$CT" --format '{{.State.Running}}')" = true ] || break
  # The Docker interval is 30 s; also ask the same command by hand so the smoke test is quick.
  docker exec "$CT" removarr healthcheck >/dev/null 2>&1 && { status=ok-by-exec; break; }
  sleep 1
done
echo "time to healthy: $(( $(date +%s) - start )) s (status: $status)"
case "$status" in healthy|ok-by-exec) ok "healthcheck passes";; *) bad "healthcheck ($status)"; docker logs "$CT" | tail -20;; esac
# The Docker HEALTHCHECK itself must become healthy within interval + start period.
for _ in $(seq 1 60); do
  [ "$(docker inspect "$CT" --format '{{.State.Health.Status}}')" = healthy ] && break; sleep 1
done
[ "$(docker inspect "$CT" --format '{{.State.Health.Status}}')" = healthy ] && ok "Docker HEALTHCHECK reports healthy" || bad "Docker HEALTHCHECK reports healthy"

echo "== runtime facts"
check "process runs as $UIDGID" test "$(docker exec "$CT" id -u):$(docker exec "$CT" id -g)" = "$UIDGID"
check "no effective capabilities" docker exec "$CT" sh -c 'grep -q "^CapEff:.0000000000000000" /proc/1/status'
check "rootfs is read-only" sh -c "! docker exec $CT sh -c 'touch /usr/local/bin/x' 2>/dev/null"
check "config volume is writable (database created)" docker exec "$CT" sh -c 'ls /config | grep -q removarr.db'
check "container is not privileged" test "$(docker inspect "$CT" --format '{{.HostConfig.Privileged}}')" = false
check "port bound to loopback only" sh -c "docker port $CT 8080 | grep -q '^127.0.0.1:'"
check "no secret in the container logs" sh -c "! docker logs $CT 2>&1 | grep -q -e '$PASSWORD' -e '$KEY' -e 'argon2id'"

echo "== HTTP (loopback)"
B="http://127.0.0.1:$PORT"
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
check "/healthz 200" test "$(code "$B/healthz")" = 200
check "/readyz 200" test "$(code "$B/readyz")" = 200
check "unauthenticated / redirects to /login" test "$(code "$B/")" = 303
page=$(curl -s -c "$JAR" "$B/login")
grep -q 'name="csrf_token"' <<<"$page" && ok "login page served with a form and a CSRF field" || bad "login page served with a form and a CSRF field"
check "security headers present" sh -c "curl -sI $B/login | grep -qi '^content-security-policy:.*script-src'"
tok=$(echo "$page" | sed -n 's/.*name="csrf_token" value="\([^"]*\)".*/\1/p' | head -1)
rc=$(curl -s -b "$JAR" -c "$JAR" -o /dev/null -w '%{http_code}' -H "Origin: $B" --data-urlencode "csrf_token=$tok" --data-urlencode username=admin --data-urlencode "password=wrong" "$B/login")
check "wrong password refused (401)" test "$rc" = 401
page=$(curl -s -b "$JAR" -c "$JAR" "$B/login"); tok=$(echo "$page" | sed -n 's/.*name="csrf_token" value="\([^"]*\)".*/\1/p' | head -1)
rc=$(curl -s -b "$JAR" -c "$JAR" -o /dev/null -w '%{http_code}' -H "Origin: $B" --data-urlencode "csrf_token=$tok" --data-urlencode username=admin --data-urlencode "password=$PASSWORD" "$B/login")
check "login succeeds (303)" test "$rc" = 303
tok=$(curl -s -b "$JAR" "$B/jobs" | sed -n 's/.*name="csrf_token" value="\([^"]*\)".*/\1/p' | head -1)
check "dashboard served after login" test "$(code -b "$JAR" "$B/")" = 200
check "a command without CSRF token is refused" test "$(code -b "$JAR" -X POST -H "Origin: $B" "$B/scan")" = 403

echo "== dry-run scan"
loc=$(curl -s -b "$JAR" -o /dev/null -D - -X POST -H "Origin: $B" --data-urlencode "csrf_token=$tok" "$B/scan" | tr -d '\r' | sed -n 's/^[Ll]ocation: //p')
echo "job: $loc"
done_=0
for _ in $(seq 1 120); do
  p=$(curl -s -b "$JAR" "$B$loc/progress")
  if ! echo "$p" | grep -q 'hx-trigger="every 2s"'; then done_=1; break; fi
  sleep 1
done
[ "$done_" = 1 ] && ok "scan job finished" || bad "scan job finished"
echo "$p" | grep -q 'check-circle' && ok "scan job succeeded" || bad "scan job succeeded"
junk=$(curl -s -b "$JAR" "$B/issues/junk")
n=$(echo "$junk" | { grep -o 'href="/issues/junk/[^"]*"' || true; } | sort -u | wc -l | tr -d ' ')
echo "junk issues listed: $n"
[ "$n" -ge 1 ] && ok "scan produced junk issues from the synthetic tree ($n)" || bad "scan produced issues"
check "dashboard shows the dry-run banner" sh -c "curl -s -b $JAR $B/ | grep -q 'badge info'"
rss=$(docker exec "$CT" sh -c "grep -E '^(VmHWM|VmRSS):' /proc/1/status" | tr -s ' ' | tr '\n' ' ')
echo "container process (small fixture): $rss"
after=$(fingerprint)
[ "$before" = "$after" ] && ok "B11 the synthetic tree is byte-for-byte and inode-for-inode unchanged" || bad "B11 the synthetic tree changed"
logs=$(docker logs "$CT" 2>&1 | wc -l | tr -d ' ')
echo "log lines emitted: $logs"
check "no secret in the container logs after login and scan" sh -c "! docker logs $CT 2>&1 | grep -q -e '$PASSWORD' -e '$KEY' -e 'argon2id'"

echo "== graceful stop"
t0=$(date +%s)
docker stop -t 30 "$CT" >/dev/null
[ "$(docker inspect "$CT" --format '{{.State.ExitCode}}')" = 0 ] && ok "SIGTERM exits 0 in $(( $(date +%s) - t0 )) s" || bad "SIGTERM exit code"

echo
echo "result: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
