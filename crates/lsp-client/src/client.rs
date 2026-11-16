//! The tokio LSP client (#100 S2 slice 2), ported from `AsyncLspClient`
//! (`src/intentumdiff/lsp/client.py`).
//!
//! Transport-generic: [`LspClient::connect_io`] takes any `AsyncRead`/`AsyncWrite` pair
//! (tests use `tokio::io::duplex` — no network); [`LspClient::connect_stdio`] spawns the
//! server subprocess with the #88 env hardening (PATH prepend, `.cmd`/`.bat` via
//! `cmd.exe /c`) and a capped stderr ring; [`LspClient::connect_tcp`] attaches to an
//! already-running server.
//!
//! Concurrency model mirrors the Python client: a background reader task decodes frames
//! (via the sans-IO [`FrameDecoder`](crate::FrameDecoder)) and resolves per-request
//! oneshot futures from a pending map. Every incoming envelope is classified BEFORE the
//! pending map is consulted (server request ids are an independent id space): only a
//! well-formed response (`id` + exactly one of `result`/`error`, no `method`) resolves a
//! pending request; notifications are discarded; server-initiated requests are answered
//! with a JSON-RPC `MethodNotFound` error (and mixed method+result envelopes with
//! `InvalidRequest`). A fatal framing violation or EOF cancels every outstanding request.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::{encode_message, notification, request, DecodeEvent, FrameDecoder, STDERR_RING_BYTES};

/// Client-facing failures, mirroring `LspConnectionError` / `LspTimeoutError` plus the
/// in-band JSON-RPC error responses (`RuntimeError` in Python).
#[derive(Debug)]
pub enum LspError {
    /// Could not connect / the connection is gone.
    Connection(String),
    /// A request exceeded the configured timeout (non-fatal — degrade gracefully).
    Timeout(String),
    /// The server answered with a JSON-RPC error object.
    Server { code: i64, message: String },
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) | Self::Timeout(msg) => write!(f, "{msg}"),
            Self::Server { code, message } => write!(f, "LSP error {code}: {message}"),
        }
    }
}

impl std::error::Error for LspError {}

/// `_subprocess_env`: the caller's environment with *prepend_dir* put at the front of
/// PATH (unless already present). The Python client prepends the venv's `Scripts`/`bin`;
/// this crate is venv-agnostic, so the caller chooses the directory.
pub fn env_with_path_prepend(prepend_dir: &Path) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let dir = prepend_dir.as_os_str().to_owned();
    let mut found_path = false;
    for (key, value) in env.iter_mut() {
        if key.to_string_lossy().eq_ignore_ascii_case("PATH") {
            found_path = true;
            let current = value.to_string_lossy().into_owned();
            let already = current
                .split(sep)
                .any(|part| Path::new(part) == prepend_dir);
            if !already {
                let mut joined = dir.clone();
                joined.push(sep);
                joined.push(&current);
                *value = joined;
            }
            break;
        }
    }
    if !found_path {
        env.push((OsString::from("PATH"), dir));
    }
    env
}

/// `_resolve_command`: resolve `cmd[0]` on *path_value* (a PATH-style string). On Windows,
/// `.cmd`/`.bat` entry-point wrappers cannot be spawned directly — run them through
/// `cmd.exe /c`. An unresolvable command is returned unchanged so the spawn raises a clear
/// not-found error, exactly as the Python client does.
pub fn resolve_command(cmd: &[String], path_value: &str) -> Vec<String> {
    let Some(first) = cmd.first() else {
        return cmd.to_vec();
    };
    let Some(resolved) = which_on(first, path_value) else {
        return cmd.to_vec();
    };
    let lower = resolved.to_string_lossy().to_lowercase();
    if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
        let mut out = vec!["cmd.exe".to_owned(), "/c".to_owned(), resolved.to_string_lossy().into_owned()];
        out.extend(cmd[1..].iter().cloned());
        return out;
    }
    let mut out = vec![resolved.to_string_lossy().into_owned()];
    out.extend(cmd[1..].iter().cloned());
    out
}

