//! End-to-end regression test for the native LSP server (#100 S3): spawn the built binary
//! and drive a full editor session over stdio, asserting the same flow the manual smoke
//! covered. Self-contained — reuses the `intentumdiff-lsp-client` codec for its own framing,
//! shells out to `git` for the fixture, and needs no Python.
//!
//! Parser fixtures: `INTENTUMDIFF_TEST_WASM_DIR` names an explicit bundle (staged by
//! `scripts/provision_lsp_fixtures.py`); without it the dev-layout ancestor walk is tried.
//! The parser-backed sessions skip with a printed note when no bundle or `git` is found —
//! UNLESS `INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE=1` (the release/CI gate), which turns a
//! missing/unverified bundle or missing `git` into a FAILURE, accepts only the explicit
//! dir, re-hashes every component against the bundle's `provenance.json`, and records the
//! binary + component identities (stderr, and `INTENTUMDIFF_LSP_ACCEPTANCE_RECORD` when
//! set). The lifecycle/bounds sessions need no parsers and always run.

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

const REQUIRE_ENV: &str = "INTENTUMDIFF_LSP_REQUIRE_ACCEPTANCE";

/// The release/CI gate: parser-backed sessions must run, never skip.
fn acceptance_required() -> bool {
    matches!(std::env::var(REQUIRE_ENV).as_deref(), Ok("1") | Ok("true"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Required mode: the explicit bundle must carry a registry-verified `provenance.json`
/// whose digests match the bytes on disk, and the manifest may only name those
/// components. Returns the acceptance identity record.
fn verify_provisioned_bundle(dir: &Path) -> Result<Value, String> {
    let read_json = |name: &str| -> Result<Value, String> {
        let text = std::fs::read_to_string(dir.join(name))
            .map_err(|e| format!("{name} unreadable in {}: {e}", dir.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{name} is not JSON: {e}"))
    };
    let provenance = read_json("provenance.json")?;
    if provenance["verified"] != json!(true) {
        return Err("provenance.json is not registry-verified (provisioned with --no-verify?)".into());
    }
    let components = provenance["components"].as_object()
        .filter(|c| !c.is_empty())
        .ok_or("provenance.json lists no components")?;
    let mut identities = serde_json::Map::new();
    for (name, entry) in components {
        let bytes = std::fs::read(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
        let digest = sha256_hex(&bytes);
        if entry["sha256"].as_str() != Some(digest.as_str()) {
            return Err(format!("{name}: sha256 {digest} does not match provenance {}", entry["sha256"]));
        }
        identities.insert(name.clone(), json!(digest));
    }
    let manifest = read_json("parser_manifest.json")?;
    for (language, entry) in manifest["parsers"].as_object().ok_or("manifest has no parsers")? {
        let wasm = entry["wasm"].as_str().unwrap_or("");
        if !components.contains_key(wasm) {
            return Err(format!("manifest language {language} names unprovisioned component {wasm:?}"));
        }
    }
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");
    let binary = std::fs::read(bin).map_err(|e| format!("read binary {bin}: {e}"))?;
    Ok(json!({
        "binary": {"path": bin, "sha256": sha256_hex(&binary)},
        "registry_ref": provenance["registry_ref"],
        "components": identities,
        "wasm_dir": dir.to_string_lossy(),
    }))
}

/// The parser bundle for a parser-backed session, or `None` to skip. In required mode
/// every gap panics instead, and the identities are recorded once per test process.
fn acceptance_fixtures() -> Option<PathBuf> {
    if !acceptance_required() {
        let Some(dir) = find_wasm_dir() else {
            eprintln!("skipping: bundled wasm parsers not built (no parser_manifest.json); \
                       set {REQUIRE_ENV}=1 to make this a failure");
            return None;
        };
        if !git_available() {
            eprintln!("skipping: git not available");
            return None;
        }
        return Some(dir);
    }
    let dir = std::env::var_os("INTENTUMDIFF_TEST_WASM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{REQUIRE_ENV}=1 but INTENTUMDIFF_TEST_WASM_DIR is not set"));
    assert!(git_available(), "{REQUIRE_ENV}=1 but git is not available");
    static RECORD: std::sync::OnceLock<Result<Value, String>> = std::sync::OnceLock::new();
    let record = RECORD.get_or_init(|| {
        let record = verify_provisioned_bundle(&dir)?;
        eprintln!("acceptance identities: {record}");
        if let Some(path) = std::env::var_os("INTENTUMDIFF_LSP_ACCEPTANCE_RECORD") {
            std::fs::write(&path, format!("{record:#}\n"))
                .map_err(|e| format!("write acceptance record: {e}"))?;
        }
        Ok(record)
    });
    if let Err(e) = record {
        panic!("{REQUIRE_ENV}=1 but the parser bundle at {} is not acceptable: {e}", dir.display());
    }
    Some(dir)
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
    let Some(wasm_dir) = acceptance_fixtures() else { return; };
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
    let Some(wasm_dir) = acceptance_fixtures() else { return; };
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

impl Session {
    /// Drain frames until a `publishDiagnostics` for `uri`, acking server requests.
    fn recv_publish(&mut self, uri: &str) -> Value {
        loop {
            let msg = self.recv();
            let Some(method) = msg.get("method").and_then(Value::as_str) else { continue };
            self.seen_notifications.push(method.to_owned());
            if let Some(id) = msg.get("id") {
                self.send(&json!({"jsonrpc": "2.0", "id": id, "result": Value::Null}));
                continue;
            }
            if method == "textDocument/publishDiagnostics" && msg["params"]["uri"] == uri {
                return msg["params"].clone();
            }
        }
    }

    fn code_lenses(&mut self, uri: &str, id: i64) -> Vec<Value> {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "textDocument/codeLens",
                          "params": {"textDocument": {"uri": uri}}}));
        self.recv_response(id)["result"].as_array().cloned().expect("codeLens array")
    }

    fn open(&mut self, uri: &str, version: i64, text: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "typescript",
                                        "version": version, "text": text}}}));
    }

    fn change(&mut self, uri: &str, version: i64, text: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": "textDocument/didChange",
            "params": {"textDocument": {"uri": uri, "version": version},
                       "contentChanges": [{"text": text}]}}));
    }
}

