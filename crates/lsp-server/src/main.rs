//! Native IntentumDiff LSP server (#100, A2.5 S3 slice 2 — first cut): stdio transport.
//!
//! Replaces the pygls `intentumdiff lsp-server` for the editor flow: live semantic-diff
//! diagnostics (push) + refactoring code lenses (pull) + the custom
//! `intentumdiff/semanticDiff` request, with ZERO Python in the process. The LSP framing is
//! the `intentumdiff-lsp-client` sans-IO codec (transport-symmetric); the compute is the
//! core's `live_handle_diff_impl` (config-aware, all-language, guardrails included); the
//! response shapes are the core's `lsp_server_shapes` mappings (parity-locked against
//! lsprotocol's encoder).
//!
//! First-cut scope (deliberate):
//! - **stdio only** (matching the live-server transport decision; `--tcp` is Python-only).
//! - **Debounce by coalescing, not timers**: the reader thread queues messages; before
//!   computing, the main loop drains everything already buffered and keeps only the LATEST
//!   content per URI — a burst of `didChange` costs one diff. No timers, no async runtime,
//!   one thread, clean #73 exit on EOF.
//! - Engine fallbacks (`{"fallback": …}`) publish no diagnostics for that revision — the
//!   pygls server's `diff is None` behaviour.
//! - **Positions are source-aware**: engine `NodePosition` columns are UTF-8 byte offsets.
//!   The server reads the ref baseline ONCE, diffs exactly that old buffer against the live
//!   buffer, and maps ranges through the core's source-aware helpers with those same two
//!   buffers. The position encoding is negotiated (`general.positionEncodings`): `utf-8`
//!   when the client offers it (byte columns are then already exact), otherwise the
//!   protocol default `utf-16`. No conversion algorithm lives in this binary.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use intentumdiff_lsp_client::{encode_message, DecodeEvent, FrameDecoder};
use intentumdiff_rust_core::c_abi;
use intentumdiff_rust_core::live_server as proto;
use intentumdiff_rust_core::lsp_server_shapes as shapes;
use serde_json::{json, Value};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn dir_has_manifest(dir: &Path) -> bool {
    dir.join("parser_manifest.json").is_file()
}

/// Bundled-parser dir, zero-setup (the live-server chain): `--wasm-dir` > env >
/// exe-adjacent `wasm/` > dev-layout ancestor walk — every candidate manifest-verified.
fn resolve_wasm_dir(args: &[String]) -> String {
    if let Some(dir) = arg_value(args, "--wasm-dir") {
        return dir;
    }
    if let Ok(dir) = std::env::var("INTENTUMDIFF_WASM_DIR") {
        if !dir.trim().is_empty() {
            return dir;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let shipped = exe_dir.join("wasm");
            if dir_has_manifest(&shipped) {
                return shipped.to_string_lossy().into_owned();
            }
            for ancestor in exe_dir.ancestors() {
                let dev = ancestor.join("src").join("intentumdiff").join("wasm");
                if dir_has_manifest(&dev) {
                    return dev.to_string_lossy().into_owned();
                }
            }
        }
    }
    String::new()
}

/// `uri_to_path` (#88 guards ported from `_handlers.py`): file scheme only, percent-decode,
/// explicit `..` traversal rejection, the Windows leading-slash drive fix, and optional
/// workspace containment.
fn uri_to_path(uri: &str, workspace_root: Option<&Path>) -> Result<PathBuf, String> {
    let rest = if let Some(rest) = uri.strip_prefix("file://") {
        rest
    } else if uri.contains("://") {
        return Err(format!("Only file:// URIs are supported, got: {uri:?}"));
    } else {
        uri
    };
    // Strip an authority component (file://host/path is not supported; file:///path is).
    let raw_path = rest.split(['?', '#']).next().unwrap_or(rest);
    let decoded = percent_decode(raw_path);
    if decoded.split(['/', '\\']).any(|part| part == "..") {
        return Err(format!("Path traversal rejected in URI: {uri:?}"));
    }
    // Windows: "/C:/Users/…" → "C:/Users/…".
    let mut text = decoded;
    if text.len() >= 3 {
        let bytes = text.as_bytes();
        if bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
            text = text[1..].to_owned();
        }
    }
    let path = PathBuf::from(&text);
    let resolved = normalize(&path);
    if let Some(root) = workspace_root {
        let root = normalize(root);
        if !resolved.starts_with(&root) {
            return Err(format!(
                "URI {uri:?} resolves to {resolved:?} which is outside the workspace root {root:?}."
            ));
        }
    }
    Ok(resolved)
}

