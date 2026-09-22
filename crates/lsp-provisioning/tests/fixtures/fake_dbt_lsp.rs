//! Test-only stand-in for `dbt lsp-server --stdio`: speaks just enough real LSP-over-stdio
//! (Content-Length framing, `initialize`, `textDocument/didOpen`, `textDocument/hover`,
//! `textDocument/didClose`, `shutdown`, `exit`) to drive a genuine subprocess + real pipes
//! exchange in `tests/dbt_enrichment.rs`.
//!
//! This is explicitly **not** a claim that it behaves like real dbt Fusion — it exists
//! because CI has no dbt install and no network path to dbt Labs' CDN. It proves the
//! plumbing (spawn the resolved command, do a real `initialize`, open/hover/close a real
//! `dbt-sql` document over real OS pipes) that would carry an actual Fusion binary in
//! production. See `tests/dbt_enrichment.rs` for the real-Fusion-if-present companion test.
//!
//! Deliberately dependency-free (`std` + `serde_json` only) — it is not part of the shipped
//! product, only a build target of this crate's own test suite.

use std::io::{self, Read, Write};

fn read_message(stdin: &mut dyn Read) -> Option<serde_json::Value> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    let mut content_length: Option<usize> = None;
    loop {
        // Read one header line at a time (until a bare CRLF ends the section).
        header.clear();
        loop {
            if stdin.read_exact(&mut byte).is_err() {
                return None;
            }
            header.push(byte[0]);
            if header.ends_with(b"\r\n") {
                break;
            }
        }
        if header == b"\r\n" {
            break;
        }
        let line = String::from_utf8_lossy(&header);
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut body = vec![0u8; len];
    stdin.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(stdout: &mut dyn Write, msg: &serde_json::Value) {
    let body = msg.to_string();
    let _ = write!(stdout, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = stdout.flush();
}

fn main() {
    // Also stands in for `dbt --version --format json` (used by discovery's version probe)
    // so the SAME fixture binary can sit on PATH as "dbt" for both the diagnose() call and
    // the subsequent stdio launch — matching how a single real Fusion binary serves both.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!(r#"{{"version": "2.0.0-preview.95"}}"#);
        return;
    }

    let stdin = io::stdin();
    let mut lock_in = stdin.lock();
    let stdout = io::stdout();
    let mut lock_out = stdout.lock();

    loop {
        let Some(msg) = read_message(&mut lock_in) else { break };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                let reply = serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "capabilities": { "hoverProvider": true } }
                });
                write_message(&mut lock_out, &reply);
            }
            "shutdown" => {
                let reply = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": null });
                write_message(&mut lock_out, &reply);
            }
            "exit" => break,
            "textDocument/hover" => {
                // A minimal, deterministic "type" so the test can assert on it — real
                // Fusion would return actual dbt-sql/dbt-yaml column type info here.
                let uri = msg["params"]["textDocument"]["uri"].as_str().unwrap_or("");
                let reply = serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "contents": format!("varchar (from {uri})") }
                });
                write_message(&mut lock_out, &reply);
            }
            // Notifications (didOpen/didChange/didClose/initialized) — no response required.
            _ => {}
        }
    }
}