fn has_rename(lenses: &[Value]) -> bool {
    lenses.iter().any(|l| l["command"]["title"].as_str().unwrap_or("").contains("RENAME"))
}

const FOO: &str = "function foo(a: number) { return a; }\n";
const BAR: &str = "function bar(a: number) { return a; }\n";
const BROKEN: &str = "function bar(a: number { return a;\n";

/// Start a session over a repo whose committed `s.ts` is `FOO`.
fn stale_state_session() -> Option<(Session, PathBuf, PathBuf, String)> {
    let wasm_dir = acceptance_fixtures()?;
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");
    let (base, repo) = fixture_repo(&[("s.ts", FOO)]);
    let uri = file_uri(&repo.join("s.ts"));
    let mut s = spawn(bin, &repo, &wasm_dir);
    s.send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"rootUri": file_uri(&repo),
                   "capabilities": {"workspace": {"codeLens": {"refreshSupport": true}}}}}));
    s.recv_response(1);
    s.send(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
    Some((s, base, repo, uri))
}

fn finish(mut s: Session, base: &Path) {
    s.send(&json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": Value::Null}));
    s.recv_response(99);
    s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE).expect("exit");
    assert!(status.success());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn close_clears_diagnostics_and_lenses_and_reopen_restores_them() {
    let Some((mut s, base, _repo, uri)) = stale_state_session() else { return };
    s.open(&uri, 1, BAR);
    assert_eq!(s.recv_publish(&uri)["version"], 1);
    assert!(has_rename(&s.code_lenses(&uri, 2)));

    s.send(&json!({"jsonrpc": "2.0", "method": "textDocument/didClose",
                   "params": {"textDocument": {"uri": uri}}}));
    let cleared = s.recv_publish(&uri);
    assert_eq!(cleared["diagnostics"], json!([]), "close must clear diagnostics");
    assert!(s.code_lenses(&uri, 3).is_empty(), "close must drop lenses");

    // Reopening recomputes from the reopened buffer's own version.
    s.open(&uri, 7, BAR);
    assert_eq!(s.recv_publish(&uri)["version"], 7);
    assert!(has_rename(&s.code_lenses(&uri, 4)));
    finish(s, &base);
}

#[test]
fn a_declined_revision_clears_the_previous_results() {
    let Some((mut s, base, _repo, uri)) = stale_state_session() else { return };
    s.open(&uri, 1, BAR);
    s.recv_publish(&uri);
    assert!(has_rename(&s.code_lenses(&uri, 2)));

    // Invalid syntax → the engine declines (fallback). The rename lens and diagnostics of
    // version 1 must not survive as if they described version 2.
    s.change(&uri, 2, BROKEN);
    let published = s.recv_publish(&uri);
    assert_eq!(published["version"], 2);
    assert_eq!(published["diagnostics"], json!([]));
    assert!(s.code_lenses(&uri, 3).is_empty(), "stale lenses survived a declined revision");
    finish(s, &base);
}

#[test]
fn a_failed_revision_clears_results_and_reports_why() {
    let Some((mut s, base, repo, uri)) = stale_state_session() else { return };
    s.open(&uri, 1, BAR);
    s.recv_publish(&uri);
    assert!(has_rename(&s.code_lenses(&uri, 2)));

    // Break the baseline: the ref can no longer be resolved → compute fails.
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    s.change(&uri, 2, FOO);
    let published = s.recv_publish(&uri);
    assert_eq!(published["version"], 2);
    let diags = published["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "expected one provenance diagnostic, got {diags:?}");
    assert_eq!(diags[0]["source"], "intentumdiff");
    assert_eq!(diags[0]["severity"], 3);
    assert!(diags[0]["message"].as_str().unwrap().contains("unavailable"));
    assert!(s.code_lenses(&uri, 3).is_empty(), "stale lenses survived a failed revision");
    finish(s, &base);
}

#[test]
fn out_of_order_changes_cannot_overwrite_a_newer_revision() {
    let Some((mut s, base, _repo, uri)) = stale_state_session() else { return };
    s.open(&uri, 1, FOO);
    s.recv_publish(&uri);
    // Version 3 (the rename) arrives before a late version 2 (no change).
    s.change(&uri, 3, BAR);
    s.change(&uri, 2, FOO);
    let published = s.recv_publish(&uri);
    assert_eq!(published["version"], 3, "got {published}");
    assert!(has_rename(&s.code_lenses(&uri, 2)), "late v2 overwrote v3");
    // Same-URI semanticDiff also serves the newest buffer.
    s.send(&json!({"jsonrpc": "2.0", "id": 3, "method": "intentumdiff/semanticDiff",
                   "params": {"oldUri": uri, "newUri": uri}}));
    let diff = s.recv_response(3);
    let types: Vec<&str> = diff["result"]["changes"].as_array().unwrap().iter()
        .filter_map(|c| c["change_type"].as_str()).collect();
    assert!(types.contains(&"REFACTORING"), "got {types:?}");
    finish(s, &base);
}

fn spawn_bare_with(extra: &[&str]) -> Session {
    let bin = env!("CARGO_BIN_EXE_intentumdiff-lsp-server");
    let dir = std::env::temp_dir();
    let mut child = Command::new(bin)
        .args(["--wasm-dir", &dir.join("no-such-wasm-dir").to_string_lossy()])
        .args(extra)
        .current_dir(&dir)
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
            while let Ok(Some(DecodeEvent::Frame(msg))) = decoder.next_event() {
                if tx.send(msg).is_err() {
                    return;
                }
            }
        }
    });
    Session { child, stdin, rx, seen_notifications: Vec::new() }
}

