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
//! - **No stale results**: every publish carries the document version it was computed
//!   from; a `didChange` whose version is not newer than the buffer's is dropped. A
//!   revision the engine declines (`{"fallback": …}`) publishes EMPTY diagnostics and drops
//!   its lenses; a revision whose compute fails publishes one Information diagnostic that
//!   says why (never relabelled as a clean diff) and drops its lenses; `didClose` publishes
//!   empty diagnostics and drops lenses. Lens drops trigger `workspace/codeLens/refresh`
//!   when the client supports it.
//! - **Bounded memory** (see [`Limits`]): the reader→main queue holds at most
//!   `max_pending_messages` frames / `max_pending_bytes` of estimated payload (one frame
//!   larger than that is still admitted alone). A full queue blocks the reader — the
//!   client's writes back-pressure through the pipe; nothing is dropped. Before blocking, a
//!   queued full-sync `didChange` is replaced in place by a newer one for the same URI when
//!   no request or other message for that URI follows it (obsolete revisions are coalesced
//!   before they accumulate). Open buffers are capped at `max_open_documents` /
//!   `max_document_bytes`; beyond that the least-recently-updated documents are evicted —
//!   their diagnostics are cleared and lenses dropped — and come back on their next
//!   full-sync change or re-open.
//! - **Positions are source-aware**: engine `NodePosition` columns are UTF-8 byte offsets.
//!   The server reads the ref baseline ONCE, diffs exactly that old buffer against the live
//!   buffer, and maps ranges through the core's source-aware helpers with those same two
//!   buffers. The position encoding is negotiated (`general.positionEncodings`): `utf-8`
//!   when the client offers it (byte columns are then already exact), otherwise the
//!   protocol default `utf-16`. No conversion algorithm lives in this binary.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

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

/// Process-wide memory ceilings. Defaults suit an editor session; each is overridable on
/// the command line (`--max-pending-messages`, `--max-pending-bytes`,
/// `--max-open-documents`, `--max-document-bytes`).
#[derive(Clone, Copy, Debug)]
struct Limits {
    max_pending_messages: usize,
    max_pending_bytes: usize,
    max_open_documents: usize,
    max_document_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pending_messages: 1_024,
            max_pending_bytes: 64 * 1024 * 1024,
            max_open_documents: 1_024,
            max_document_bytes: 256 * 1024 * 1024,
        }
    }
}

impl Limits {
    fn from_args(args: &[String]) -> Self {
        let mut limits = Self::default();
        let set = |flag: &str, slot: &mut usize| {
            if let Some(v) = arg_value(args, flag).and_then(|v| v.parse::<usize>().ok()) {
                *slot = v.max(1);
            }
        };
        set("--max-pending-messages", &mut limits.max_pending_messages);
        set("--max-pending-bytes", &mut limits.max_pending_bytes);
        set("--max-open-documents", &mut limits.max_open_documents);
        set("--max-document-bytes", &mut limits.max_document_bytes);
        limits
    }
}

/// Estimated retained size of a queued frame: the document text dominates for
/// didOpen/didChange, so count it directly (plus envelope slack) instead of re-serialising.
fn estimate_size(msg: &Value) -> usize {
    const ENVELOPE: usize = 256;
    let text = msg
        .pointer("/params/textDocument/text")
        .and_then(Value::as_str)
        .map(str::len)
        .or_else(|| {
            msg.pointer("/params/contentChanges")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(|c| c.get("text")?.as_str()).map(str::len).sum())
        });
    match text {
        Some(len) => len + ENVELOPE,
        None => msg.to_string().len(),
    }
}

fn method_of(msg: &Value) -> &str {
    msg.get("method").and_then(Value::as_str).unwrap_or("")
}

fn uri_of(msg: &Value) -> Option<&str> {
    msg.pointer("/params/textDocument/uri").and_then(Value::as_str)
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<(Value, usize)>,
    bytes: usize,
    closed: bool,
}

