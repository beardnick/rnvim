//! LSP stdio proxy with prefix path rewriting.
//!
//! Sits between the local nvim LSP client and a language server running on
//! the remote host (`ssh host <server>`). Every Content-Length frame is
//! parsed and every JSON string value is rewritten at the path boundary:
//!
//!   nvim → server: `file://<ws_root>/x` → `file:///x`, `<ws_root>/x` → `/x`
//!   server → nvim: `file:///x` → `file://<ws_root>/x`, and plain strings
//!                  starting with the remote workspace root get prefixed
//!
//! The remote workspace root needed for the reverse plain-path rule is
//! captured from the `initialize` request after rewriting — no extra
//! configuration channel required.
//!
//! Known limitation (documented): string values that merely *contain* a path
//! mid-string (e.g. compiler messages) are not rewritten; only values that
//! start at a path boundary are. Percent-encoded URIs are not handled yet.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// True if `rest` continues a path cleanly after a prefix match, so that
/// ws_root `/a/b` never matches `/a/bc`.
fn boundary(rest: &str) -> bool {
    rest.is_empty() || rest.starts_with('/')
}

/// Rewrite one string value in the nvim→server direction.
fn to_remote(ws_root: &str, s: &str) -> Option<String> {
    if let Some(rest) = s
        .strip_prefix("file://")
        .and_then(|u| u.strip_prefix(ws_root))
    {
        if boundary(rest) {
            return Some(if rest.is_empty() {
                "file:///".to_string()
            } else {
                format!("file://{rest}")
            });
        }
    }
    if let Some(rest) = s.strip_prefix(ws_root) {
        if boundary(rest) {
            return Some(if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            });
        }
    }
    None
}

/// Rewrite one string value in the server→nvim direction.
fn to_local(ws_root: &str, remote_root: Option<&str>, s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("file://") {
        if rest.starts_with('/') {
            return Some(format!("file://{ws_root}{rest}"));
        }
    }
    if let Some(rr) = remote_root {
        if let Some(rest) = s.strip_prefix(rr) {
            if boundary(rest) {
                return Some(format!("{ws_root}{s}"));
            }
        }
    }
    None
}

fn rewrite_strings(v: &mut Value, f: &dyn Fn(&str) -> Option<String>) {
    match v {
        Value::String(s) => {
            if let Some(new) = f(s) {
                *s = new;
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_strings(item, f);
            }
        }
        Value::Object(map) => {
            for (_, val) in map.iter_mut() {
                rewrite_strings(val, f);
            }
        }
        _ => {}
    }
}

/// Read one `Content-Length`-framed message; None on clean EOF.
fn read_frame(r: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).context("read frame header")? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_len = Some(v.trim().parse().context("parse Content-Length")?);
        }
    }
    let n = content_len.context("frame without Content-Length")?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).context("read frame body")?;
    Ok(Some(buf))
}

fn write_frame(w: &mut impl Write, body: &[u8]) -> Result<()> {
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

/// Extract the remote workspace root from an already-rewritten `initialize`
/// request (rootUri preferred, first workspaceFolder as fallback).
fn capture_remote_root(msg: &Value) -> Option<String> {
    if msg.get("method")?.as_str()? != "initialize" {
        return None;
    }
    let params = msg.get("params")?;
    let from_uri = |u: &str| u.strip_prefix("file://").map(str::to_string);
    if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
        return from_uri(uri);
    }
    params
        .get("workspaceFolders")?
        .as_array()?
        .first()?
        .get("uri")?
        .as_str()
        .and_then(from_uri)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn spawn_server(host: &str, server_cmd: &[String]) -> Result<Child> {
    let mut cmd = if host == "local" {
        let mut c = Command::new(&server_cmd[0]);
        c.args(&server_cmd[1..]);
        if let (Ok(home), Ok(path)) = (std::env::var("HOME"), std::env::var("PATH")) {
            c.env(
                "PATH",
                format!("{home}/.rnvim/tools/bin:{home}/.rnvim/tools/npm/node_modules/.bin:{path}"),
            );
        }
        c
    } else {
        // Through the user's login shell so PATH from profile files applies,
        // with rnvim-installed tools prepended (mirrors exec.which).
        let quoted: Vec<String> = server_cmd.iter().map(|a| shell_quote(a)).collect();
        let script = format!(
            "PATH=\"{}:$PATH\" exec {}",
            rnvim_agent::TOOLS_PATH,
            quoted.join(" ")
        );
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            host,
            &format!("exec \"${{SHELL:-/bin/sh}}\" -lc {}", shell_quote(&script)),
        ]);
        c
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    cmd.spawn()
        .with_context(|| format!("spawn language server: {server_cmd:?}"))
}

