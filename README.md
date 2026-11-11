# cache-tool

A build cache you can share between machines. You put it in front of a build
command, it hashes your source files, and if it has seen those exact inputs
before it hands back the old output instead of building again.

It doesn't know anything about your build system, only the command you give it.

```bash
cache-tool --outputs target/release/myapp -- cargo build --release
```

Run it again without changing anything and you get:

```
cache-tool: local hit 5c0abb9c7baa
```

and no build happens.

## Install

```bash
cargo build --release
```

The binaries end up in `target/release/`. You don't need `protoc`; the build
script parses the `.proto` file with [`protox`](https://crates.io/crates/protox).

## Usage

### Client

```bash
cache-tool --outputs <path> [--outputs <path>...] \
           [--remote <url>] [--api-key <key>] [--cache-dir <path>] \
           [--ca-cert <path>] \
           -- <build command>
```

| Flag | What it does |
|---|---|
| `--outputs` | File or directory the build produces. Repeat for more than one. |
| `--remote` | Cache server URL, like `http://10.0.0.5:50051` or `https://cache.example.com`. Leave it out to use the local cache only. |
| `--api-key` | Key for a server that needs one. Also read from `CACHE_TOOL_API_KEY`. |
| `--cache-dir` | Where local blobs are kept. Also read from `CACHE_TOOL_CACHE_DIR`. |
| `--ca-cert` | PEM certificate to trust for an `https://` remote, instead of the system trust store. Use this for a self-signed or private-CA server. |

Local blobs go in your platform's cache directory by default
(`~/.cache/cache-tool` on Linux, `%LOCALAPPDATA%\cache-tool` on Windows). In CI
you'll probably want `--cache-dir` so the cache lands somewhere the runner keeps
between jobs.

If the server has a normal CA-signed certificate you don't need anything extra.
For a self-signed one, pass `--ca-cert path/to/cert.pem` or the connection will
be refused.

### Server

```bash
cache-server [--addr 127.0.0.1:50051] \
             [--storage ./cache-storage] \
             [--max-size-gb 10] \
             [--api-key <key>] \
             [--tls-cert <path> --tls-key <path>]
```

Without `--api-key` there is no authentication. That's fine on `127.0.0.1` and
a bad idea anywhere else. You can also set `CACHE_SERVER_API_KEY`, which is
better than the flag since command lines are visible to other processes.

Without `--tls-cert` and `--tls-key` the server speaks plain gRPC. Same story:
fine on localhost, and anywhere else your API key would go over the network in
the clear. The two flags have to be given together. A self-signed certificate
works (point clients at it with `--ca-cert`).

## How it works

The cache key is a BLAKE3 hash of a scheme version, the build command, the
declared output paths, and then every source file (its path, its length and its
contents). Each piece is there for a reason:

- Files are sorted by path, because filesystems list entries in whatever order
  they like and the hash has to be the same on every machine.
- Paths use `/` everywhere. Windows gives you `src\main.rs` and Linux gives you
  `src/main.rs`, and the same tree needs the same key on both.
- Lengths are included so `"ab" + "c"` and `"a" + "bc"` don't hash the same.
- The command is included because `cargo build` and `cargo build --release`
  read the same files but produce different output.
- Declared outputs are left out of the inputs. Otherwise the build's own result
  would change the key every time and you'd never get a hit.

`.git`, `target`, `node_modules` and `cache-storage` are skipped.

Lookup goes local cache, then remote cache, then build. A remote hit is also
saved locally. After a successful build the outputs are archived (tar + zstd),
stored locally, and uploaded if you set a remote.

A failed build is never cached, and its exit code is passed straight through.

The cache should never be the reason a build fails. If the server is down, the
key is wrong, or a transfer dies halfway, you get a warning and a normal build.

## Tests

```bash
cargo test                                    # 34 tests
cargo build --release && scripts/e2e.sh       # 26 end-to-end checks
```

- **Unit tests** (`src/main.rs`, 14) check the hashing rules: renaming a file
  changes the key, moving bytes between files changes it, a declared output
  doesn't, inputs come back sorted.
- **Integration tests** (`tests/server.rs`, 20) start a real `cache-server` and
  talk to it over gRPC. They cover uploads bigger than one frame, malformed
  streams, path traversal attempts, LRU eviction order, rebuilding the index
  after `metadata.db` is deleted, auth on every call, and TLS (trusted cert
  works, untrusted self-signed cert is rejected, plaintext is rejected by a
  TLS-only server).
- **End-to-end** (`scripts/e2e.sh`, 26) runs the real client binary through
  local hit, remote hit and build, exit code passthrough, what happens when the
  server is unreachable or the key is wrong, and the same again over TLS.

I also tried breaking the design on purpose (16 mutations) to see whether the
tests would notice. 13 were caught. The three that weren't:

- Deleting the input sort isn't noticed on Windows, because NTFS already
  returns entries in name order. Reversing the sort is caught. The deletion
  only matters on ext4 and network filesystems, which is why CI also runs Linux
  and macOS.
- Removing either one of the two write-once checks alone isn't noticed, since
  the other still stops an overwrite. Removing both is caught.

## Benchmarks

This is a real build, not a synthetic one: `cargo build --release` on cache-tool
itself, with its actual dependencies (tonic, rustls, tokio, rusqlite with
bundled SQLite) producing two binaries, 11 MB uncompressed. Loopback network,
Windows 11. Run it yourself with `scripts/bench.sh [remote-url]`.

| | Time |
|---|---|
| build, uncached | 59.6 s |
| cache miss (build + archive + store) | 63.3 s |
| local hit | 0.75 s |
| remote hit (download + restore) | 0.64 s |

The difference between the first two rows, about 3.7 s here, is what cache-tool
costs you on a miss: hashing the source tree, compressing the output and
writing the blob. It stays roughly the same however long the build takes. In
return, a build of about a minute turns into under a second when nothing has
changed, whether the blob comes from disk or the network.

An earlier version of this benchmark used random data as the artifact, which
doesn't compress at all, so remote hits looked much slower than local ones.
Real binaries compress well, which is why the two are close now.

## Design notes

**Blobs are write-once.** If a `Put` arrives for a hash the server already has,
it accepts straight away without reading the upload. That makes a duplicate
upload of a big artifact cheap and makes concurrent uploads of the same thing
harmless.

**Temp file, then rename.** Client and server both write to a temp path and only
rename it into place once the bytes are complete and `sync_all`ed. A half-written
file at a final blob path would look like a valid cache entry forever, which is
the worst thing this could do. The rename makes the name appear atomically, and
`sync_all` makes the contents durable. You need both.

**The server doesn't check blob contents against the key.** This is on purpose.
In a content-addressed store the key is the hash of the value, so it can check
itself. Here the key is a hash of the build inputs and the value is the compiled
output, so there's nothing to check against. Only the machine that ran the build
knows the right answer. So write access is the trust boundary, which is what
`--api-key` is for.

**Eviction** is LRU against a size cap. SQLite stores `(hash, size, last_access)`
as an index only. The blobs on disk are the real data, and the table is rebuilt
from the directory on startup, so deleting `metadata.db` costs nothing.

Eviction can't hand a truncated blob to a download that's already running. On
Unix an unlinked file stays readable until it's closed. On Windows, deleting an
open file fails and that blob is skipped.

**TLS uses the system trust store unless you say otherwise.** An `https://`
remote with no `--ca-cert` is checked against the OS certificates (through
`rustls-native-certs`), the same ones browsers use. `--ca-cert` swaps that for
one specific certificate, which suits a self-signed server or a private CA, and
that's probably what most small deployments of `cache-server` look like.

## Limitations

- TLS is optional. `cache-server` runs plaintext unless you pass
  `--tls-cert`/`--tls-key`. Client and server warn if an API key would cross a
  plaintext, non-loopback connection, but neither refuses. A badly configured
  setup can still leak a key.
- Every input file is read on every run. There's no mtime shortcut, so the cost
  grows with the size of the tree.
- Outputs are restored whole. No partial or incremental restore.
- Eviction runs after every upload. That's fine for personal use, but a busy
  shared server would want it debounced.

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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
