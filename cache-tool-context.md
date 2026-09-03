# cache-tool — Project Context

## What This Is

A distributed build cache written in Rust. Wraps any build command, computes a
BLAKE3 hash of all source inputs, and caches the output artifact. Cache hits skip
the build entirely and restore from cache. Language-agnostic, zero config.

```bash
cache-tool --outputs target/release/myapp -- cargo build --release
```

## Current State

**V0.1 – V0.8 complete.** See [README.md](README.md) for usage, design notes,
benchmarks, and limitations — it is the authoritative document now.

| Version | What | Status |
|---------|------|--------|
| V0.1 | Hashing engine | done |
| V0.2 | Local cache | done |
| V0.3 | gRPC protocol definition | done |
| V0.4 | Cache server (Check, Get, Put) | done |
| V0.5 | Client–server integration, `--remote` | done |
| V0.6 | SQLite LRU eviction | done |
| V0.7 | Auth (API key via gRPC metadata) | done |
| V0.8 | Polish, README, benchmarks | done |
| — | Tests (31 Rust + 19 e2e), CI, mutation-verified | done |

Not yet done: publishing to crates.io (needs `LICENSE` files and a `repository`
field), and TLS.

## Project Structure

```
build.rs                  protox + tonic-build codegen (no protoc needed)
proto/cache.proto         Check / Get / Put service definition
src/lib.rs                generated gRPC types, shared by both binaries
src/main.rs               cache-tool client
src/bin/cache-server.rs   cache-server
tests/server.rs           gRPC integration tests
tests/common/mod.rs       test harness (spawns a real server)
scripts/e2e.sh            CLI end-to-end checks
scripts/bench.sh          benchmark harness
.github/workflows/ci.yml  test matrix + fmt/clippy gates
```

---

## Corrections to earlier versions of this document

Two things previously recorded here turned out to be wrong. Both are worth
keeping visible so they are not reintroduced.

**1. The server cannot verify blob contents against the cache key.**

This document used to list "hash verification on upload — reject if content hash
doesn't match claimed key (prevents cache poisoning)" as a design decision, and
the server was written that way. It cannot work.

The key is a hash of the build *inputs*. The value is the compiled *output*.
They are unrelated byte streams, so `BLAKE3(artifact)` never equals the input
hash, and such a check would reject every legitimate upload.

This is the difference between a content-addressed store (key *is* the hash of
the value, self-verifying) and a build cache (key is a hash of inputs, value is
outputs, inherently unverifiable — only the machine that ran the build knows the
correct answer). Bazel handles this with two separate indices: a CAS plus an
action cache pointing into it. This project uses one index and makes **write
access** the trust boundary instead, which is what `--api-key` is for.

The hex-format validation on the hash was kept — that is path-traversal
protection and is unrelated to content verification.

**2. "Sort before hashing" was documented but never implemented.**

The rationale was recorded here correctly; the code never called
`sort_by_file_name` or any equivalent. It does now. Three related bugs were
fixed at the same time: filenames were not hashed (renaming a file left the key
unchanged), contents were concatenated without length delimiters (`"ab"+"c"`
collided with `"a"+"bc"`), and the build command was not part of the key
(`cargo build` and `cargo build --release` shared one blob).

A fourth was added that this document never anticipated: paths are normalised to
forward slashes, because Windows yields `src\main.rs` where Linux yields
`src/main.rs`, and an unnormalised hash keys the same tree differently per
platform — which quietly defeats a *distributed* cache.

---

## Bugs the test suite found

**Temp files leaked on every rejected upload (Windows only).** `discard()` ran
while the temp file was still open. Unix allows unlinking an open file, so the
cleanup appeared to work; Windows refuses, and `discard` ignores the failure by
design, so every rejected upload left a file in `tmp/`. Fixed by moving the
write loop into `stream_into`, which takes the file by value and therefore
closes it on every exit path before the caller cleans up.

**LRU ordering was unreliable.** `now()` returned whole seconds, so blobs
touched within the same second tied on `last_access` and eviction order became
arbitrary. Now microseconds.

**The local cache directory could not be configured.** There was no
`--cache-dir`, and an environment variable cannot substitute on Windows because
the `dirs` crate reads the Known Folder API rather than `%LOCALAPPDATA%`. That
made the client impossible to test in isolation and impossible to point at a
persistent CI cache.

---

## Architecture Notes

**Why BLAKE3:** cryptographic-grade, very fast, good crate support in Rust.

**Why skip `.git` / `target` / `node_modules` / `cache-storage`:** `.git` changes
on every commit even when source doesn't; `target` is build output, so hashing it
is circular; `node_modules` is dependency code defined by a lockfile; and
`cache-storage` belongs to a server that may be running in the same directory.

**Why temp file + atomic rename:** crash-safe writes, used on both client and
server. A partial blob at a final path would be read as a valid cache entry
forever. Rename makes the *name* appear atomically; `sync_all` is what makes the
*contents* durable. Both are required — rename alone is not enough.

**Why a cache must never break a build:** every remote failure (unreachable
server, rejected key, mid-transfer error) is logged and treated as a miss. There
is no remote condition that can cause a compile to fail.

**Cache hit/miss flow:**
1. Compute hash of source tree, command, and declared outputs
2. Local cache → hit: restore, skip build
3. Remote cache → hit: download, store locally, restore, skip build
4. Miss: run command; on success archive outputs, store locally, upload
5. On build failure: pass the exit code through and cache nothing
