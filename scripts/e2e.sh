#!/usr/bin/env bash
# End-to-end checks for the cache-tool CLI.
#
# The Rust integration tests drive the server over gRPC; this drives the actual
# client binary, which is the only way to cover argument parsing, the
# local -> remote -> build fallthrough, archive/restore, and exit-code
# propagation.
#
#   cargo build --release && scripts/e2e.sh
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CT="$ROOT/target/release/cache-tool"
SRV="$ROOT/target/release/cache-server"
[ -x "$CT" ] || CT="$CT.exe"
[ -x "$SRV" ] || SRV="$SRV.exe"
[ -x "$CT" ] && [ -x "$SRV" ] || { echo "build first: cargo build --release"; exit 1; }

PORT=$(( (RANDOM % 20000) + 40000 ))
WORK="$(mktemp -d)"
CACHE="$WORK/client-cache"
SERVER_DIR="$WORK/server"
mkdir -p "$SERVER_DIR" "$WORK/proj"

# The client's cache is redirected with --cache-dir so this never touches a real
# user cache. An environment variable would not work on Windows: the dirs crate
# reads the Known Folder API there, not %LOCALAPPDATA%.
mkdir -p "$CACHE"
if [ "${OS:-}" = "Windows_NT" ]; then
  CACHE_ARG="$(cygpath -w "$CACHE")"
else
  CACHE_ARG="$CACHE"
fi

KEY="e2e-test-key"
# Started without a subshell so $! is the server itself; killing a wrapper
# subshell would leave the server running and holding metadata.db open.
cd "$SERVER_DIR" || exit 1
"$SRV" --addr "127.0.0.1:$PORT" --api-key "$KEY" >/dev/null 2>&1 &
SERVER_PID=$!
cd "$WORK" || exit 1

cleanup() {
  kill "$SERVER_PID" 2>/dev/null
  wait "$SERVER_PID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

for _ in $(seq 1 200); do
  (echo > "/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break
done

REMOTE="http://127.0.0.1:$PORT"
export CACHE_TOOL_API_KEY="$KEY"
cd "$WORK/proj" || exit 1

pass=0; fail=0
check() {
  if [ "$2" = "$3" ]; then
    printf '  PASS  %s\n' "$1"; pass=$((pass + 1))
  else
    printf '  FAIL  %s\n         got [%s] want [%s]\n' "$1" "$2" "$3"; fail=$((fail + 1))
  fi
}

blobs() { ls -1 "$CACHE"/blobs 2>/dev/null | wc -l | tr -d ' '; }
run()   { "$CT" --outputs out.txt --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- cp input.txt out.txt 2>&1; }

echo "cache-tool end-to-end"
echo

mkdir -p src && echo "fn main() {}" > src/lib.rs && echo "payload one" > input.txt

out=$(run); code=$?
check "cold run builds"                "$(echo "$out" | grep -c 'miss')" "1"
check "cold run exits 0"               "$code" "0"
check "output produced"                "$(cat out.txt)" "payload one"
check "blob stored locally"            "$(blobs)" "1"
check "blob uploaded to server"        "$(ls -1 "$SERVER_DIR/cache-storage/blobs" | wc -l | tr -d ' ')" "1"

rm -f out.txt
out=$(run)
check "second run hits local cache"    "$(echo "$out" | grep -c 'local hit')" "1"
check "output restored from cache"     "$(cat out.txt)" "payload one"

rm -f out.txt; rm -rf "$CACHE/blobs"
out=$(run)
check "remote hit after local wipe"    "$(echo "$out" | grep -c 'remote hit')" "1"
check "output restored from remote"    "$(cat out.txt)" "payload one"

echo "payload two" > input.txt; rm -f out.txt
out=$(run)
check "edited input misses"            "$(echo "$out" | grep -c 'miss')" "1"
check "rebuilt output is current"      "$(cat out.txt)" "payload two"

"$CT" --outputs never.txt --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- sh -c "exit 7" >/dev/null 2>&1
check "failed build propagates code"   "$?" "7"

before=$(blobs)
"$CT" --outputs never.txt --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- sh -c "exit 7" >/dev/null 2>&1
check "failed build is not cached"     "$(blobs)" "$before"

rm -f out.txt; rm -rf "$CACHE/blobs"
out=$(CACHE_TOOL_API_KEY="wrong" "$CT" --outputs out.txt --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- cp input.txt out.txt 2>&1); code=$?
check "bad key still builds"           "$code" "0"
check "bad key degrades to a miss"     "$(echo "$out" | grep -c 'remote unavailable (missing or invalid API key)')" "1"
check "bad key still produces output"  "$(cat out.txt)" "payload two"

rm -f out.txt; rm -rf "$CACHE/blobs"
out=$("$CT" --outputs out.txt --cache-dir "$CACHE_ARG" --remote "http://127.0.0.1:1" -- cp input.txt out.txt 2>&1); code=$?
check "unreachable server still builds" "$code" "0"
check "unreachable server warns once"   "$(echo "$out" | grep -c 'remote unavailable')" "1"

mkdir -p dist && echo "a" > dist/a.txt && echo "b" > dist/b.txt
"$CT" --outputs dist --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- sh -c "echo c > dist/c.txt" >/dev/null 2>&1
rm -rf dist
"$CT" --outputs dist --cache-dir "$CACHE_ARG" --remote "$REMOTE" -- sh -c "echo c > dist/c.txt" >/dev/null 2>&1
check "directory output restores"       "$(ls -1 dist 2>/dev/null | wc -l | tr -d ' ')" "3"

echo
echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
