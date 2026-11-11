#!/usr/bin/env bash
# Measures cache-tool against a real build: compiling cache-tool's own release
# binaries. Real source, real dependency tree (tonic/rustls/rusqlite-bundled),
# real compiled-binary artifacts — as opposed to a synthetic build, which tells
# you about tar/zstd throughput on whatever bytes you feed it and nothing about
# an actual compile.
#
# Each "build, uncached" and "cache miss" run compiles from a from-scratch
# target directory, so expect this to take a couple of minutes — that cost is
# the point: it is what a cache-less CI runner pays on every single job.
#
# Usage:
#   scripts/bench.sh                              # local cache only
#   scripts/bench.sh http://127.0.0.1:50051       # also measure the remote path
set -u

REMOTE="${1:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CT="$ROOT/target/release/cache-tool"
[ -x "$CT" ] || CT="$CT.exe"
[ -x "$CT" ] || { echo "build first: cargo build --release"; exit 1; }

EXT=""
[ "${OS:-}" = "Windows_NT" ] && EXT=".exe"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

BASELINE_DIR="$WORK/target-baseline"
MISS_DIR="$WORK/target-miss"
CACHE_DIR="$WORK/client-cache"

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

echo "cache-tool benchmark - compiling cache-tool's own release binaries"
echo "(this rebuilds the full dependency tree twice; expect a couple of minutes)"
echo

cd "$ROOT" || exit 1

# Baseline: the build with no caching at all, from a clean target directory —
# what a cache-less CI runner pays on every job.
run "build, uncached (baseline)" \
    cargo build --release --target-dir "$BASELINE_DIR"

run "cache miss (build + archive + store)" \
    "$CT" --outputs "$MISS_DIR/release/cache-tool$EXT" \
          --outputs "$MISS_DIR/release/cache-server$EXT" \
          --cache-dir "$CACHE_DIR" $REMOTE_ARGS \
          -- cargo build --release --target-dir "$MISS_DIR"

rm -f "$MISS_DIR/release/cache-tool$EXT" "$MISS_DIR/release/cache-server$EXT"
run "local hit (restore, no build)" \
    "$CT" --outputs "$MISS_DIR/release/cache-tool$EXT" \
          --outputs "$MISS_DIR/release/cache-server$EXT" \
          --cache-dir "$CACHE_DIR" $REMOTE_ARGS \
          -- cargo build --release --target-dir "$MISS_DIR"

if [ -n "$REMOTE" ]; then
  rm -f "$MISS_DIR/release/cache-tool$EXT" "$MISS_DIR/release/cache-server$EXT"
  rm -rf "$CACHE_DIR/blobs"
  run "remote hit (download + restore)" \
      "$CT" --outputs "$MISS_DIR/release/cache-tool$EXT" \
            --outputs "$MISS_DIR/release/cache-server$EXT" \
            --cache-dir "$CACHE_DIR" $REMOTE_ARGS \
            -- cargo build --release --target-dir "$MISS_DIR"
fi

echo
echo "artifact: $(du -ch "$MISS_DIR/release/cache-tool$EXT" "$MISS_DIR/release/cache-server$EXT" 2>/dev/null | tail -1 | cut -f1) (cache-tool + cache-server, uncompressed)"