/// Minimal `shutil.which` over an explicit PATH string. On Windows, tries the
/// conventional launcher extensions when the name has none.
fn which_on(name: &str, path_value: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        // Path-like commands are used as-is when they exist.
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let sep = if cfg!(windows) { ';' } else { ':' };
    let has_ext = candidate.extension().is_some();
    let exts: &[&str] = if cfg!(windows) && !has_ext {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in path_value.split(sep).filter(|d| !d.is_empty()) {
        for ext in exts {
            let mut file = PathBuf::from(dir);
            file.push(format!("{name}{ext}"));
            if file.is_file() {
                return Some(file);
            }
        }
    }
    None
}

/// Pending client requests by id. A `std` mutex: it is never held across an `.await`, and
/// it lets [`PendingGuard`] deregister a cancelled request synchronously in `Drop`.
type PendingMap = Arc<StdMutex<HashMap<i64, oneshot::Sender<Result<Value, LspError>>>>>;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;

/// JSON-RPC 2.0 error codes the client itself emits when answering server requests.
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// Removes a request's pending entry when its future finishes OR is dropped (caller
/// cancellation), so a late response can never find a stale sender.
struct PendingGuard<'a> {
    pending: &'a PendingMap,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

fn lock_pending(
    pending: &PendingMap,
) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<Result<Value, LspError>>>> {
    // A poisoned map only means another task panicked mid-insert/remove; the map itself
    // is still consistent.
    pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared ring buffer of the child's most recent stderr bytes (`_STDERR_RING_BYTES` cap).
#[derive(Clone, Default)]
pub struct StderrRing(Arc<StdMutex<Vec<u8>>>);

impl StderrRing {
    fn push(&self, chunk: &[u8]) {
        let mut buf = self.0.lock().expect("stderr ring lock");
        buf.extend_from_slice(chunk);
        if buf.len() > STDERR_RING_BYTES {
            let excess = buf.len() - STDERR_RING_BYTES;
            buf.drain(..excess);
        }
    }

    /// The formatted hint the Python client appends to connection errors (last 500 chars).
    pub fn hint(&self) -> String {
        let buf = self.0.lock().expect("stderr ring lock");
        if buf.is_empty() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        let tail: String = if text.chars().count() > 500 {
            let skip = text.chars().count() - 500;
            format!("\u{2026}{}", text.chars().skip(skip).collect::<String>())
        } else {
            text.to_owned()
        };
        format!(" (stderr: {tail})")
    }
}

/// Asyncio-equivalent LSP client. Construct via `connect_stdio` / `connect_tcp` /
/// `connect_io`; always call [`LspClient::shutdown`] when done.
pub struct LspClient {
    writer: SharedWriter,
    pending: PendingMap,
    next_id: AtomicI64,
    timeout: Duration,
    reader_task: JoinHandle<()>,
    stderr_task: Option<JoinHandle<()>>,
    process: Option<tokio::process::Child>,
    stderr: StderrRing,
}

impl LspClient {
    /// Spawn *command* as an LSP subprocess (stdio transport) and complete the handshake.
    /// *path_prepend* is put at the front of the child's PATH (the venv hardening); the
    /// command is resolved on that PATH.
    pub async fn connect_stdio(
        command: &[String],
        path_prepend: Option<&Path>,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        if command.is_empty() {
            return Err(LspError::Connection("empty LSP server command".to_owned()));
        }
        let env = match path_prepend {
            Some(dir) => env_with_path_prepend(dir),
            None => std::env::vars_os().collect(),
        };
        let path_value = env
            .iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let resolved = resolve_command(command, &path_value);
        let mut child = tokio::process::Command::new(&resolved[0])
            .args(&resolved[1..])
            .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                LspError::Connection(format!("Cannot start LSP server {:?}: {e}", command[0]))
            })?;

        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let stderr = child.stderr.take().expect("piped stderr");

        let ring = StderrRing::default();
        let ring_writer = ring.clone();
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 512];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                ring_writer.push(&chunk[..n]);
            }
        });

        let mut client =
            Self::wire(Box::new(stdout), Box::new(stdin), timeout, ring, Some(stderr_task));
        client.process = Some(child);
        client.initialize(root_uri).await?;
        Ok(client)
    }

    /// Connect to an already-running server over TCP and complete the handshake.
    pub async fn connect_tcp(
        host: &str,
        port: u16,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        let stream = tokio::time::timeout(
            timeout,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| {
            LspError::Connection(format!("Timed out connecting to LSP server at {host}:{port}"))
        })?
        .map_err(|e| {
            LspError::Connection(format!(
                "Cannot connect to LSP server at {host}:{port} — is the server running? ({e})"
            ))
        })?;
        let (read_half, write_half) = stream.into_split();
        let mut client = Self::wire(
            Box::new(read_half),
            Box::new(write_half),
            timeout,
            StderrRing::default(),
            None,
        );
        client.initialize(root_uri).await?;
        Ok(client)
    }

    /// Attach to an arbitrary IO pair (tests use `tokio::io::duplex`) and complete the
    /// handshake.
    pub async fn connect_io(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        let mut client = Self::wire(reader, writer, timeout, StderrRing::default(), None);
        client.initialize(root_uri).await?;
        Ok(client)
    }

    fn wire(
        reader: Box<dyn AsyncRead + Unpin + Send>,
        writer: Box<dyn AsyncWrite + Unpin + Send>,
        timeout: Duration,
        stderr: StderrRing,
        stderr_task: Option<JoinHandle<()>>,
    ) -> Self {
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let writer: SharedWriter = Arc::new(Mutex::new(writer));
        let reader_writer = Arc::clone(&writer);
        let reader_task = tokio::spawn(async move {
            read_loop(reader, reader_pending, reader_writer).await;
        });
        Self {
            writer,
            pending,
            next_id: AtomicI64::new(1),
            timeout,
            reader_task,
            stderr_task,
            process: None,
            stderr,
        }
    }

    /// The `initialize` / `initialized` handshake with the same capability payload as the
    /// Python client.
    async fn initialize(&mut self, root_uri: &str) -> Result<(), LspError> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
            "capabilities": {
                "textDocument": {
                    "hover": { "contentFormat": ["plaintext", "markdown"] }
                },
                "workspace": { "workspaceFolders": true },
            },
            "clientInfo": {"name": "IntentumDiff", "version": "0.1"},
        });
        match self.send_request("initialize", params).await {
            Ok(_) => {
                self.send_notification("initialized", json!({})).await?;
                Ok(())
            }
            Err(e) => {
                // Mirror `_cleanup_start_failure` + the stderr hint on handshake failure.
                let hint = self.stderr.hint();
                self.teardown_tasks();
                if let Some(mut process) = self.process.take() {
                    let _ = process.start_kill();
                }
                Err(match e {
                    LspError::Timeout(msg) => LspError::Timeout(format!("{msg}{hint}")),
                    LspError::Connection(msg) => LspError::Connection(format!(
                        "{msg} — the server may have crashed{hint}"
                    )),
                    other => other,
                })
            }
        }
    }

    /// `textDocument/didOpen`.
    pub async fn did_open(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<(), LspError> {
        self.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await
    }

    /// `textDocument/didClose`.
    pub async fn did_close(&self, uri: &str) -> Result<(), LspError> {
        self.send_notification("textDocument/didClose", json!({"textDocument": {"uri": uri}}))
            .await
    }

    /// `textDocument/hover` → the plain-text type string, `None` when the server has no
    /// information at that position. Timeouts raise (non-fatal), exactly like the Python
    /// client.
    pub async fn hover(&self, uri: &str, line: u32, col: u32) -> Result<Option<String>, LspError> {
        let result = self
            .send_request(
                "textDocument/hover",
                json!({
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": col},
                }),
            )
            .await?;
        Ok(extract_hover_text(&result))
    }

    /// `shutdown` request + `exit` notification, then release the transport. For stdio
    /// servers, the terminate→kill ladder from the Python client; the stdin writer is
    /// dropped rather than explicitly closed after process exit (the Python client skips
    /// `writer.close()` there to avoid its InvalidStateError gotcha — dropping is our
    /// equivalent).
    pub async fn shutdown(mut self) -> Result<(), LspError> {
        let _ = self.send_request("shutdown", Value::Null).await;
        let _ = self.send_notification("exit", Value::Null).await;
        self.teardown_tasks();
        if let Some(mut process) = self.process.take() {
            let _ = process.start_kill();
            if tokio::time::timeout(Duration::from_secs(3), process.wait())
                .await
                .is_err()
            {
                let _ = process.kill().await;
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), process.wait()).await;
            }
        } else {
            let mut writer = self.writer.lock().await;
            let _ = writer.shutdown().await;
        }
        Ok(())
    }

    /// The last captured stderr from a stdio server, formatted as an error hint.
    pub fn stderr_hint(&self) -> String {
        self.stderr.hint()
    }

    fn teardown_tasks(&mut self) {
        self.reader_task.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.write_frame(&notification(method, params), method).await
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock_pending(&self.pending).insert(id, tx);
        // Deregisters on every exit path, including the caller dropping this future.
        let _guard = PendingGuard { pending: &self.pending, id };
        self.write_frame(&request(method, params, id), method).await?;
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError::Connection(format!(
                "LSP server closed the connection during {method:?}"
            ))),
            Err(_) => {
                Err(LspError::Timeout(format!(
                    "No response to {method:?} within {:?}",
                    self.timeout
                )))
            }
        }
    }

    async fn write_frame(&self, msg: &Value, method: &str) -> Result<(), LspError> {
        let bytes = encode_message(msg);
        let mut writer = self.writer.lock().await;
        tokio::time::timeout(self.timeout, async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| LspError::Timeout(format!("Write timeout sending {method:?}")))?
        .map_err(|e| LspError::Connection(format!("LSP connection lost sending {method:?}: {e}")))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(mut process) = self.process.take() {
            let _ = process.start_kill();
        }
    }
}