#[test]
fn document_ceiling_evicts_the_oldest_buffer_and_clears_it() {
    let mut s = spawn_bare_with(&["--max-open-documents", "2"]);
    initialize_bare(&mut s);
    for name in ["a", "b", "c"] {
        s.open(&format!("file:///lim/{name}.ts"), 1, "const x = 1;\n");
    }
    // No workspace root → nothing is computed; the only publish is the eviction of `a`.
    let cleared = s.recv_publish("file:///lim/a.ts");
    assert_eq!(cleared["diagnostics"], json!([]));
    finish_bare(s);
}

#[test]
fn bursts_stay_bounded_and_every_request_is_answered() {
    // A tiny queue forces back-pressure on every write; requests interleaved with
    // 10 KiB full-sync revisions must all be answered, in order.
    let mut s = spawn_bare_with(&["--max-pending-messages", "4", "--max-pending-bytes", "16384"]);
    initialize_bare(&mut s);
    let text = "x".repeat(10 * 1024);
    s.open("file:///burst/a.ts", 1, &text);
    let mut writer = s.stdin;
    let frames: Vec<Vec<u8>> = (2..402)
        .flat_map(|v| {
            let mut out = vec![encode_message(&json!({
                "jsonrpc": "2.0", "method": "textDocument/didChange",
                "params": {"textDocument": {"uri": "file:///burst/a.ts", "version": v},
                           "contentChanges": [{"text": format!("{text}{v}")}]}}))];
            if v % 8 == 0 {
                out.push(encode_message(&json!({
                    "jsonrpc": "2.0", "id": 1000 + v, "method": "textDocument/codeLens",
                    "params": {"textDocument": {"uri": "file:///burst/a.ts"}}})));
            }
            out
        })
        .collect();
    let feeder = std::thread::spawn(move || {
        for f in frames {
            writer.write_all(&f).expect("write");
        }
        writer.flush().expect("flush");
        writer
    });
    let mut answered = Vec::new();
    while answered.len() < 50 {
        // `s.stdin` is lent to the feeder; read the frame channel directly.
        let msg = s.rx.recv_timeout(Duration::from_secs(120)).expect("timed out waiting for a frame");
        if let Some(id) = msg.get("id").and_then(Value::as_i64) {
            assert!(msg["result"].is_array(), "got {msg}");
            answered.push(id);
        }
    }
    let expected: Vec<i64> = (2..402).filter(|v| v % 8 == 0).map(|v| 1000 + v).collect();
    assert_eq!(answered, expected);
    s.stdin = feeder.join().unwrap();
    finish_bare(s);
}