pub fn run(host: &str, ws_root: &str, server_cmd: &[String]) -> Result<i32> {
    if server_cmd.is_empty() {
        bail!("no language server command given (use `-- <cmd> [args...]`)");
    }
    let ws_root = ws_root.trim_end_matches('/').to_string();
    let mut child = spawn_server(host, server_cmd)?;
    let mut child_in = child.stdin.take().context("server stdin")?;
    let mut child_out = BufReader::new(child.stdout.take().context("server stdout")?);

    let remote_root: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // nvim stdin → server
    let rr_tx = Arc::clone(&remote_root);
    let ws_tx = ws_root.clone();
    let to_server = std::thread::spawn(move || -> Result<()> {
        let mut stdin = BufReader::new(std::io::stdin());
        while let Some(frame) = read_frame(&mut stdin)? {
            let mut msg: Value = serde_json::from_slice(&frame).context("parse client frame")?;
            rewrite_strings(&mut msg, &|s| to_remote(&ws_tx, s));
            if let Some(root) = capture_remote_root(&msg) {
                *rr_tx.lock().unwrap() = Some(root);
            }
            write_frame(&mut child_in, &serde_json::to_vec(&msg)?)?;
        }
        Ok(())
    });

    // server → nvim stdout
    let mut stdout = std::io::stdout();
    while let Some(frame) = read_frame(&mut child_out)? {
        let mut msg: Value = serde_json::from_slice(&frame).context("parse server frame")?;
        let rr = remote_root.lock().unwrap().clone();
        rewrite_strings(&mut msg, &|s| to_local(&ws_root, rr.as_deref(), s));
        write_frame(&mut stdout, &serde_json::to_vec(&msg)?)?;
    }

    let _ = to_server.join();
    let status = child.wait().context("wait language server")?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WS: &str = "/Users/me/.rnvim/ws/dev";

    #[test]
    fn to_remote_rules() {
        assert_eq!(
            to_remote(WS, &format!("file://{WS}/home/q/main.go")).as_deref(),
            Some("file:///home/q/main.go")
        );
        assert_eq!(
            to_remote(WS, &format!("{WS}/home/q")).as_deref(),
            Some("/home/q")
        );
        assert_eq!(to_remote(WS, WS).as_deref(), Some("/"));
        assert_eq!(
            to_remote(WS, &format!("file://{WS}")).as_deref(),
            Some("file:///")
        );
        // boundary: /devX must not match /dev
        assert_eq!(to_remote(WS, &format!("{WS}X/nope")), None);
        assert_eq!(to_remote(WS, "unrelated text"), None);
    }

    #[test]
    fn to_local_rules() {
        assert_eq!(
            to_local(WS, None, "file:///home/q/main.go").as_deref(),
            Some(format!("file://{WS}/home/q/main.go").as_str())
        );
        assert_eq!(
            to_local(WS, Some("/home/q/proj"), "/home/q/proj/lib/x.go").as_deref(),
            Some(format!("{WS}/home/q/proj/lib/x.go").as_str())
        );
        // plain paths outside the workspace root are left alone
        assert_eq!(to_local(WS, Some("/home/q/proj"), "/usr/lib/foo"), None);
        // without a captured root, plain paths are never touched
        assert_eq!(to_local(WS, None, "/home/q/proj/lib/x.go"), None);
    }

    #[test]
    fn rewrites_nested_structures() {
        let mut msg = json!({
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": format!("file://{WS}/p/a.go"),
                    "text": "package main // file:// mentions inside code are untouched"
                },
                "related": [ { "location": { "uri": format!("file://{WS}/p/b.go") } } ]
            }
        });
        rewrite_strings(&mut msg, &|s| to_remote(WS, s));
        assert_eq!(msg["params"]["textDocument"]["uri"], "file:///p/a.go");
        assert_eq!(
            msg["params"]["related"][0]["location"]["uri"],
            "file:///p/b.go"
        );
        assert!(msg["params"]["textDocument"]["text"]
            .as_str()
            .unwrap()
            .contains("untouched"));
    }

    #[test]
    fn captures_initialize_root() {
        let msg = json!({
            "method": "initialize",
            "params": { "rootUri": "file:///home/q/proj" }
        });
        assert_eq!(capture_remote_root(&msg).as_deref(), Some("/home/q/proj"));

        let msg = json!({
            "method": "initialize",
            "params": { "workspaceFolders": [ { "uri": "file:///w1" } ] }
        });
        assert_eq!(capture_remote_root(&msg).as_deref(), Some("/w1"));

        let msg = json!({ "method": "textDocument/hover", "params": {} });
        assert_eq!(capture_remote_root(&msg), None);
    }

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"x":1}"#).unwrap();
        let mut r = std::io::Cursor::new(buf);
        let frame = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(frame, br#"{"x":1}"#);
        assert!(read_frame(&mut r).unwrap().is_none(), "clean EOF");
    }
}