/// The background reader: decode frames, classify each envelope, resolve pending requests
/// from responses only, answer server requests. On EOF or a fatal framing violation,
/// cancel every outstanding request (dropping the senders wakes the callers with a
/// connection error).
async fn read_loop(
    mut reader: Box<dyn AsyncRead + Unpin + Send>,
    pending: PendingMap,
    writer: SharedWriter,
) {
    let mut decoder = FrameDecoder::new();
    let mut chunk = [0u8; 8_192];
    'outer: loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        decoder.feed(&chunk[..n]);
        loop {
            match decoder.next_event() {
                Ok(Some(DecodeEvent::Frame(msg))) => {
                    if let Some(reply) = dispatch_message(&msg, &pending) {
                        let mut w = writer.lock().await;
                        let _ = w.write_all(&encode_message(&reply)).await;
                        let _ = w.flush().await;
                    }
                }
                Ok(Some(DecodeEvent::MalformedFrame)) => {}
                Ok(None) => break,
                Err(_) => break 'outer,
            }
        }
    }
    lock_pending(&pending).clear();
}

/// The shape of one incoming JSON-RPC envelope, decided before any pending-id lookup.
#[derive(Debug, PartialEq)]
enum Envelope {
    /// `method` + `id`, no `result`/`error`: a server→client request.
    Request,
    /// `method`, no `id`: a notification.
    Notification,
    /// `id` + exactly one of `result`/`error`, no `method`: a response.
    Response,
    /// Anything else (mixed method+result, result+error, neither, non-object…).
    Invalid,
}