/// Lexical normalization (Python `Path.resolve()` without symlink IO): absolute via cwd,
/// `.` dropped. `..` never appears here — `uri_to_path` rejects it earlier.
fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for comp in absolute.components() {
        match comp {
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct ServerState {
    root: Option<PathBuf>,
    git_ref: String,
    config_json: String,
    wasm_dir: String,
    supports_codelens_refresh: bool,
    /// The negotiated LSP position encoding for every range this server emits.
    position_encoding: PositionEncoding,
    /// uri → code lenses mapped from the last served SemanticDiff (the codeLens pull
    /// cache), already converted with the exact old/new buffers of that revision.
    lens_cache: HashMap<String, Vec<Value>>,
    /// uri → latest known buffer content (didOpen/didChange).
    content_cache: HashMap<String, String>,
    /// id source for server→client requests (codeLens refresh).
    next_server_id: i64,
}

/// LSP `PositionEncodingKind` this server can emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionEncoding {
    /// UTF-8 code units — the engine's native byte columns.
    Utf8,
    /// UTF-16 code units — the protocol default every client must support.
    Utf16,
}

impl PositionEncoding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
        }
    }
}

/// Pick the position encoding from `initialize` params (LSP 3.17
/// `general.positionEncodings`): `utf-8` when the client offers it, else `utf-16` (the
/// mandatory default — also chosen when the client offers only `utf-32`).
fn negotiate_position_encoding(params: &Value) -> PositionEncoding {
    let offers_utf8 = params
        .pointer("/capabilities/general/positionEncodings")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("utf-8")));
    if offers_utf8 {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

/// Call a core C-ABI handler in-process and unwrap its `{"ok", "result" | "error"}`
/// envelope. The error carries the core's `error_type` (e.g. `not_found`).
fn core_call(name: &str, args: &[Value]) -> Result<Value, (String, String)> {
    let envelope: Value = serde_json::from_str(&c_abi::dispatch(name, args))
        .map_err(|e| ("internal".to_owned(), format!("{name}: invalid core envelope: {e}")))?;
    if envelope.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(envelope.get("result").cloned().unwrap_or(Value::Null));
    }
    let kind = envelope.get("error_type").and_then(Value::as_str).unwrap_or("internal");
    let message = envelope.get("error").and_then(Value::as_str).unwrap_or("core call failed");
    Err((kind.to_owned(), format!("{name}: {message}")))
}

/// The ref baseline of `rel`, read strictly by the core (unresolvable refs and invalid
/// UTF-8 are errors). A path absent at a valid commit is an addition: empty baseline.
fn read_baseline(root: &Path, rel: &str, git_ref: &str) -> Result<String, String> {
    let root = root.to_string_lossy();
    match core_call("git_source_content", &[json!(root), json!(rel), json!(git_ref), json!(git_ref)]) {
        Ok(pair) => pair
            .get("old_content")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "git_source_content: missing old_content".to_owned()),
        Err((kind, _)) if kind == "not_found" => Ok(String::new()),
        Err((_, message)) => Err(message),
    }
}

/// Diff the live `content` of `rel` against the ref with the live handler's semantics,
/// returning the engine payload together with the EXACT baseline buffer it diffed, so
/// ranges can be mapped against the same sources the engine positioned.
fn diff_live_buffer(
    state: &ServerState,
    root: &Path,
    rel: &str,
    content: &str,
) -> Result<(Value, String), String> {
    let old_source = read_baseline(root, rel, &state.git_ref)?;
    let payload = proto::live_diff_contents_impl(
        &root.to_string_lossy(),
        rel,
        &old_source,
        content,
        &state.config_json,
        &state.wasm_dir,
    )?;
    let parsed = serde_json::from_str(&payload).map_err(|e| format!("invalid engine payload: {e}"))?;
    Ok((parsed, old_source))
}

