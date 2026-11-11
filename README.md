# cache-tool

A distributed build cache. Wrap any build command; `cache-tool` hashes every
source input, and if it has seen that exact input set before, it restores the
previous output instead of building.

Language-agnostic — it knows nothing about your build system beyond the command
you hand it.

```bash
cache-tool --outputs target/release/myapp -- cargo build --release
```

Second run with unchanged sources:

```
cache-tool: local hit 5c0abb9c7baa
```

No build runs.

---

## Install

```bash
cargo build --release
# binaries land in target/release/
```

There is no `protoc` dependency — the build script parses `.proto` with
[`protox`](https://crates.io/crates/protox) in pure Rust.

---

## Usage

### Client

```bash
cache-tool --outputs <path> [--outputs <path>...] \
           [--remote <url>] [--api-key <key>] [--cache-dir <path>] \
           [--ca-cert <path>] \
           -- <build command>
```

| Flag | Meaning |
|---|---|
| `--outputs` | File or directory the build produces. Repeat for several. |
| `--remote` | Cache server URL, e.g. `http://10.0.0.5:50051` or `https://cache.example.com`. Omit for local-only. |
| `--api-key` | Key for a server that requires one. Also read from `CACHE_TOOL_API_KEY`. |
| `--cache-dir` | Where to keep local blobs. Also read from `CACHE_TOOL_CACHE_DIR`. |
| `--ca-cert` | Trust this CA certificate (PEM) for an `https://` remote, instead of the system trust store. For a self-signed or private-CA server. |

By default local blobs live in your platform cache directory
(`~/.cache/cache-tool` on Linux, `%LOCALAPPDATA%\cache-tool` on Windows).
`--cache-dir` overrides it, which is what CI wants so the cache lands somewhere
the runner can persist between jobs.

An `https://` remote backed by a real, CA-signed certificate needs nothing
extra — the system trust store already vouches for it. A self-signed or
private-CA server needs `--ca-cert path/to/cert.pem` or the connection is
rejected.

### Server

```bash
cache-server [--addr 127.0.0.1:50051] \
             [--storage ./cache-storage] \
             [--max-size-gb 10] \
             [--api-key <key>] \
             [--tls-cert <path> --tls-key <path>]
```

Omitting `--api-key` runs without authentication, which is reasonable on
`127.0.0.1` and not reasonable anywhere else. The key is also read from
`CACHE_SERVER_API_KEY` — prefer that, since command lines are visible to every
process on the machine.

Omitting `--tls-cert`/`--tls-key` serves plaintext gRPC — fine on `127.0.0.1`,
not fine anywhere else, since an API key would then cross the network in the
clear. Both flags are required together; a self-signed certificate is enough
(clients point `--ca-cert` at it), or use one from a real CA if the server has
a public hostname.

---

## How it works

**The cache key** is a BLAKE3 hash over, in order: a scheme version, the build
command, the declared output paths, and then every source file — each
contributing its path, its byte length, and its contents.

Every part of that is load-bearing:

- **Sorted by path**, because filesystems return entries in arbitrary order and
  an unsorted hash would differ between machines.
- **Paths normalised to `/`**, because Windows yields `src\main.rs` where Linux
  yields `src/main.rs`, and the same tree must key identically on both.
- **Lengths included**, because concatenating contents alone makes `"ab" + "c"`
  indistinguishable from `"a" + "bc"`.
- **Command included**, because `cargo build` and `cargo build --release` read
  the same sources and produce different artifacts.
- **Declared outputs excluded** from the inputs, because hashing the build's own
  product would change the key on every run and never hit.

Skipped directories: `.git`, `target`, `node_modules`, `cache-storage`.

**The lookup order** is local cache → remote cache → build. A remote hit is
written into the local cache on the way through. After a successful build the
artifact is archived (tar + zstd), stored locally, and uploaded if a remote is
configured.

**A failing build is never cached**, and its exit code is passed through
unchanged.

**A cache is an optimisation, so it must never break a build.** Every remote
failure — unreachable server, rejected key, mid-transfer error — is logged and
treated as a miss. There is no remote condition that can make compilation fail.

---

## Tests

```bash
cargo test                                    # 34 tests
cargo build --release && scripts/e2e.sh       # 26 CLI end-to-end checks
```

Three layers, each covering what the one below cannot:

**Unit tests** (`src/main.rs`, 14) pin the hashing rules as properties — that
renaming a file changes the key, that moving bytes between files changes it,
that a declared output does not, that inputs come back sorted.

**Integration tests** (`tests/server.rs`, 20) drive a real `cache-server`
subprocess over gRPC: chunked round trips larger than one frame, rejected
malformed streams, path-traversal attempts, LRU eviction order, index rebuild
after `metadata.db` is deleted, auth on every RPC, and TLS (a trusted round
trip, an untrusted self-signed cert rejected, plaintext rejected by a TLS-only
listener).

**End-to-end** (`scripts/e2e.sh`, 26) drives the actual client binary through
the local → remote → build fallthrough, exit-code propagation, graceful
degradation when the server is unreachable or the key is wrong, and the same
flows again over TLS.

The suite was checked by mutation testing — deliberately breaking each design
decision and confirming a test fails. 13 of 16 mutations were caught. The
survivors are documented rather than hidden:

- **Deleting the input sort** is invisible on Windows, because NTFS returns
  directory entries in name order anyway. Reversing the comparator *is* caught,
  so the test works; the deletion case only bites on ext4 and network
  filesystems, which is why CI runs Linux and macOS too.
- **Removing either write-once guard alone** is survivable because the other
  still prevents an overwrite. Removing both is caught. That is defence in
  depth, not a gap.

## Benchmarks

A real build, not a synthetic one: `cargo build --release` on cache-tool
itself — its actual dependency tree (tonic, rustls, tokio, rusqlite with
bundled SQLite) compiled into two real binaries, 11 MB uncompressed. Loopback
network, Windows 11. Reproduce with `scripts/bench.sh [remote-url]`.

| | Time |
|---|---|
| build, uncached (baseline) | 59.6 s |
| cache miss (build + archive + store) | 63.3 s |
| local hit (restore, no build) | 0.75 s |
| remote hit (download + restore) | 0.64 s |

The gap between the two full-compile rows is cache-tool's fixed overhead —
hashing the whole source tree, archiving the outputs with zstd, and writing
the blob — about 3.7 s on top of a 59.6 s build here, and roughly constant
regardless of how long the build itself takes. The two hit rows are what that
overhead buys back: once the inputs are unchanged, a ~60 s build collapses to
under a second, whether the blob comes from disk or over the network. This is
the actual value proposition — most builds are not this slow, but essentially
none are as fast as a cache hit.

Real compiled binaries compress well, unlike the incompressible synthetic
payload an earlier version of this benchmark used — which is why the remote
column here is competitive with the local one instead of several times
slower.

---

## Design notes

**Write-once blobs.** A blob is never overwritten. If a `Put` arrives for a hash
that already exists, the server accepts immediately without reading the upload —
which turns a duplicate upload of a large artifact into two messages, and makes
concurrent uploads of the same artifact harmless.

**Temp file, then atomic rename.** Both client and server write to a temp path
and rename into place only once the bytes are complete and `sync_all`ed. A
partial file at a final blob path would be read as a valid cache entry
forever — the worst failure this design can have. Rename makes the *name* appear
atomically; `sync_all` is what makes the *contents* durable, and both are
required.

**The server does not verify blob contents against their key.** This is
deliberate and worth understanding. A content-addressed store can verify itself,
because the key *is* the hash of the value. A build cache cannot: the key hashes
the build *inputs* while the value is the compiled *output*, and nothing about
the bytes can prove that mapping is honest. Only the machine that ran the build
knows the right answer. Write access is therefore the trust boundary — that is
what `--api-key` is for.

**Eviction** is LRU against a size cap. SQLite holds `(hash, size, last_access)`
purely as an index; blobs on disk are the source of truth, and the table is
rebuilt from the directory at startup. Deleting `metadata.db` costs nothing.

Eviction cannot serve a truncated blob to an in-flight download, for opposite
reasons per platform: on Unix, `unlink` leaves an open descriptor valid until it
closes; on Windows, deleting an open file fails and that victim is skipped.

**TLS trusts the system store by default, a given CA on request.** An
`https://` remote with no `--ca-cert` is checked against the OS trust store
(`rustls-native-certs`), the same set curl and browsers use — the right
default for a server with a real, CA-issued certificate. `--ca-cert` replaces
that check with one specific certificate, for a self-signed server or a
private CA, which describes most personal and small-team deployments of
`cache-server` far better than a public CA would.

---

## Limitations

- **TLS is opt-in, not enforced.** `cache-server` serves plaintext unless
  `--tls-cert`/`--tls-key` are set, and both client and server warn but do not
  refuse when an API key would cross a plaintext, non-loopback connection. A
  misconfigured deployment can still expose a key; nothing forces the fix.
- **Every input file is read on every invocation.** There is no mtime shortcut,
  so hashing cost scales with tree size.
- **Outputs restore whole.** There is no partial or incremental restore.
- **Eviction runs after every upload.** Fine at personal scale; a busy shared
  server would want it debounced.

---

## Project layout

```
build.rs                  protox + tonic-build codegen
proto/cache.proto         Check / Get / Put service definition
src/lib.rs                generated gRPC types, shared by both binaries
src/main.rs               cache-tool client
src/bin/cache-server.rs   cache-server
tests/server.rs           gRPC integration tests
tests/common/mod.rs       test harness (spawns a real server)
tests/data/tls/           self-signed fixture cert/key for TLS tests
scripts/e2e.sh            CLI end-to-end checks
scripts/bench.sh          benchmark harness
.github/workflows/ci.yml  test matrix + fmt/clippy gates
```

---

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