/// The bounded reader→main queue (replaces an unbounded mpsc channel).
struct PendingQueue {
    state: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
    limits: Limits,
}

impl PendingQueue {
    fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState::default()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            limits,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Try to fold a full-sync `didChange` into a queued one for the same URI. Returns
    /// `None` when it was absorbed (replaced the queued revision, or was itself older and
    /// would be ignored anyway), else gives the message back.
    fn coalesce(q: &mut QueueState, msg: Value, size: usize) -> Option<(Value, usize)> {
        if method_of(&msg) != "textDocument/didChange" {
            return Some((msg, size));
        }
        let Some(uri) = uri_of(&msg).map(str::to_owned) else { return Some((msg, size)) };
        // Walk back from the tail: anything that is a request, or touches this URI and is
        // not a didChange, pins the order — stop.
        for i in (0..q.items.len()).rev() {
            let queued = &q.items[i].0;
            let touches = uri_of(queued) == Some(uri.as_str());
            if touches && method_of(queued) == "textDocument/didChange" {
                let version = |m: &Value| m.pointer("/params/textDocument/version").and_then(Value::as_i64);
                return match (version(queued), version(&msg)) {
                    (Some(old), Some(new)) if new <= old => None, // stale: main would drop it
                    _ => {
                        let old_size = q.items[i].1;
                        q.items[i] = (msg, size);
                        q.bytes = q.bytes - old_size + size;
                        None
                    }
                };
            }
            if touches || queued.get("id").is_some() {
                break;
            }
        }
        Some((msg, size))
    }

    /// Enqueue, blocking while the queue is full (back-pressure). Never drops a message
    /// except an obsolete full-sync revision superseded per [`Self::coalesce`].
    fn push(&self, msg: Value) {
        let size = estimate_size(&msg);
        let mut q = self.lock();
        let Some((mut msg, size)) = Self::coalesce(&mut q, msg, size) else {
            self.not_empty.notify_one();
            return;
        };
        loop {
            let full = q.items.len() >= self.limits.max_pending_messages
                || q.bytes + size > self.limits.max_pending_bytes;
            if q.items.is_empty() || !full {
                break;
            }
            q = self.not_full.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
            // The queue changed while waiting: the revision may now coalesce after all.
            match Self::coalesce(&mut q, msg, size) {
                None => {
                    self.not_empty.notify_one();
                    return;
                }
                Some((back, _)) => msg = back,
            }
        }
        q.bytes += size;
        q.items.push_back((msg, size));
        self.not_empty.notify_one();
    }

    /// The reader is gone (EOF / fatal framing).
    fn close(&self) {
        self.lock().closed = true;
        self.not_empty.notify_all();
    }

    /// Block until frames are queued, then take ALL of them (one coalescing batch).
    /// `None` once the reader closed and the queue is drained.
    fn recv_batch(&self) -> Option<Vec<Value>> {
        let mut q = self.lock();
        while q.items.is_empty() {
            if q.closed {
                return None;
            }
            q = self.not_empty.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let batch: Vec<Value> = q.items.drain(..).map(|(m, _)| m).collect();
        q.bytes = 0;
        self.not_full.notify_all();
        Some(batch)
    }
}

/// One open document: full text (full sync) and the client's document version.
struct Doc {
    text: String,
    version: Option<i64>,
    /// Monotonic "last updated" stamp for LRU eviction.
    touched: u64,
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
    /// uri → latest known buffer (didOpen/didChange), with its LSP document version.
    content_cache: HashMap<String, Doc>,
    /// id source for server→client requests (codeLens refresh).
    next_server_id: i64,
    /// `shutdown` was received: every later request except `exit` is InvalidRequest.
    shutdown_requested: bool,
    limits: Limits,
    /// Sum of `content_cache` text lengths.
    cached_bytes: usize,
    /// Source of `Doc::touched` stamps.
    clock: u64,
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

impl ServerState {
    fn new(git_ref: String, wasm_dir: String) -> Self {
        Self {
            root: None,
            git_ref,
            config_json: "{}".to_owned(),
            wasm_dir,
            supports_codelens_refresh: false,
            position_encoding: PositionEncoding::Utf16,
            lens_cache: HashMap::new(),
            content_cache: HashMap::new(),
            next_server_id: 1,
            shutdown_requested: false,
            limits: Limits::default(),
            cached_bytes: 0,
            clock: 0,
        }
    }