/// STYLE_ONLY hints + parse-error warnings, positioned in `encoding`. Spans are always
/// validated against `new_source` by the core's source-aware mapper (a stale or foreign
/// buffer is an error, never a misplaced range); for `utf-8` the byte columns are
/// already the LSP character offsets.
fn diagnostics_for(
    diff: &Value,
    new_source: &str,
    encoding: PositionEncoding,
) -> Result<Vec<Value>, String> {
    let utf16 = core_call(
        "lsp_server_diagnostics_utf16",
        &[json!(diff.to_string()), json!(new_source)],
    )
    .map_err(|(_, m)| m)?;
    match encoding {
        PositionEncoding::Utf16 => into_array(utf16),
        PositionEncoding::Utf8 => Ok(shapes::semantic_diff_to_diagnostics_value(diff)),
    }
}

/// Refactoring-family code lenses, positioned in `encoding`: new-node anchors against
/// `new_source`, old-only anchors against `old_source` (side selection is the core's).
fn lenses_for(
    diff: &Value,
    old_source: &str,
    new_source: &str,
    encoding: PositionEncoding,
) -> Result<Vec<Value>, String> {
    let utf16 = core_call(
        "lsp_server_codelens_utf16",
        &[json!(diff.to_string()), json!(old_source), json!(new_source)],
    )
    .map_err(|(_, m)| m)?;
    match encoding {
        PositionEncoding::Utf16 => into_array(utf16),
        PositionEncoding::Utf8 => Ok(shapes::semantic_diff_to_codelens_value(diff)),
    }
}

fn into_array(value: Value) -> Result<Vec<Value>, String> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(format!("expected an array of LSP shapes, got {other}")),
    }
}

