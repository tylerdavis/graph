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
   directives** (fixed upstream post-0.18.0). Workaround: `RUSTFLAGS` in
   `mise.toml` `[env]`, with the lib dir resolved via
   `pkg-config --variable=libdir openssl` so any OpenSSL source works (nix,
   Homebrew, …). Drop when a newer lbug releases.
2. **Extensions dlopen against host-exported symbols.** Static linking needs
   `-Wl,-export_dynamic` (macOS spelling of `-rdynamic`) or `LOAD EXTENSION`
   fails with `symbol not found in flat namespace`. Also in the mise
   `RUSTFLAGS`; also emitted by the fixed upstream build script.
3. `INSTALL X; LOAD X;` as one multi-statement `query()` call does not load the
   extension — issue `INSTALL FTS` and `LOAD EXTENSION FTS` as separate calls.
4. `INSTALL` downloads the extension (network) into `~/.lbdb/extension/<ver>/`;
   plan for offline/first-run UX (install lazily, clear error if offline).
5. Statement-level "cargo build script directives don't reach the final link"
   dead end: don't put the OpenSSL fix in a consumer `build.rs`; `rustc-link-lib`
   from a dependent crate's build script did not propagate to test binaries in
   this workspace. Use rustflags.