fn classify(msg: &Value) -> Envelope {
    let Some(obj) = msg.as_object() else { return Envelope::Invalid };
    let has_method = obj.get("method").is_some_and(Value::is_string);
    let has_id = obj.contains_key("id");
    let has_result = obj.contains_key("result");
    let has_error = obj.contains_key("error");
    match (has_method, has_id, has_result || has_error) {
        (true, true, false) => Envelope::Request,
        (true, false, false) => Envelope::Notification,
        (false, true, true) if has_result != has_error => Envelope::Response,
        _ => Envelope::Invalid,
    }
}

/// Handle one envelope; returns the reply frame to write back, if any.
fn dispatch_message(msg: &Value, pending: &PendingMap) -> Option<Value> {
    match classify(msg) {
        Envelope::Notification => None,
        Envelope::Request => {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            Some(json!({
                "jsonrpc": "2.0",
                "id": msg["id"],
                "error": {
                    "code": METHOD_NOT_FOUND,
                    "message": format!("Unhandled method {method}"),
                },
            }))
        }
        Envelope::Invalid => {
            // Only an envelope that names a request (method + usable id) gets an
            // InvalidRequest answer; malformed responses are dropped — answering a
            // response would itself be a protocol violation.
            let id = msg.get("id").filter(|id| id.is_string() || id.is_number())?;
            msg.get("method")?;
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": INVALID_REQUEST, "message": "Invalid request envelope"},
            }))
        }
        Envelope::Response => {
            let id = msg.get("id").and_then(Value::as_i64)?;
            let tx = lock_pending(pending).remove(&id)?;
            let outcome = if let Some(err) = msg.get("error") {
                Err(LspError::Server {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                })
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(outcome);
            None
        }
    }
}