fn write_msg(out: &mut impl Write, msg: &Value) {
    let _ = out.write_all(&encode_message(msg));
    let _ = out.flush();
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    write_msg(out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn respond_error(out: &mut impl Write, id: &Value, code: i64, message: &str) {
    write_msg(
        out,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let git_ref = arg_value(&args, "--ref").unwrap_or_else(|| "HEAD".to_owned());
    let wasm_dir = resolve_wasm_dir(&args);

    let mut state = ServerState {
        root: None,
        git_ref,
        config_json: "{}".to_owned(),
        wasm_dir,
        supports_codelens_refresh: false,
        position_encoding: PositionEncoding::Utf16,
        lens_cache: HashMap::new(),
        content_cache: HashMap::new(),
        next_server_id: 1,
    };

    // Reader thread: bytes → frames → channel. Exits on EOF/fatal framing; dropping the
    // sender ends the main loop (#73: one thread, joined, nothing orphaned).
    let (tx, rx) = mpsc::channel::<Value>();
    let reader = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut decoder = FrameDecoder::new();
        let mut chunk = [0u8; 8_192];
        loop {
            let n = match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            decoder.feed(&chunk[..n]);
            loop {
                match decoder.next_event() {
                    Ok(Some(DecodeEvent::Frame(msg))) => {
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Ok(Some(DecodeEvent::MalformedFrame)) => {}
                    Ok(None) => break,
                    Err(_) => return, // fatal framing violation → close
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    'main: while let Ok(first) = rx.recv() {
        // Debounce by coalescing: drain whatever is already buffered, then keep only the
        // LATEST content per URI. Everything else is handled in arrival order.
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        let mut dirty: Vec<String> = Vec::new();
        for msg in &batch {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            if method.is_empty() {
                continue; // a response to one of OUR requests (codeLens refresh) — ignore
            }
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let id = msg.get("id");
            match method {
                "initialize" => {
                    let root_uri = params
                        .get("rootUri")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            params
                                .get("workspaceFolders")
                                .and_then(Value::as_array)
                                .and_then(|f| f.first())
                                .and_then(|f| f.get("uri"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        });
                    if let Some(uri) = root_uri {
                        if let Ok(path) = uri_to_path(&uri, None) {
                            // Repo config, same precedence as the live-server binary.
                            if let Ok(loaded) =
                                proto::live_load_project_config_impl(&path.to_string_lossy())
                            {
                                if loaded.trim() != "{}" {
                                    state.config_json = loaded;
                                }
                            }
                            state.root = Some(path);
                        }
                    }
                    state.supports_codelens_refresh = params
                        .pointer("/capabilities/workspace/codeLens/refreshSupport")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    state.position_encoding = negotiate_position_encoding(&params);
                    if let Some(id) = id {
                        respond(
                            &mut out,
                            id,
                            json!({
                                "capabilities": {
                                    "positionEncoding": state.position_encoding.as_str(),
                                    "textDocumentSync": 1,
                                    "codeLensProvider": {"resolveProvider": false},
                                },
                                "serverInfo": {"name": "intentumdiff-lsp", "version": "1.0.0"},
                            }),
                        );
                    }
                }
                "initialized" => {}
                "textDocument/didOpen" => {
                    let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
                    let text = params.pointer("/textDocument/text").and_then(Value::as_str);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        state.content_cache.insert(uri.to_owned(), text.to_owned());
                        if !dirty.iter().any(|u| u == uri) {
                            dirty.push(uri.to_owned());
                        }
                    }
                }
                "textDocument/didChange" => {
                    let uri = params.pointer("/textDocument/uri").and_then(Value::as_str);
                    // Full sync — the last (and only) item is the complete document.
                    let text = params
                        .get("contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|c| c.last())
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        state.content_cache.insert(uri.to_owned(), text.to_owned());
                        if !dirty.iter().any(|u| u == uri) {
                            dirty.push(uri.to_owned());
                        }
                    }
                }
                "textDocument/didClose" => {
                    if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                        state.lens_cache.remove(uri);
                        state.content_cache.remove(uri);
                        dirty.retain(|u| u != uri);
                    }
                }
                "textDocument/codeLens" => {
                    let uri = params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let lenses = state.lens_cache.get(uri).cloned().unwrap_or_default();
                    if let Some(id) = id {
                        respond(&mut out, id, Value::Array(lenses));
                    }
                }
                "intentumdiff/semanticDiff" => {
                    if let Some(id) = id {
                        let result = semantic_diff_request(&mut state, &params);
                        respond(&mut out, id, result);
                    }
                }
                "shutdown" => {
                    if let Some(id) = id {
                        respond(&mut out, id, Value::Null);
                    }
                }
                "exit" => break 'main,
                _ => {
                    if let Some(id) = id {
                        respond_error(&mut out, id, -32601, &format!("method not found: {method}"));
                    }
                }
            }
        }

        // Compute once per dirty URI with the final coalesced content.
        for uri in dirty {
            compute_and_push(&mut state, &mut out, &uri);
        }
    }

    drop(rx);
    let _ = reader.join();
}

/// `_compute_and_push`: diff the live buffer against the ref, map diagnostics + lenses
/// with that revision's exact old/new buffers, cache the lenses for codeLens, publish
/// diagnostics, and nudge the client's lens refresh when supported. Engine fallbacks,
/// mapping failures and out-of-root files publish nothing (the pygls `diff is None` path).
fn compute_and_push(state: &mut ServerState, out: &mut impl Write, uri: &str) {
    let Some(root) = state.root.clone() else { return };
    let Some(content) = state.content_cache.get(uri).cloned() else { return };
    let Ok(file_path) = uri_to_path(uri, Some(&root)) else { return };
    let Ok(rel) = file_path.strip_prefix(&root) else { return };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let Ok((parsed, old_source)) = diff_live_buffer(state, &root, &rel, &content) else {
        return;
    };
    let Some(diff) = parsed.get("diff") else { return };
    let encoding = state.position_encoding;
    let Ok(diagnostics) = diagnostics_for(diff, &content, encoding) else { return };
    let Ok(lenses) = lenses_for(diff, &old_source, &content, encoding) else { return };

    state.lens_cache.insert(uri.to_owned(), lenses);
    write_msg(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": diagnostics},
        }),
    );
    if state.supports_codelens_refresh {
        let id = state.next_server_id;
        state.next_server_id += 1;
        write_msg(
            out,
            &json!({"jsonrpc": "2.0", "id": id, "method": "workspace/codeLens/refresh",
                    "params": Value::Null}),
        );
    }
}

/// The custom `intentumdiff/semanticDiff` request: same-URI = live buffer (or on-disk file)
/// vs the ref; different URIs = workspace-contained two-file compare. Errors are in-band
/// `{"error": …}` objects, exactly like the pygls handler.
fn semantic_diff_request(state: &mut ServerState, params: &Value) -> Value {
    let old_uri = params.get("oldUri").and_then(Value::as_str).unwrap_or("");
    let new_uri = params.get("newUri").and_then(Value::as_str).unwrap_or("");
    if old_uri.is_empty() || new_uri.is_empty() {
        return json!({"error": "oldUri and newUri are required"});
    }
    let Some(root) = state.root.clone() else {
        return json!({"error": "intentumdiff/semanticDiff requires a valid workspace root"});
    };
    let compute = |state: &ServerState| -> Result<Value, String> {
        if old_uri == new_uri {
            let file_path = uri_to_path(new_uri, Some(&root))?;
            let content = match state.content_cache.get(new_uri) {
                Some(text) => text.clone(),
                None => std::fs::read_to_string(&file_path).map_err(|e| e.to_string())?,
            };
            let rel = file_path
                .strip_prefix(&root)
                .map_err(|_| "file is outside the workspace root".to_owned())?
                .to_string_lossy()
                .replace('\\', "/");
            diff_live_buffer(state, &root, &rel, &content).map(|(parsed, _)| parsed)
        } else {
            let old_path = uri_to_path(old_uri, Some(&root))?;
            let new_path = uri_to_path(new_uri, Some(&root))?;
            let old_content = std::fs::read_to_string(&old_path).map_err(|e| e.to_string())?;
            let new_content = std::fs::read_to_string(&new_path).map_err(|e| e.to_string())?;
            let filename = new_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let payload = proto::live_diff_contents_impl(
                &root.to_string_lossy(),
                &filename,
                &old_content,
                &new_content,
                &state.config_json,
                &state.wasm_dir,
            )?;
            Ok(serde_json::from_str(&payload).unwrap_or(Value::Null))
        }
    };
    match compute(state) {
        Ok(parsed) => {
            if let Some(diff) = parsed.get("diff") {
                diff.clone()
            } else if let Some(reason) = parsed.get("fallback").and_then(Value::as_str) {
                json!({"error": format!("native_fallback: {reason}")})
            } else {
                json!({"error": "Diff computation failed"})
            }
        }
        Err(e) => json!({"error": e}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_path_ports_the_python_guards() {
        // Traversal rejected.
        assert!(uri_to_path("file:///a/../etc/passwd", None).is_err());
        // Non-file schemes rejected.
        assert!(uri_to_path("https://example.com/x", None).is_err());
        // Percent-decoding.
        let p = uri_to_path("file:///tmp/a%20b.py", None).unwrap();
        assert!(p.to_string_lossy().contains("a b.py"));
        // Windows drive fix: "/C:/x" loses the leading slash.
        if cfg!(windows) {
            let p = uri_to_path("file:///C:/Users/x.py", None).unwrap();
            assert!(p.to_string_lossy().starts_with("C:"));
        }
    }

    fn style_diff(start: (u64, u64), end: (u64, u64)) -> Value {
        json!({
            "changes": [{
                "change_type": "STYLE_ONLY",
                "description": "style",
                "new_node": {"position": {
                    "start_line": start.0, "start_col": start.1,
                    "end_line": end.0, "end_col": end.1,
                }},
            }],
            "parse_errors": [],
        })
    }

    fn range_of(shape: &Value) -> ((i64, i64), (i64, i64)) {
        let r = &shape["range"];
        let p = |k: &str| (r[k]["line"].as_i64().unwrap(), r[k]["character"].as_i64().unwrap());
        (p("start"), p("end"))
    }

    #[test]
    fn diagnostics_use_utf16_code_units_by_default() {
        // ECO-04: `été` is 5 UTF-8 bytes but 3 UTF-16 code units.
        let diags = diagnostics_for(&style_diff((0, 0), (0, 5)), "été", PositionEncoding::Utf16).unwrap();
        assert_eq!(range_of(&diags[0]), ((0, 0), (0, 3)));
        // Astral-plane emoji: 4 bytes, 2 UTF-16 units.
        let diags = diagnostics_for(&style_diff((0, 1), (0, 6)), "a😀bc", PositionEncoding::Utf16).unwrap();
        assert_eq!(range_of(&diags[0]), ((0, 1), (0, 4)));
        // Combining mark: e + U+0301 (2 bytes) is two UTF-16 units, not one grapheme.
        let diags = diagnostics_for(&style_diff((0, 0), (0, 4)), "e\u{301}xy", PositionEncoding::Utf16).unwrap();
        assert_eq!(range_of(&diags[0]), ((0, 0), (0, 3)));
        // Multiline, CRLF: each line is measured on its own text.
        let diags = diagnostics_for(&style_diff((0, 2), (1, 3)), "é\r\nüx", PositionEncoding::Utf16).unwrap();
        assert_eq!(range_of(&diags[0]), ((0, 1), (1, 2)));
    }

    #[test]
    fn negotiated_utf8_keeps_the_engine_byte_columns() {
        let diags = diagnostics_for(&style_diff((0, 0), (0, 5)), "été", PositionEncoding::Utf8).unwrap();
        assert_eq!(range_of(&diags[0]), ((0, 0), (0, 5)));
    }

    #[test]
    fn stale_or_foreign_buffers_are_rejected_not_misplaced() {
        // A span that does not fit the buffer (wrong revision) must fail in either encoding.
        for enc in [PositionEncoding::Utf16, PositionEncoding::Utf8] {
            assert!(diagnostics_for(&style_diff((0, 0), (0, 9)), "été", enc).is_err());
            assert!(diagnostics_for(&style_diff((3, 0), (3, 1)), "été", enc).is_err());
            // Byte column 1 splits `é`.
            assert!(diagnostics_for(&style_diff((0, 1), (0, 3)), "été", enc).is_err());
        }
    }

    #[test]
    fn code_lenses_map_each_node_against_its_own_side() {
        let diff = json!({
            "changes": [
                // Old-only anchor: measured on the OLD source.
                {"change_type": "MOVE", "description": "moved",
                 "new_node": null,
                 "old_node": {"position": {"start_line": 0, "start_col": 0, "end_line": 0, "end_col": 4}}},
                // New anchor: measured on the NEW source.
                {"change_type": "REFACTORING", "refactoring_kind": "RENAME", "description": "renamed",
                 "new_node": {"position": {"start_line": 0, "start_col": 2, "end_line": 0, "end_col": 6}},
                 "old_node": {"position": {"start_line": 0, "start_col": 0, "end_line": 0, "end_col": 1}}},
            ],
            "parse_errors": [],
        });
        let old = "ééé";
        let new = "ab😀";
        let lenses = lenses_for(&diff, old, new, PositionEncoding::Utf16).unwrap();
        assert_eq!(lenses.len(), 2);
        assert_eq!(range_of(&lenses[0]), ((0, 0), (0, 2)));
        assert_eq!(range_of(&lenses[1]), ((0, 2), (0, 4)));
        assert!(lenses[1]["command"]["title"].as_str().unwrap().contains("RENAME"));
        // Swapping the buffers puts the old anchor's byte 4 inside the emoji → refused.
        assert!(lenses_for(&diff, new, old, PositionEncoding::Utf16).is_err());
        // utf-8: byte columns pass through.
        let lenses = lenses_for(&diff, old, new, PositionEncoding::Utf8).unwrap();
        assert_eq!(range_of(&lenses[0]), ((0, 0), (0, 4)));
    }

    #[test]
    fn position_encoding_negotiation_follows_lsp_3_17() {
        let neg = |caps: Value| negotiate_position_encoding(&json!({"capabilities": caps}));
        assert_eq!(neg(json!({})), PositionEncoding::Utf16);
        assert_eq!(neg(json!({"general": {"positionEncodings": ["utf-16"]}})), PositionEncoding::Utf16);
        assert_eq!(neg(json!({"general": {"positionEncodings": ["utf-32"]}})), PositionEncoding::Utf16);
        assert_eq!(
            neg(json!({"general": {"positionEncodings": ["utf-16", "utf-8"]}})),
            PositionEncoding::Utf8
        );
        assert_eq!(PositionEncoding::Utf8.as_str(), "utf-8");
        assert_eq!(PositionEncoding::Utf16.as_str(), "utf-16");
    }

    #[test]
    fn containment_guard_rejects_out_of_root_paths() {
        let root = std::env::temp_dir();
        let inside = format!("file:///{}", root.join("f.py").to_string_lossy().replace('\\', "/"));
        assert!(uri_to_path(&inside, Some(&root)).is_ok());
        let outside = if cfg!(windows) { "file:///C:/other-root/f.py" } else { "file:///other-root/f.py" };
        assert!(uri_to_path(outside, Some(&root)).is_err());
    }
}
