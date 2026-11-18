# Building intentumdiff-lsp

Toolchain: **Rust 1.93.0**. The crates are standalone — build each from its own manifest:

```bash
cargo test  --manifest-path crates/lsp-client/Cargo.toml
cargo build --release --manifest-path crates/lsp-server/Cargo.toml
cargo test  --manifest-path crates/lsp-server/Cargo.toml
```

The server's engine dependency is a git dep on
[intentumdiff-core](https://github.com/buchochelliq-labs/intentumdiff-core) pinned by tag; for a
private clone set `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

The native editor-session acceptance needs registry-verified parser components; see
`crates/lsp-server/README.md#testing` (`scripts/provision_lsp_fixtures.py`,
`INTENTUMDIFF_TEST_WASM_DIR`, `INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE=1`).
