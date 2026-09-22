# Building intentumdiff-lsp

Toolchain: **Rust 1.93.0**. The crates are standalone — build each from its own manifest:

```bash
cargo test  --manifest-path crates/lsp-client/Cargo.toml
cargo build --release --manifest-path crates/lsp-server/Cargo.toml
cargo test  --manifest-path crates/lsp-server/Cargo.toml
cargo test  --manifest-path crates/lsp-provisioning/Cargo.toml
```

The server's engine dependency is a git dep on
[intentumdiff-core](https://github.com/buchochelliq-labs/intentumdiff-core) pinned by tag; for a
private clone set `CARGO_NET_GIT_FETCH_WITH_CLI=true`.

`lsp-provisioning`'s default test run never touches the network or spawns a real installer —
its integration tests use a small dependency-free fixture binary (`fake-dbt-lsp`, built from
`crates/lsp-provisioning/tests/fixtures/fake_dbt_lsp.rs`) that speaks real LSP-over-stdio
framing over a real subprocess, standing in for an actual dbt Fusion install. A separate
`#[ignore]`d test drives the identical exchange against a real `dbt` if one happens to be on
your PATH — opt in with:

```bash
cargo test --manifest-path crates/lsp-provisioning/Cargo.toml -- --ignored
```

