#!/usr/bin/env bash
# Measures cache-tool against a build that produces a 50 MB incompressible
# artifact. Random data is the worst case for zstd, so the transfer numbers here
# are a floor rather than a flattering average.
#
# Usage:
#   scripts/bench.sh                              # local cache only
#   scripts/bench.sh http://127.0.0.1:50051       # also measure the remote path
set -u

REMOTE="${1:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CT="$ROOT/target/release/cache-tool"
[ -x "$CT" ] || CT="$ROOT/target/release/cache-tool.exe"
[ -x "$CT" ] || { echo "build first: cargo build --release"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK" || exit 1

# A few source files so hashing has something realistic to walk.
mkdir -p src
for i in $(seq 1 40); do head -c 4096 /dev/urandom > "src/file_$i.dat"; done

BUILD='head -c 52428800 /dev/urandom > out.bin'
REMOTE_ARGS=""
[ -n "$REMOTE" ] && REMOTE_ARGS="--remote $REMOTE"

ms() { date +%s%3N; }

run() {
  local label="$1"; shift
  local start stop
  start=$(ms)
  "$@" >/dev/null 2>&1
  stop=$(ms)
  printf '  %-34s %6s ms\n' "$label" "$((stop - start))"
}

echo "cache-tool benchmark - 50 MB artifact, 40 source files"
echo

# Baseline: the build with no caching at all.
run "build, uncached (baseline)" sh -c "$BUILD"

rm -rf "${LOCALAPPDATA:-$HOME/.cache}/cache-tool/blobs" 2>/dev/null
rm -f out.bin
run "cache miss (build + archive + store)" \
    "$CT" --outputs out.bin $REMOTE_ARGS -- sh -c "$BUILD"

rm -f out.bin
run "local hit (restore, no build)" \
    "$CT" --outputs out.bin $REMOTE_ARGS -- sh -c "$BUILD"

if [ -n "$REMOTE" ]; then
  rm -f out.bin
  rm -rf "${LOCALAPPDATA:-$HOME/.cache}/cache-tool/blobs" 2>/dev/null
  run "remote hit (download + restore)" \
      "$CT" --outputs out.bin $REMOTE_ARGS -- sh -c "$BUILD"
fi

echo
echo "artifact: $(du -h out.bin 2>/dev/null | cut -f1)"
