//! End-to-end regression test for the native LSP server (#100 S3): spawn the built binary
//! and drive a full editor session over stdio, asserting the same flow the manual smoke
//! covered. Self-contained — reuses the `intentumdiff-lsp-client` codec for its own framing,
//! shells out to `git` for the fixture, and needs no Python. Skips (passes with a printed
//! note) when the bundled wasm parsers or `git` are unavailable, so a checkout without built
//! wasm does not fail the suite.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use intentumdiff_lsp_client::{encode_message, DecodeEvent, FrameDecoder};
use serde_json::{json, Value};

/// The parser bundle: `INTENTUMDIFF_TEST_WASM_DIR` when set, else the dev-layout wasm dir
/// (walk the crate's ancestors for `src/intentumdiff/wasm`), verified by
/// `parser_manifest.json`. `None` when the parsers have not been built.
fn find_wasm_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("INTENTUMDIFF_TEST_WASM_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join("parser_manifest.json").is_file() {
            return Some(dir);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for ancestor in manifest.ancestors() {
        let dev = ancestor.join("src").join("intentumdiff").join("wasm");
        if dev.join("parser_manifest.json").is_file() {
            return Some(dev);
        }
    }
    None
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git spawn");
    assert!(status.success(), "git {args:?} failed");
}

fn file_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// A unique temp dir for the fixture repo (no `tempfile` dep).
fn unique_temp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("intentumdiff-lsp-it-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The documented bound for protocol `exit` to end the process (generous for debug CI).
const EXIT_DEADLINE: Duration = Duration::from_secs(5);

/// Poll `child` until it exits or `deadline` passes; on timeout the child is killed and
/// `None` is returned, so a hung server fails the test instead of hanging it.
fn wait_with_deadline(
    child: &mut std::process::Child,
    deadline: Duration,
) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<Value>,
    seen_notifications: Vec<String>,
}

impl Session {
    fn send(&mut self, msg: &Value) {
        self.stdin.write_all(&encode_message(msg)).expect("write");
        self.stdin.flush().expect("flush");
    }

    fn recv(&self) -> Value {
        // Generous: a debug-profile core JIT-compiles the wasm parser on first diff.
        self.rx
            .recv_timeout(Duration::from_secs(120))
            .expect("timed out waiting for an LSP frame")
    }

    /// Drain frames until the response with `want_id`, recording any notifications and
    /// acking any server→client requests (e.g. codeLens refresh).
    fn recv_response(&mut self, want_id: i64) -> Value {
        loop {
            let msg = self.recv();
            if let Some(method) = msg.get("method").and_then(Value::as_str) {
                self.seen_notifications.push(method.to_owned());
                if let Some(id) = msg.get("id") {
                    // A server-initiated request — ack so the peer contract stays clean.
                    self.send(&json!({"jsonrpc": "2.0", "id": id, "result": Value::Null}));
                }
                continue;
            }
            if msg.get("id").and_then(Value::as_i64) == Some(want_id) {
                return msg;
            }
        }
    }
}

fn spawn(bin: &str, repo: &Path, wasm_dir: &Path) -> Session {
    let mut child = Command::new(bin)
        .args(["--wasm-dir", &wasm_dir.to_string_lossy()])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lsp-server");

    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = match stdout.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            decoder.feed(&buf[..n]);
            while let Ok(Some(ev)) = decoder.next_event() {
                if let DecodeEvent::Frame(msg) = ev {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            }
        }
    });

    Session { child, stdin, rx, seen_notifications: Vec::new() }
}