    /// Insert/replace a buffer, keeping `cached_bytes` exact.
    fn store_doc(&mut self, uri: &str, text: &str, version: Option<i64>) {
        self.clock += 1;
        let doc = Doc { text: text.to_owned(), version, touched: self.clock };
        self.cached_bytes += doc.text.len();
        if let Some(old) = self.content_cache.insert(uri.to_owned(), doc) {
            self.cached_bytes -= old.text.len();
        }
    }

    fn remove_doc(&mut self, uri: &str) {
        if let Some(old) = self.content_cache.remove(uri) {
            self.cached_bytes -= old.text.len();
        }
    }

    /// Evict least-recently-updated documents (never `keep`) until the open-document and
    /// cached-byte ceilings hold. Returns the evicted URIs.
    fn evict_over_limits(&mut self, keep: &str) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.content_cache.len() > self.limits.max_open_documents
            || self.cached_bytes > self.limits.max_document_bytes
        {
            let victim = self
                .content_cache
                .iter()
                .filter(|(uri, _)| uri.as_str() != keep)
                .min_by_key(|(_, doc)| doc.touched)
                .map(|(uri, _)| uri.clone());
            let Some(victim) = victim else { break }; // only `keep` is left
            self.remove_doc(&victim);
            self.lens_cache.remove(&victim);
            evicted.push(victim);
        }
        evicted
    }
}

/// Apply the document ceilings after `uri` was stored: evicted documents lose their
/// diagnostics (empty publish) and lenses (refresh) and leave the dirty set.
fn enforce_document_limits(
    state: &mut ServerState,
    out: &mut impl Write,
    dirty: &mut Vec<String>,
    uri: &str,
) {
    let evicted = state.evict_over_limits(uri);
    if evicted.is_empty() {
        return;
    }
    for victim in &evicted {
        dirty.retain(|u| u != victim);
        publish_diagnostics(out, victim, None, &[]);
    }
    request_codelens_refresh(state, out);
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

    let mut state = ServerState::new(git_ref, wasm_dir);
    state.limits = Limits::from_args(&args);

    // Reader thread: bytes → frames → channel. Exits on EOF/fatal framing; dropping the
    // sender ends the main loop (#73: one thread, joined, nothing orphaned). A blocking
    // stdin read cannot be cancelled portably, so protocol `exit` does NOT join it: the
    // process terminates instead (see the end of `main`).
    let queue = PendingQueue::new(state.limits);
    let reader_queue = Arc::clone(&queue);
    let reader = std::thread::spawn(move || {
        let queue = reader_queue;
        // Closing on every exit path wakes the main loop at EOF / fatal framing.
        struct CloseOnDrop(Arc<PendingQueue>);
        impl Drop for CloseOnDrop {
            fn drop(&mut self) {
                self.0.close();
            }
        }
        let _close = CloseOnDrop(Arc::clone(&queue));
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
                    Ok(Some(DecodeEvent::Frame(msg))) => queue.push(msg),
                    Ok(Some(DecodeEvent::MalformedFrame)) => {}
                    Ok(None) => break,
                    Err(_) => return, // fatal framing violation → close
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // Set by protocol `exit`: 0 after `shutdown`, 1 without (LSP 3.17).
    let mut exit_code: Option<i32> = None;

    'main: while let Some(batch) = queue.recv_batch() {
        // Debounce by coalescing: take whatever is already buffered, then keep only the
        // LATEST content per URI. Everything else is handled in arrival order.
        let mut dirty: Vec<String> = Vec::new();
        for mut msg in batch {
            let method = method_of(&msg).to_owned();
            let method = method.as_str();
            if method.is_empty() {
                continue; // a response to one of OUR requests (codeLens refresh) — ignore
            }
            // Move (not clone) the params out: a full-sync text is the bulk of the frame.
            let params = msg.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let id = msg.get("id");
            if state.shutdown_requested && method != "exit" {
                if let Some(id) = id {
                    respond_error(&mut out, id, -32600, "server is shutting down");
                }
                continue;
            }
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
                    let version = params.pointer("/textDocument/version").and_then(Value::as_i64);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        // (Re)open starts a fresh version history for the URI.
                        state.store_doc(uri, text, version);
                        if !dirty.iter().any(|u| u == uri) {
                            dirty.push(uri.to_owned());
                        }
                        enforce_document_limits(&mut state, &mut out, &mut dirty, uri);
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
                    let version = params.pointer("/textDocument/version").and_then(Value::as_i64);
                    if let (Some(uri), Some(text)) = (uri, text) {
                        if accept_change(&mut state, uri, text, version) {
                            if !dirty.iter().any(|u| u == uri) {
                                dirty.push(uri.to_owned());
                            }
                            enforce_document_limits(&mut state, &mut out, &mut dirty, uri);
                        }
                    }
                }
                "textDocument/didClose" => {
                    if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                        state.remove_doc(uri);
                        dirty.retain(|u| u != uri);
                        let had_lenses = state.lens_cache.remove(uri).is_some();
                        publish_diagnostics(&mut out, uri, None, &[]);
                        if had_lenses {
                            request_codelens_refresh(&mut state, &mut out);
                        }
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
                    state.shutdown_requested = true;
                    if let Some(id) = id {
                        respond(&mut out, id, Value::Null);
                    }
                }
                "exit" => {
                    exit_code = Some(if state.shutdown_requested { 0 } else { 1 });
                    break 'main;
                }
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

    let _ = out.flush();
    drop(out);
    if let Some(code) = exit_code {
        // Protocol exit: the reader may be parked in a blocking stdin read that only EOF
        // would release. Terminating the process ends it immediately and leaves nothing
        // orphaned (it is a thread of this process); stdout is already flushed.
        std::process::exit(code);
    }
    // EOF / fatal framing: the reader has already returned (that is what closed the
    // queue), so this join is immediate.
    let _ = reader.join();
}

/// Apply a full-sync `didChange` unless it is stale: a change whose version is not newer
/// than the buffer's (late / out-of-order delivery) is dropped. Returns whether the buffer
/// changed. A change for a URI that was never opened is accepted as its first revision.
fn accept_change(state: &mut ServerState, uri: &str, text: &str, version: Option<i64>) -> bool {
    if let Some(doc) = state.content_cache.get(uri) {
        if let (Some(current), Some(incoming)) = (doc.version, version) {
            if incoming <= current {
                return false;
            }
        }
    }
    state.store_doc(uri, text, version);
    true
}

/// What one compute produced for a document revision.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Nothing is ever published for this URI (no workspace root / outside it).
    NotServed,
    /// A semantic diff, already mapped for the negotiated encoding.
    Diff { diagnostics: Vec<Value>, lenses: Vec<Value> },
    /// The engine declined this revision (`{"fallback": reason}`).
    Declined(String),
    /// The compute or the mapping failed.
    Failed(String),
}

/// `_compute_and_push` (compute half): diff the live buffer against the ref and map
/// diagnostics + lenses with that revision's exact old/new buffers.
fn compute_outcome(state: &ServerState, uri: &str, content: &str) -> Outcome {
    let Some(root) = state.root.clone() else { return Outcome::NotServed };
    let Ok(file_path) = uri_to_path(uri, Some(&root)) else { return Outcome::NotServed };
    let Ok(rel) = file_path.strip_prefix(&root) else { return Outcome::NotServed };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let (parsed, old_source) = match diff_live_buffer(state, &root, &rel, content) {
        Ok(ok) => ok,
        Err(e) => return Outcome::Failed(e),
    };
    let Some(diff) = parsed.get("diff") else {
        return match parsed.get("fallback").and_then(Value::as_str) {
            Some(reason) => Outcome::Declined(reason.to_owned()),
            None => Outcome::Failed("engine returned neither a diff nor a fallback".to_owned()),
        };
    };
    let encoding = state.position_encoding;
    let mapped = diagnostics_for(diff, content, encoding)
        .and_then(|d| lenses_for(diff, &old_source, content, encoding).map(|l| (d, l)));
    match mapped {
        Ok((diagnostics, lenses)) => Outcome::Diff { diagnostics, lenses },
        Err(e) => Outcome::Failed(format!("position mapping failed: {e}")),
    }
}

/// The Information diagnostic that keeps a failed compute's provenance visible.
fn failure_diagnostic(reason: &str) -> Value {
    json!({
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
        "severity": 3,
        "message": format!("Semantic diff unavailable for this revision: {reason}"),
        "source": "intentumdiff",
    })
}

fn publish_diagnostics(out: &mut impl Write, uri: &str, version: Option<i64>, diagnostics: &[Value]) {
    let mut params = json!({"uri": uri, "diagnostics": diagnostics});
    if let Some(version) = version {
        params["version"] = json!(version);
    }
    write_msg(
        out,
        &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}),
    );
}