/// Hover `contents` → plain text, ported shape-for-shape from `AsyncLspClient.hover`.
fn extract_hover_text(result: &Value) -> Option<String> {
    if result.is_null() {
        return None;
    }
    let contents = result.get("contents")?;
    match contents {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_owned())
        }
        Value::Object(map) => {
            let t = map.get("value").and_then(Value::as_str).unwrap_or("").trim();
            (!t.is_empty()).then(|| t.to_owned())
        }
        Value::Array(items) => {
            let texts: Vec<String> = items
                .iter()
                .map(|c| match c {
                    Value::Object(m) => {
                        m.get("value").and_then(Value::as_str).unwrap_or("").to_owned()
                    }
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .filter(|t| !t.is_empty())
                .collect();
            let combined = texts.join("\n").trim().to_owned();
            (!combined.is_empty()).then_some(combined)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    /// A scripted LSP peer over an in-process duplex pipe: answers `initialize`, then
    /// responds to each request id with the next canned `result`/`error` payload.
    async fn mock_peer(
        io: tokio::io::DuplexStream,
        replies: Vec<Value>,
        respond: bool,
    ) -> JoinHandle<Vec<Value>> {
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(io);
            let mut decoder = FrameDecoder::new();
            let mut chunk = [0u8; 4096];
            let mut received = Vec::new();
            let mut reply_iter = replies.into_iter();
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..n]);
                while let Ok(Some(ev)) = decoder.next_event() {
                    let DecodeEvent::Frame(msg) = ev else { continue };
                    received.push(msg.clone());
                    let Some(id) = msg.get("id").cloned() else { continue };
                    if msg.get("method").and_then(Value::as_str) == Some("exit") {
                        continue;
                    }
                    if !respond {
                        continue;
                    }
                    let body = match msg.get("method").and_then(Value::as_str) {
                        Some("initialize") | Some("shutdown") => {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        }
                        _ => {
                            let reply = reply_iter.next().unwrap_or(Value::Null);
                            if reply.get("__error").is_some() {
                                json!({"jsonrpc": "2.0", "id": id, "error": reply["__error"]})
                            } else {
                                json!({"jsonrpc": "2.0", "id": id, "result": reply})
                            }
                        }
                    };
                    let _ = writer.write_all(&encode_message(&body)).await;
                    let _ = writer.flush().await;
                }
            }
            received
        })
    }

    async fn connected_client(
        replies: Vec<Value>,
    ) -> (LspClient, JoinHandle<Vec<Value>>) {
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = mock_peer(peer_io, replies, true).await;
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_secs(2),
        )
        .await
        .expect("handshake");
        (client, peer)
    }

    #[tokio::test]
    async fn handshake_sends_initialize_with_the_python_capability_payload() {
        let (client, peer) = connected_client(vec![]).await;
        client.shutdown().await.unwrap();
        let received = peer.await.unwrap();
        let init = &received[0];
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["rootUri"], "file:///ws");
        assert_eq!(
            init["params"]["capabilities"]["textDocument"]["hover"]["contentFormat"],
            json!(["plaintext", "markdown"])
        );
        assert_eq!(init["params"]["clientInfo"]["name"], "IntentumDiff");
        // initialized notification follows, with no id.
        assert_eq!(received[1]["method"], "initialized");
        assert!(received[1].get("id").is_none());
    }

    #[tokio::test]
    async fn hover_extracts_dict_string_and_list_contents() {
        let (client, _peer) = connected_client(vec![
            json!({"contents": {"kind": "markdown", "value": " int "}}),
            json!({"contents": "str"}),
            json!({"contents": [{"value": "a"}, "b", {"value": ""}]}),
            Value::Null,
        ])
        .await;
        assert_eq!(client.hover("u", 0, 0).await.unwrap(), Some("int".to_owned()));
        assert_eq!(client.hover("u", 0, 1).await.unwrap(), Some("str".to_owned()));
        assert_eq!(client.hover("u", 0, 2).await.unwrap(), Some("a\nb".to_owned()));
        assert_eq!(client.hover("u", 0, 3).await.unwrap(), None);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn server_error_response_surfaces_code_and_message() {
        let (client, _peer) = connected_client(vec![
            json!({"__error": {"code": -32601, "message": "method not found"}}),
        ])
        .await;
        let err = client.hover("u", 0, 0).await.unwrap_err();
        assert_eq!(err.to_string(), "LSP error -32601: method not found");
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn unanswered_request_times_out_without_killing_the_client() {
        let (client_io, peer_io) = duplex(1 << 16);
        // Peer answers ONLY initialize (respond=true but no canned replies → hover gets
        // Null result)… we need silence instead: use respond=false after the handshake.
        // Simplest: a peer that answers initialize/shutdown but never other requests.
        let peer = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(peer_io);
            let mut decoder = FrameDecoder::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..n]);
                while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
                    if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                        let body = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
                        let _ = writer.write_all(&encode_message(&body)).await;
                        let _ = writer.flush().await;
                    }
                }
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_millis(200),
        )
        .await
        .expect("handshake");
        let err = client.hover("u", 1, 2).await.unwrap_err();
        assert!(matches!(err, LspError::Timeout(_)), "got: {err}");
        // The client survives a per-request timeout (Python: non-fatal).
        client.did_close("u").await.unwrap();
        client.shutdown().await.unwrap();
        peer.abort();
    }

    #[tokio::test]
    async fn concurrent_hovers_resolve_by_request_id() {
        let (client, _peer) = connected_client(vec![
            json!({"contents": "first"}),
            json!({"contents": "second"}),
        ])
        .await;
        let (a, b) = tokio::join!(client.hover("u", 0, 0), client.hover("u", 0, 1));
        // Replies are canned in order of arrival; ids route each to its own future.
        let mut got = vec![a.unwrap().unwrap(), b.unwrap().unwrap()];
        got.sort();
        assert_eq!(got, vec!["first".to_owned(), "second".to_owned()]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn did_open_and_did_close_are_notifications_without_ids() {
        let (client, peer) = connected_client(vec![]).await;
        client.did_open("file:///a.py", "python", "x = 1\n").await.unwrap();
        client.did_close("file:///a.py").await.unwrap();
        client.shutdown().await.unwrap();
        let received = peer.await.unwrap();
        let methods: Vec<&str> = received
            .iter()
            .filter_map(|m| m.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            vec!["initialize", "initialized", "textDocument/didOpen",
                 "textDocument/didClose", "shutdown", "exit"]
        );
        let did_open = &received[2];
        assert!(did_open.get("id").is_none());
        assert_eq!(did_open["params"]["textDocument"]["languageId"], "python");
        assert_eq!(did_open["params"]["textDocument"]["version"], 1);
    }

    #[tokio::test]
    async fn peer_disconnect_fails_pending_requests_with_connection_error() {
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(peer_io);
            let mut decoder = FrameDecoder::new();
            let mut chunk = [0u8; 4096];
            // Answer initialize; hang up when the hover request arrives, leaving it pending.
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..n]);
                while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
                    match msg.get("method").and_then(Value::as_str) {
                        Some("initialize") => {
                            let body = json!({"jsonrpc": "2.0", "id": msg["id"], "result": {}});
                            let _ = writer.write_all(&encode_message(&body)).await;
                            let _ = writer.flush().await;
                        }
                        Some("textDocument/hover") => {
                            return; // drop both halves → EOF for the client
                        }
                        _ => {}
                    }
                }
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_secs(2),
        )
        .await
        .expect("handshake");
        let err = client.hover("u", 0, 0).await.unwrap_err();
        peer.await.unwrap();
        assert!(
            matches!(err, LspError::Connection(_)) || matches!(err, LspError::Timeout(_)),
            "got: {err}"
        );
    }

    /// A scripted peer that reads client frames and, for each one, lets `script` decide
    /// which raw frames to write back. Returns every frame the client sent.
    fn scripted_peer<F>(io: tokio::io::DuplexStream, mut script: F) -> JoinHandle<Vec<Value>>
    where
        F: FnMut(&Value) -> Vec<Value> + Send + 'static,
    {
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(io);
            let mut decoder = FrameDecoder::new();
            let mut chunk = [0u8; 4096];
            let mut received = Vec::new();
            loop {
                let n = match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                decoder.feed(&chunk[..n]);
                while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
                    for reply in script(&msg) {
                        let _ = writer.write_all(&encode_message(&reply)).await;
                        let _ = writer.flush().await;
                    }
                    received.push(msg);
                }
            }
            received
        })
    }

    #[tokio::test]
    async fn server_request_with_colliding_id_does_not_complete_initialize() {
        // ECO-03 reproduction: the peer answers initialize (id=1) ONLY with a server→client
        // request that happens to reuse id 1. That must not resolve the pending initialize.
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = scripted_peer(peer_io, |msg| {
            if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                vec![json!({"jsonrpc": "2.0", "id": msg["id"], "method": "workspace/configuration",
                            "params": {"items": []}})]
            } else {
                vec![]
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let result = LspClient::connect_io(
            Box::new(r),
            Box::new(w),
            "file:///ws",
            Duration::from_millis(100),
        )
        .await;
        let err = result.err().expect("initialize must not succeed on a server request");
        assert!(matches!(err, LspError::Timeout(_)), "got: {err}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        peer.abort();
    }

    #[tokio::test]
    async fn unsupported_server_requests_get_a_method_not_found_error() {
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = scripted_peer(peer_io, |msg| {
            match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
                // Server request first (string id), then the real initialize response.
                (Some("initialize"), Some(id)) => vec![
                    json!({"jsonrpc": "2.0", "id": "srv-1", "method": "window/workDoneProgress/create",
                           "params": {"token": "t"}}),
                    json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                ],
                (Some("shutdown"), Some(id)) => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
                _ => vec![],
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(Box::new(r), Box::new(w), "file:///ws", Duration::from_secs(2))
            .await
            .expect("handshake");
        client.shutdown().await.unwrap();
        let received = peer.await.unwrap();
        let answer = received
            .iter()
            .find(|m| m.get("id") == Some(&json!("srv-1")))
            .expect("the client must answer the server request");
        assert!(answer.get("method").is_none(), "an answer is a response, not a request");
        assert!(answer.get("result").is_none());
        assert_eq!(answer["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn only_well_formed_responses_resolve_pending_requests() {
        // Before the real hover result arrives, the peer sends: a notification, a server
        // request and a mixed method+result envelope reusing the hover id, and a response
        // carrying both result and error. None may resolve the hover.
        let (client_io, peer_io) = duplex(1 << 16);
        let peer = scripted_peer(peer_io, |msg| {
            let id = msg.get("id").cloned();
            match (msg.get("method").and_then(Value::as_str), id) {
                (Some("initialize"), Some(id)) => vec![json!({"jsonrpc": "2.0", "id": id, "result": {}})],
                (Some("textDocument/hover"), Some(id)) => vec![
                    json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"type": 3, "message": "x"}}),
                    json!({"jsonrpc": "2.0", "id": id, "method": "workspace/configuration", "params": {}}),
                    json!({"jsonrpc": "2.0", "id": id, "method": "bogus", "result": {"contents": "mixed"}}),
                    json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "both"},
                           "error": {"code": 1, "message": "both"}}),
                    json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "real"}}),
                ],
                (Some("shutdown"), Some(id)) => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
                _ => vec![],
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(Box::new(r), Box::new(w), "file:///ws", Duration::from_secs(2))
            .await
            .expect("handshake");
        assert_eq!(client.hover("u", 0, 0).await.unwrap(), Some("real".to_owned()));
        client.shutdown().await.unwrap();
        let received = peer.await.unwrap();
        // The server request and the mixed envelope were each answered with an error; the
        // notification and the malformed response were not answered.
        let answers: Vec<&Value> = received
            .iter()
            .filter(|m| m.get("method").is_none() && m.get("error").is_some())
            .collect();
        assert_eq!(answers.len(), 2, "got {received:?}");
        let codes: Vec<i64> = answers.iter().map(|a| a["error"]["code"].as_i64().unwrap()).collect();
        assert_eq!(codes, vec![-32601, -32600]);
    }

    #[tokio::test]
    async fn late_responses_after_timeout_or_cancellation_are_dropped() {
        // The peer answers hover #1 only after hover #2 has been sent, and answers hover #2
        // correctly. The late answer must not leak into hover #2.
        let (client_io, peer_io) = duplex(1 << 16);
        let mut held: Option<Value> = None;
        let peer = scripted_peer(peer_io, move |msg| {
            let id = msg.get("id").cloned();
            match (msg.get("method").and_then(Value::as_str), id) {
                (Some("initialize"), Some(id)) => vec![json!({"jsonrpc": "2.0", "id": id, "result": {}})],
                (Some("textDocument/hover"), Some(id)) => match held.take() {
                    None => {
                        held = Some(id);
                        vec![]
                    }
                    Some(first) => vec![
                        json!({"jsonrpc": "2.0", "id": first, "result": {"contents": "late"}}),
                        json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "second"}}),
                    ],
                },
                (Some("shutdown"), Some(id)) => vec![json!({"jsonrpc": "2.0", "id": id, "result": null})],
                _ => vec![],
            }
        });
        let (r, w) = tokio::io::split(client_io);
        let client = LspClient::connect_io(Box::new(r), Box::new(w), "file:///ws", Duration::from_millis(150))
            .await
            .expect("handshake");
        // Cancel the first request by dropping its future before it resolves.
        let first = tokio::time::timeout(Duration::from_millis(50), client.hover("u", 0, 0)).await;
        assert!(first.is_err(), "first hover must still be pending when cancelled");
        assert!(client.pending.lock().expect("pending").is_empty(), "cancelled request left pending");
        assert_eq!(client.hover("u", 0, 1).await.unwrap(), Some("second".to_owned()));
        client.shutdown().await.unwrap();
        peer.abort();
    }

    #[test]
    fn envelopes_are_classified_before_any_pending_lookup() {
        use Envelope::*;
        let cases = [
            (json!({"jsonrpc": "2.0", "id": 1, "method": "m"}), Request),
            (json!({"jsonrpc": "2.0", "id": "s", "method": "m", "params": {}}), Request),
            (json!({"jsonrpc": "2.0", "method": "m"}), Notification),
            (json!({"jsonrpc": "2.0", "id": 1, "result": null}), Response),
            (json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 1, "message": ""}}), Response),
            (json!({"jsonrpc": "2.0", "id": 1, "method": "m", "result": 1}), Invalid),
            (json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {}}), Invalid),
            (json!({"jsonrpc": "2.0", "id": 1}), Invalid),
            (json!({"jsonrpc": "2.0", "result": 1}), Invalid),
            (json!([1, 2]), Invalid),
        ];
        for (msg, want) in cases {
            assert_eq!(classify(&msg), want, "{msg}");
        }
        // A pending id is never consumed by a non-response envelope.
        let pending: PendingMap = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        lock_pending(&pending).insert(1, tx);
        let reply = dispatch_message(&json!({"jsonrpc": "2.0", "id": 1, "method": "m"}), &pending);
        assert_eq!(reply.unwrap()["error"]["code"], METHOD_NOT_FOUND);
        assert!(lock_pending(&pending).contains_key(&1));
        assert!(dispatch_message(&json!({"jsonrpc": "2.0", "id": 1, "result": 7}), &pending).is_none());
        assert!(lock_pending(&pending).is_empty());
    }

    #[test]
    fn env_prepend_puts_dir_first_without_duplicating() {
        let dir = std::env::temp_dir();
        let env = env_with_path_prepend(&dir);
        let path = env
            .iter()
            .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let first = path.split(sep).next().unwrap();
        assert_eq!(Path::new(first), dir.as_path());
        // Idempotent: prepending again must not duplicate.
        let count = path.split(sep).filter(|p| Path::new(p) == dir.as_path()).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn resolve_command_passes_through_when_not_found() {
        let cmd = vec!["definitely-not-a-real-lsp".to_owned(), "--stdio".to_owned()];
        assert_eq!(resolve_command(&cmd, ""), cmd);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_command_wraps_cmd_wrappers_in_cmd_exe() {
        let dir = std::env::temp_dir().join("intentumdiff-lsp-client-test");
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join("fake-lsp.cmd");
        std::fs::write(&wrapper, "@echo off\r\n").unwrap();
        let resolved = resolve_command(
            &["fake-lsp".to_owned(), "--stdio".to_owned()],
            dir.to_string_lossy().as_ref(),
        );
        assert_eq!(resolved[0], "cmd.exe");
        assert_eq!(resolved[1], "/c");
        assert!(resolved[2].to_lowercase().ends_with("fake-lsp.cmd"));
        assert_eq!(resolved[3], "--stdio");
        let _ = std::fs::remove_file(&wrapper);
    }
}