#[test]
fn native_lsp_server_serves_the_full_editor_flow() {
    let Some(wasm_dir) = find_wasm_dir() else {
        eprintln!("skipping: bundled wasm parsers not built (no parser_manifest.json)");
        return;
    };
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");

    let base = unique_temp_dir();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("a.ts"), "const x: number = 1;\n").unwrap();
    std::fs::write(repo.join("b.ts"), "const y: number = 5;\n").unwrap();
    git(&repo, &["add", "a.ts", "b.ts"]);
    git(&repo, &["commit", "-m", "v1"]);

    let root_uri = file_uri(&repo);
    let a_uri = file_uri(&repo.join("a.ts"));
    let b_uri = file_uri(&repo.join("b.ts"));

    let mut s = spawn(bin, &repo, &wasm_dir);

    // 1. Handshake advertises full-sync + a code-lens provider.
    s.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "rootUri": root_uri,
            "capabilities": {"workspace": {"codeLens": {"refreshSupport": true}}},
        },
    }));
    let init = s.recv_response(1);
    let caps = &init["result"]["capabilities"];
    assert_eq!(caps["textDocumentSync"], 1);
    assert_eq!(caps["codeLensProvider"], json!({"resolveProvider": false}));
    assert_eq!(init["result"]["serverInfo"]["name"], "intentumdiff-lsp");
    s.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));

    // 2. An edit pushes diagnostics (the modified value is still a MODIFICATION, not style).
    s.send(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {
            "uri": a_uri, "languageId": "typescript", "version": 1,
            "text": "const x: number = 42;\n"}},
    }));
    // The first frame after an edit is the diagnostics push.
    let diag = s.recv();
    assert_eq!(diag["method"], "textDocument/publishDiagnostics");
    assert_eq!(diag["params"]["uri"], a_uri);

    // 3. codeLens pulls (list shape; may be empty depending on the change family).
    s.send(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "textDocument/codeLens",
        "params": {"textDocument": {"uri": a_uri}},
    }));
    let lens = s.recv_response(2);
    assert!(lens["result"].is_array(), "codeLens must return an array");

    // 4. intentumdiff/semanticDiff, same URI = live buffer vs HEAD.
    s.send(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "intentumdiff/semanticDiff",
        "params": {"oldUri": a_uri, "newUri": a_uri},
    }));
    let same = &s.recv_response(3)["result"];
    assert_eq!(same["language"], "typescript");
    let same_types: Vec<&str> =
        same["changes"].as_array().unwrap().iter().filter_map(|c| c["change_type"].as_str()).collect();
    assert!(same_types.contains(&"MODIFICATION"), "got {same_types:?}");

    // 5. intentumdiff/semanticDiff, two files = disk compare.
    s.send(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "intentumdiff/semanticDiff",
        "params": {"oldUri": a_uri, "newUri": b_uri},
    }));
    let two = &s.recv_response(4)["result"];
    assert_eq!(two["language"], "typescript");
    assert!(!two["changes"].as_array().unwrap().is_empty(), "a.ts vs b.ts must change");

    // 6. The #88 containment guard rejects an out-of-root URI.
    let outside = if cfg!(windows) {
        "file:///C:/outside/evil.ts"
    } else {
        "file:///outside/evil.ts"
    };
    s.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "intentumdiff/semanticDiff",
        "params": {"oldUri": a_uri, "newUri": outside},
    }));
    let guarded = &s.recv_response(5)["result"];
    assert!(
        guarded["error"].as_str().unwrap_or("").contains("outside the workspace root"),
        "expected containment error, got {guarded}"
    );

    // The edit also nudged the client's code-lens refresh (recorded while draining).
    assert!(
        s.seen_notifications.iter().any(|m| m == "workspace/codeLens/refresh"),
        "expected a codeLens refresh request after the edit, saw {:?}",
        s.seen_notifications
    );

    // 7. Clean shutdown (#73): shutdown → null, exit → process leaves with code 0.
    s.send(&json!({"jsonrpc": "2.0", "id": 6, "method": "shutdown", "params": Value::Null}));
    assert!(s.recv_response(6)["result"].is_null());
    s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
    // stdin stays open: protocol exit alone must end the process (ECO-09-EXIT).
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE).expect("exit must not wait for EOF");
    assert!(status.success(), "clean exit expected, got {status:?}");

    let _ = std::fs::remove_dir_all(&base);
}

/// A git fixture repo with one committed file; returns (temp base, repo).
fn fixture_repo(files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let base = unique_temp_dir();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "T"]);
    for (name, text) in files {
        std::fs::write(repo.join(name), text).unwrap();
        git(&repo, &["add", name]);
    }
    git(&repo, &["commit", "-m", "v1"]);
    (base, repo)
}

/// Open `uri` with `text`, wait for the diagnostics push, then pull its code lenses.
fn lenses_after_open(s: &mut Session, uri: &str, text: &str, request_id: i64) -> Vec<Value> {
    s.send(&json!({
        "jsonrpc": "2.0", "method": "textDocument/didOpen",
        "params": {"textDocument": {"uri": uri, "languageId": "typescript", "version": 1, "text": text}},
    }));
    let diag = s.recv();
    assert_eq!(diag["method"], "textDocument/publishDiagnostics");
    s.send(&json!({
        "jsonrpc": "2.0", "id": request_id, "method": "textDocument/codeLens",
        "params": {"textDocument": {"uri": uri}},
    }));
    s.recv_response(request_id)["result"].as_array().cloned().unwrap_or_default()
}

