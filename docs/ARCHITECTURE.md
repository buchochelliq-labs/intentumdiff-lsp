# intentumdiff-lsp architecture

Two **standalone** crates, side by side, plus a third for server acquisition:

- **`crates/lsp-client`** — a generic (IntentumDiff-agnostic) LSP client codec: message framing,
  request/response correlation, capability negotiation. No engine dependency.
- **`crates/lsp-server`** — the native `intentumdiff-lsp-server` binary: links
  [intentumdiff-core](https://github.com/buchochelliq-labs/intentumdiff-core) in-process (the same
  engine handlers behind the
  [C ABI](https://github.com/buchochelliq-labs/intentumdiff-core/blob/main/docs/C_ABI.md))
  plus the client codec, serving semantic-diff-aware diagnostics and code lenses over LSP.
- **`crates/lsp-provisioning`** — discovery, actionable diagnostics, and explicit-consent
  acquisition for third-party servers used for enrichment (dbt Fusion's `dbt-lsp` today).
  Deliberately separate from `lsp-client`: server-specific acquisition logic (installers,
  version pinning, redistribution decisions) doesn't belong in a codec that other projects
  reuse precisely because it stays agnostic of any one server. See
  `crates/lsp-provisioning/src/dbt_fusion.rs` for the current implementation and
  `THIRD_PARTY_NOTICES.md` for why it fetches from the vendor's own channel rather than
  vendoring a binary.


## Why no workspace

Deliberate: cargo only honors `[patch.crates-io]` tables in a build's **top-level manifest**.
A workspace root would silently disable each crate's vendored patch table, so the crates stay
standalone (each is its own build root) — mirroring how they build in the archive monorepo.
