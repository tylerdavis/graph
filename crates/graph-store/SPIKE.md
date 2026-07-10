# LadybugDB spike findings (Phase 0)

Spike executed against `lbug = "0.18.0"` on macOS arm64 (2026-07-09). All six
tests in `tests/ladybug_spike.rs` pass. Verdict: **Ladybug is viable** for the
storage design; no fallback needed yet.

## Validated

- `CREATE NODE/REL TABLE IF NOT EXISTS` is supported → idempotent startup DDL.
- Data and schema persist across `Database` close/reopen.
- `MERGE ... ON CREATE SET ... ON MATCH SET` upserts work (needed by `graph__ingest`).
- Prepared statements with `$param` binding via `conn.prepare` / `conn.execute`.
- Large JSON strings round-trip cleanly (checkpoint blobs).
- FTS extension: `INSTALL FTS` → `LOAD EXTENSION FTS` → `CREATE_FTS_INDEX` / `QUERY_FTS_INDEX`.
- Vector extension: `CREATE_VECTOR_INDEX` / `QUERY_VECTOR_INDEX` (HNSW, FLOAT[N] columns).
- `Database: Send + Sync`. `Connection<'a>` borrows the `Database` → the Store
  owns the `Database` and creates short-lived connections per operation.

## Gotchas (all handled in-repo)

1. **lbug 0.18.0 ships a prebuilt `liblbug.a` but omits its OpenSSL link
   directives** (fixed upstream post-0.18.0). Workaround: build directives
   in `crates/graph-store/build.rs` (link-search via pkg-config + `-lssl
   -lcrypto`), NOT env-wide RUSTFLAGS — RUSTFLAGS also links every *build
   script* against libssl, which segfaulted on x86_64 Linux CI. Note the
   cargo quirk: `rustc-link-lib` propagates to downstream binaries via rlib
   metadata but not to the emitting package's own test binaries, which get
   the libs via `rustc-link-arg-tests` instead. Drop all of it when a newer
   lbug releases.
2. **Extensions dlopen against host-exported symbols.** Binaries and test
   binaries need `-export_dynamic` (ld64) / `--export-dynamic` (GNU ld) or
   `LOAD EXTENSION` fails with `symbol not found`. Emitted per-target by
   `crates/graph-store/build.rs` (tests) and `crates/graph-cli/build.rs`
   (bins).
3. **`INSTALL <ext>` races**: concurrent installs share `~/.lbdb/`; the
   spike serializes extension tests with a mutex.
4. `INSTALL X; LOAD X;` as one multi-statement `query()` call does not load the
   extension — issue `INSTALL FTS` and `LOAD EXTENSION FTS` as separate calls.
5. `INSTALL` downloads the extension (network) into `~/.lbdb/extension/<ver>/`;
   plan for offline/first-run UX (install lazily, clear error if offline).
6. lbug's Linux build compiles a C++ bridge that includes `<format>` —
   **GCC 13+ (libstdc++ 13) required**; Debian bookworm's GCC 12 fails.
7. Linking the whole-archive lbug binary needs real memory: GNU ld got
   OOM-killed in a 2 GiB VM.