fn request_codelens_refresh(state: &mut ServerState, out: &mut impl Write) {
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

/// `_compute_and_push`: compute the buffer's current revision, then replace EVERYTHING
/// previously published for the URI with that revision's results — cache lenses for the
/// codeLens pull, publish versioned diagnostics, and nudge the client's lens refresh.
fn compute_and_push(state: &mut ServerState, out: &mut impl Write, uri: &str) {
    let Some(doc) = state.content_cache.get(uri) else { return };
    let (content, version) = (doc.text.clone(), doc.version);
    let outcome = compute_outcome(state, uri, &content);
    apply_outcome(state, out, uri, version, outcome);
}

fn apply_outcome(
    state: &mut ServerState,
    out: &mut impl Write,
    uri: &str,
    version: Option<i64>,
    outcome: Outcome,
) {
    match outcome {
        Outcome::NotServed => {}
        Outcome::Diff { diagnostics, lenses } => {
            state.lens_cache.insert(uri.to_owned(), lenses);
            publish_diagnostics(out, uri, version, &diagnostics);
            request_codelens_refresh(state, out);
        }
        Outcome::Declined(_) | Outcome::Failed(_) => {
            let had_lenses = state.lens_cache.remove(uri).is_some();
            let diagnostics = match &outcome {
                Outcome::Failed(reason) => vec![failure_diagnostic(reason)],
                _ => Vec::new(),
            };
            publish_diagnostics(out, uri, version, &diagnostics);
            if had_lenses {
                request_codelens_refresh(state, out);
            }
        }
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
                Some(doc) => doc.text.clone(),
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

    /// Decode every frame the server wrote into `out`.
    fn frames(out: &[u8]) -> Vec<Value> {
        let mut decoder = FrameDecoder::new();
        decoder.feed(out);
        let mut msgs = Vec::new();
        while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
            msgs.push(msg);
        }
        msgs
    }

    #[test]
    fn stale_and_out_of_order_changes_are_dropped() {
        let mut state = ServerState::new("HEAD".into(), String::new());
        assert!(accept_change(&mut state, "u", "v1", Some(1)));
        assert!(accept_change(&mut state, "u", "v3", Some(3)));
        assert!(!accept_change(&mut state, "u", "v2", Some(2)), "late v2 must be dropped");
        assert!(!accept_change(&mut state, "u", "v3-dup", Some(3)), "replayed v3 must be dropped");
        assert_eq!(state.content_cache["u"].text, "v3");
        assert_eq!(state.content_cache["u"].version, Some(3));
        // Unversioned changes cannot be ordered, so they apply.
        assert!(accept_change(&mut state, "u", "nv", None));
    }

    #[test]
    fn declined_and_failed_revisions_replace_previous_results() {
        let mut state = ServerState::new("HEAD".into(), String::new());
        state.supports_codelens_refresh = true;
        let lens = json!({"range": {}, "command": {"title": "t", "command": ""}});
        let mut out = Vec::new();
        apply_outcome(&mut state, &mut out, "u", Some(1),
                      Outcome::Diff { diagnostics: vec![json!({"x": 1})], lenses: vec![lens] });
        let msgs = frames(&out);
        assert_eq!(msgs[0]["params"]["version"], 1);
        assert_eq!(msgs[0]["params"]["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[1]["method"], "workspace/codeLens/refresh");
        assert_eq!(state.lens_cache["u"].len(), 1);

        // Declined: empty diagnostics for v2, lenses dropped, refresh requested.
        let mut out = Vec::new();
        apply_outcome(&mut state, &mut out, "u", Some(2), Outcome::Declined("no parser".into()));
        let msgs = frames(&out);
        assert_eq!(msgs[0]["params"], json!({"uri": "u", "version": 2, "diagnostics": []}));
        assert_eq!(msgs[1]["method"], "workspace/codeLens/refresh");
        assert!(!state.lens_cache.contains_key("u"));

        // Failed: one Information diagnostic carrying the reason, never an empty "clean" set.
        let mut out = Vec::new();
        apply_outcome(&mut state, &mut out, "u", Some(3), Outcome::Failed("bad ref".into()));
        let msgs = frames(&out);
        assert_eq!(msgs.len(), 1, "no lenses left to refresh");
        let diags = msgs[0]["params"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["severity"], 3);
        assert!(diags[0]["message"].as_str().unwrap().contains("bad ref"));

        // NotServed publishes nothing.
        let mut out = Vec::new();
        apply_outcome(&mut state, &mut out, "u", Some(4), Outcome::NotServed);
        assert!(out.is_empty());
    }

    fn change(uri: &str, version: i64, text: &str) -> Value {
        json!({"jsonrpc": "2.0", "method": "textDocument/didChange",
               "params": {"textDocument": {"uri": uri, "version": version},
                          "contentChanges": [{"text": text}]}})
    }

    fn small_limits() -> Limits {
        Limits { max_pending_messages: 2, max_pending_bytes: 4_096, ..Limits::default() }
    }

    #[test]
    fn queue_coalesces_obsolete_full_sync_revisions_before_accumulating() {
        let q = PendingQueue::new(Limits::default());
        q.push(change("u", 1, "u1"));
        q.push(change("v", 1, "v1"));
        q.push(change("u", 2, "u2"));
        q.push(change("u", 1, "late")); // older than the queued u2 → absorbed
        let batch = q.recv_batch().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["params"]["contentChanges"][0]["text"], "u2");
        assert_eq!(batch[1]["params"]["contentChanges"][0]["text"], "v1");
        assert_eq!(q.lock().bytes, 0);
    }

    #[test]
    fn queue_never_reorders_across_requests_or_other_messages_for_the_uri() {
        let q = PendingQueue::new(Limits::default());
        q.push(change("u", 1, "u1"));
        q.push(json!({"jsonrpc": "2.0", "id": 7, "method": "intentumdiff/semanticDiff",
                      "params": {"oldUri": "u", "newUri": "u"}}));
        q.push(change("u", 2, "u2"));
        q.push(json!({"jsonrpc": "2.0", "method": "textDocument/didClose",
                      "params": {"textDocument": {"uri": "u"}}}));
        q.push(change("u", 3, "u3"));
        assert_eq!(q.recv_batch().unwrap().len(), 5);
    }

    #[test]
    fn full_queue_blocks_the_reader_without_dropping_requests() {
        let q = PendingQueue::new(small_limits());
        let producer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                for id in 0..50 {
                    q.push(json!({"jsonrpc": "2.0", "id": id, "method": "textDocument/codeLens",
                                  "params": {"textDocument": {"uri": "u"}}}));
                }
                q.close();
            })
        };
        let mut ids = Vec::new();
        while let Some(batch) = q.recv_batch() {
            assert!(batch.len() <= 2, "queue exceeded its message ceiling: {}", batch.len());
            ids.extend(batch.iter().map(|m| m["id"].as_i64().unwrap()));
        }
        producer.join().unwrap();
        assert_eq!(ids, (0..50).collect::<Vec<_>>(), "every request must arrive, in order");
    }

    #[test]
    fn byte_ceiling_applies_but_one_oversize_frame_is_admitted_alone() {
        let q = PendingQueue::new(small_limits());
        let big = "x".repeat(10_000);
        q.push(change("a", 1, &big)); // empty queue: admitted despite the byte ceiling
        let (tx, rx) = std::sync::mpsc::channel();
        let producer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                q.push(change("b", 1, "small"));
                tx.send(()).unwrap();
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(),
            "a push over the byte ceiling must block"
        );
        assert_eq!(q.recv_batch().unwrap().len(), 1);
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("drain must unblock the reader");
        producer.join().unwrap();
        assert_eq!(q.recv_batch().unwrap().len(), 1);
        q.close();
        assert!(q.recv_batch().is_none());
    }

    #[test]
    fn document_cache_evicts_least_recently_updated_and_clears_their_results() {
        let mut state = ServerState::new("HEAD".into(), String::new());
        state.limits = Limits { max_open_documents: 2, max_document_bytes: 10, ..Limits::default() };
        state.supports_codelens_refresh = true;
        let mut dirty = Vec::new();
        let mut out = Vec::new();
        for uri in ["a", "b"] {
            state.store_doc(uri, "123", Some(1));
            state.lens_cache.insert(uri.to_owned(), vec![]);
            dirty.push(uri.to_owned());
            enforce_document_limits(&mut state, &mut out, &mut dirty, uri);
        }
        assert!(out.is_empty());
        assert!(accept_change(&mut state, "a", "1234", Some(2))); // a is now newest
        state.store_doc("c", "1", Some(1));
        dirty.push("c".to_owned());
        enforce_document_limits(&mut state, &mut out, &mut dirty, "c");
        let msgs = frames(&out);
        assert_eq!(msgs[0]["params"], json!({"uri": "b", "diagnostics": []}));
        assert_eq!(msgs[1]["method"], "workspace/codeLens/refresh");
        assert!(!state.content_cache.contains_key("b") && !state.lens_cache.contains_key("b"));
        assert_eq!(dirty, vec!["a".to_owned(), "c".to_owned()]);
        assert_eq!(state.cached_bytes, 5);

        // Byte ceiling: one big document pushes the others out but is itself kept.
        let mut out = Vec::new();
        state.store_doc("big", "0123456789abc", Some(1));
        enforce_document_limits(&mut state, &mut out, &mut dirty, "big");
        assert_eq!(state.content_cache.len(), 1);
        assert_eq!(state.cached_bytes, 13);
        // Close releases everything.
        state.remove_doc("big");
        assert_eq!(state.cached_bytes, 0);
    }

    #[test]
    fn limits_parse_from_the_command_line() {
        let args: Vec<String> = ["bin", "--max-open-documents", "3", "--max-pending-bytes", "0"]
            .iter().map(|s| s.to_string()).collect();
        let limits = Limits::from_args(&args);
        assert_eq!(limits.max_open_documents, 3);
        assert_eq!(limits.max_pending_bytes, 1, "zero is clamped to one");
        assert_eq!(limits.max_pending_messages, Limits::default().max_pending_messages);
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
