# intentumdiff-lsp-server (native)

The native IntentumDiff LSP server (#100, A2.5 Surface S3): a standalone stdio binary that
serves live semantic-diff **diagnostics**, refactoring **code lenses**, and the custom
`intentumdiff/semanticDiff` request — the native successor to the pygls
`intentumdiff lsp-server`, with **no Python in the process**. It links the pure-Rust engine
(`intentumdiff-rust-core` with `default-features = false`) and the sans-IO framing codec from
`intentumdiff-lsp-client` (the same codec both directions of the wire share). Extracted to its
own repo at the split (#82).

## Build

```bash
cd crates/lsp-server
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release
# -> target/release/intentumdiff-lsp-server(.exe)
```

(The codegen-units override sidesteps the engine's known rustc release-OOM at
`codegen-units = 1` on Windows; see `docs/BUILDING.md`.)

## What it speaks

Standard LSP 3.17 over stdio, plus one custom method:

| Message | Behaviour |
|---|---|
| `initialize` | Advertises `textDocumentSync: Full` + a `codeLensProvider` + the negotiated `positionEncoding` (`utf-8` when the client offers it in `general.positionEncodings`, else `utf-16`); loads the repo's `intentumdiff.yaml` config from `rootUri`; reads the client's `codeLens/refreshSupport` and `window.showDocument.support`. Also advertises a `quickfix` `codeActionProvider` and the `intentumdiff.reportIncompleteReview` command. |
| `textDocument/didOpen` / `didChange` | Updates the buffer (a `didChange` whose version is not newer than the buffer's is dropped) and computes a diff against the ref, then **pushes** versioned `publishDiagnostics` and (when supported) requests `workspace/codeLens/refresh`. A declined revision publishes empty diagnostics and drops its lenses; a failed one publishes a single Information diagnostic naming the reason and drops its lenses. An **incomplete** review (core #252, `completeness: "incomplete"`) is never published as clean: each of core's `incomplete.source_ranges` gets an Information diagnostic (`code: "incomplete_review"`) whose message is core's `incomplete.message` verbatim (one at the top of the file when core recorded no range). |
| `textDocument/didClose` | Drops the buffer and lenses and publishes empty diagnostics (lens refresh when lenses were dropped). |
| `textDocument/codeLens` | Returns refactoring-family lenses mapped from the last computed diff (pull, from cache). |
| `textDocument/codeAction` | While the document's current review is incomplete: one `quickfix` **Report IntentumDiff bug…** that runs `intentumdiff.reportIncompleteReview`. |
| `workspace/executeCommand` | `intentumdiff.reportIncompleteReview [uri, version]`: sends `window/showMessageRequest` previewing core's exact `report.title` and full `report.body_markdown` (with a note when `report.truncated`: the link carries a shortened body) with **Open** / **Cancel**. Only **Open** sends `window/showDocument {uri: report.issue_url, external: true}` (a client without `showDocument` gets the link in `window/showMessage`). Nothing is sent anywhere by the server or the core; no path or source text is added to core's report. A stale version is refused (`-32602`). |
| `intentumdiff/semanticDiff` | `oldUri == newUri`: the live buffer (or on-disk file) vs the ref. Distinct URIs: a workspace-contained two-file compare. Errors are in-band `{"error": …}`. |
| `shutdown` / `exit` | `shutdown` → `null`; later requests get `-32600`. `exit` terminates the process at once — code 0 after `shutdown`, 1 without — even while stdin stays open; EOF also exits cleanly (#73). |

**Compute** is the engine's live-diff path (config-aware, all-language, guardrails included):
the server reads the ref baseline once through the core's strict Git reader and diffs exactly
that buffer against the live one with `live_diff_contents_impl` — the same resolution as
`live_handle_diff_impl`. **Response shapes** are the core's source-aware
`lsp_server_shapes` mappings, fed the same old/new buffers the engine positioned: engine
columns are UTF-8 byte offsets, converted to UTF-16 code units by the core (or passed through
when `utf-8` was negotiated). Spans that do not fit their buffer are refused, never
misplaced.

## First-cut scope (deliberate)

- **stdio only.** (`--tcp` remains a Python-only convenience; the socket/named-pipe
  transports with the #88 controls are their own careful slice.)
- **Debounce by coalescing, not timers.** One reader thread queues frames; before computing,
  the loop drains what is already buffered and keeps only the *latest* content per URI — a
  keystroke burst costs one diff. No async runtime, one joined thread.
- **Bounded memory.** The reader→main queue holds at most 1024 frames / 64 MiB of estimated
  payload (one larger frame is admitted alone; frames are capped at 64 MiB by the codec). A
  full queue blocks the reader, so the client's writes back-pressure through the pipe —
  requests are never dropped. A queued full-sync `didChange` is replaced in place by a newer
  one for the same URI when no request or other message for that URI follows it. Open
  buffers are capped at 1024 documents / 256 MiB of text: the least-recently-updated ones
  are evicted (diagnostics cleared, lenses dropped) and return on their next change or
  re-open. Override with `--max-pending-messages`, `--max-pending-bytes`,
  `--max-open-documents`, `--max-document-bytes`.
- **`uri_to_path` ports the #88 guards:** file scheme only, percent-decode, explicit `..`
  rejection, the Windows drive-letter fix, and workspace containment.
- Engine fallbacks publish empty diagnostics for that revision (clearing the previous
  revision's results) instead of leaving stale ones in place.

**Parser wasm dir resolution:** `--wasm-dir` flag → `INTENTUMDIFF_WASM_DIR` env →
`<exe dir>/wasm` → dev-layout ancestor walk for `src/intentumdiff/wasm`; every candidate is
verified by `parser_manifest.json`.

## Testing

`tests/lsp_session.rs` spawns the built binary and drives real editor sessions over stdio:
the full flow (handshake, an edit → diagnostics + lens refresh, codeLens pull, both
`semanticDiff` modes, the containment guard, exit with stdin open), UTF-16/utf-8 lens
positions, close/declined/failed/out-of-order stale-state regressions, protocol exit, and
queue/cache bounds. It reuses the `intentumdiff-lsp-client` codec, shells out to `git` for
fixtures and needs no Python.

The parser-backed sessions need a parser bundle. Stage the registry-verified one CI uses
(needs a token with read access to `intentumdiff-registry` and the parser repos' Actions):

```bash
pip install "pyyaml>=6.0"
GH_TOKEN=... python3 scripts/provision_lsp_fixtures.py --out build/lsp-wasm
INTENTUMDIFF_TEST_WASM_DIR=$PWD/build/lsp-wasm INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE=1 \
  cargo test --manifest-path crates/lsp-server/Cargo.toml
```

Without `INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE=1`, those sessions skip with a printed note
when no bundle (explicit dir or dev-layout ancestor) or `git` is found. With it — as CI runs
— a missing dir, missing `git`, an unverified (`--no-verify`) bundle, a component whose
bytes do not match `provenance.json`, or a manifest naming an unprovisioned component FAILS
the run, and the binary + component SHA-256 identities are printed and written to
`INTENTUMDIFF_LSP_ACCEPTANCE_RECORD` when set. The lifecycle and bounds sessions need no
parsers and always run.

## Cutover

The CLI/pyproject switch (making `intentumdiff lsp-server` spawn this binary) and deleting
`src/intentumdiff/lsp_server/` + the `pygls` / `lsprotocol` extras ride the Phase-B clap CLI —
the porting work itself is complete.