#[test]
fn native_code_lenses_use_the_negotiated_position_encoding() {
    // ECO-04: engine columns are UTF-8 bytes. `/* ééé */ ` is 13 bytes but 10 UTF-16 units,
    // so the rename lens over `function …` must start at character 10 under the default
    // encoding, and stay at the byte column 13 when the client negotiated utf-8.
    let Some(wasm_dir) = find_wasm_dir() else {
        eprintln!("skipping: bundled wasm parsers not built (no parser_manifest.json)");
        return;
    };
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");
    let old = "/* ééé */ function foo(a: number) { return a; }\n";
    let new = "/* ééé */ function bar(a: number) { return a; }\n";
    let (base, repo) = fixture_repo(&[("u.ts", old)]);
    let uri = file_uri(&repo.join("u.ts"));

    for (offered, want_encoding, want_start, want_end) in [
        (json!(null), "utf-16", 10, 47),
        (json!(["utf-16"]), "utf-16", 10, 47),
        (json!(["utf-8", "utf-16"]), "utf-8", 13, 50),
    ] {
        let mut s = spawn(bin, &repo, &wasm_dir);
        let mut caps = json!({});
        if !offered.is_null() {
            caps["general"] = json!({"positionEncodings": offered});
        }
        s.send(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"rootUri": file_uri(&repo), "capabilities": caps},
        }));
        let init = s.recv_response(1);
        assert_eq!(init["result"]["capabilities"]["positionEncoding"], want_encoding);
        s.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));

        let lenses = lenses_after_open(&mut s, &uri, new, 2);
        let rename = lenses
            .iter()
            .find(|l| l["command"]["title"].as_str().unwrap_or("").contains("RENAME"))
            .unwrap_or_else(|| panic!("expected a rename lens, got {lenses:?}"));
        assert_eq!(
            rename["range"],
            json!({"start": {"line": 0, "character": want_start},
                   "end": {"line": 0, "character": want_end}}),
            "offered {offered}"
        );

        s.send(&json!({"jsonrpc": "2.0", "id": 9, "method": "shutdown", "params": Value::Null}));
        s.recv_response(9);
        s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
        let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE).expect("exit must not wait for EOF");
        assert!(status.success());
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// A bare session: no workspace root, no parser bundle, no git — protocol lifecycle only.
fn spawn_bare() -> Session {
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");
    let dir = std::env::temp_dir();
    spawn(bin, &dir, &dir.join("no-such-wasm-dir"))
}

fn initialize_bare(s: &mut Session) {
    s.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"capabilities": {}}}));
    s.recv_response(1);
    s.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
}

#[test]
fn exit_after_shutdown_terminates_with_stdin_still_open() {
    let mut s = spawn_bare();
    initialize_bare(&mut s);
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": Value::Null}));
    assert!(s.recv_response(2)["result"].is_null());
    s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
    // Deliberately keep `s.stdin` alive until after the wait.
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE);
    let status = status.expect("server ignored exit while stdin was open");
    assert_eq!(status.code(), Some(0));
    drop(s.stdin);
}

#[test]
fn exit_without_shutdown_terminates_with_code_1() {
    // LSP 3.17: `exit` without a prior `shutdown` exits with code 1.
    let mut s = spawn_bare();
    initialize_bare(&mut s);
    s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE);
    assert_eq!(status.expect("server ignored exit while stdin was open").code(), Some(1));
    drop(s.stdin);
}

#[test]
fn requests_after_shutdown_are_rejected_and_eof_still_exits() {
    let mut s = spawn_bare();
    initialize_bare(&mut s);
    s.send(&json!({"jsonrpc": "2.0", "id": 2, "method": "shutdown", "params": Value::Null}));
    s.recv_response(2);
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "textDocument/codeLens",
                   "params": {"textDocument": {"uri": "file:///x.ts"}}}));
    let rejected = s.recv_response(3);
    assert_eq!(rejected["error"]["code"], -32600, "got {rejected}");
    // EOF (client vanished without exit) still ends the process cleanly.
    drop(s.stdin);
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE).expect("EOF must end the server");
    assert!(status.success(), "got {status:?}");
}
