# Third-party notices

This repository vendors patched third-party crates under `patches/` (build-script
stabilization; see each directory's own license file): the base build chain
(`quote`, `serde_core`, `serde_json`, `zmij`, `wit-bindgen-rust-macro`, `tree-sitter`)
and, where present, this language's grammar crate(s):

- (none — base build chain only)

All other dependencies are consumed unmodified from crates.io under their declared licenses.

## Third-party LSP servers (not vendored)

`intentumdiff-lsp-provisioning` discovers, diagnoses, and — only on a caller's explicit,
per-call consent — helps acquire third-party language servers used for enrichment. **This
repository does not embed, vendor, or redistribute any of these servers' binaries.**

- **dbt Fusion engine / `dbt-lsp`** (`crates/lsp-provisioning/src/dbt_fusion.rs`, languages
  `dbt-sql`/`dbt-yaml`): dbt Core proper is Apache-2.0, but the Fusion *binary* — which is
  what actually contains the language server — is additionally governed by dbt Labs' own
  [dbt Product Licensing Agreement](https://www.getdbt.com/licenses-faq). It is free to use,
  with provisions, but is not a permissive OSS license this project is clearly clear to
  redistribute under. Accordingly, `acquire()` never downloads or caches the binary itself:
  on explicit consent it shells out to dbt Labs' own official standalone installer
  (`https://public.cdn.getdbt.com/fs/install/install.sh`), the same public distribution
  channel dbt Labs ship to every user, and the resulting install lives wherever that
  installer places it — never inside this repository or its release artefacts.

If a future server added under this initiative *is* under a license permissive enough to
vendor, that decision and the resulting notice belong here, argued explicitly, not assumed
by precedent from this entry.