fn finish_bare(mut s: Session) {
    s.send(&json!({"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": Value::Null}));
    s.recv_response(99);
    s.send(&json!({"jsonrpc": "2.0", "method": "exit", "params": Value::Null}));
    let status = wait_with_deadline(&mut s.child, EXIT_DEADLINE).expect("exit");
    assert!(status.success());
}

#[test]
fn required_gate_rejects_unverified_tampered_or_unprovisioned_bundles() {
    let dir = unique_temp_dir();
    let bytes = b"\0asm fake component";
    std::fs::write(dir.join("js_ts_parser.wasm"), bytes).unwrap();
    let write = |verified: bool, sha: &str, wasm: &str| {
        std::fs::write(dir.join("provenance.json"), json!({
            "verified": verified, "registry_ref": "r",
            "components": {"js_ts_parser.wasm": {"sha256": sha}},
        }).to_string()).unwrap();
        std::fs::write(dir.join("parser_manifest.json"), json!({
            "schema_version": 1, "extension_index": {".ts": "typescript"},
            "parsers": {"typescript": {"wasm": wasm, "plugin_id": "typescript", "extensions": [".ts"]}},
        }).to_string()).unwrap();
    };
    let good = sha256_hex(bytes);
    write(true, &good, "js_ts_parser.wasm");
    let record = verify_provisioned_bundle(&dir).expect("a verified bundle is accepted");
    assert_eq!(record["components"]["js_ts_parser.wasm"], good.as_str());
    assert_eq!(record["binary"]["sha256"].as_str().unwrap().len(), 64);

    write(false, &good, "js_ts_parser.wasm");
    assert!(verify_provisioned_bundle(&dir).unwrap_err().contains("not registry-verified"));
    write(true, &"0".repeat(64), "js_ts_parser.wasm");
    assert!(verify_provisioned_bundle(&dir).unwrap_err().contains("does not match"));
    write(true, &good, "go_parser.wasm");
    assert!(verify_provisioned_bundle(&dir).unwrap_err().contains("unprovisioned"));
    std::fs::remove_file(dir.join("provenance.json")).unwrap();
    assert!(verify_provisioned_bundle(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
