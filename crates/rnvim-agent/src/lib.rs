//! The rnvim remote agent: a JSON-lines RPC server over stdio.
//!
//! Runs on the remote machine (spawned via `ssh host rnvim agent --stdio`).
//! Also used verbatim for `rnvim local:` loopback sessions and tests.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rnvim_proto::*;
use serde_json::{json, Value};

pub fn run_stdio() -> Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    for line in stdin.lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = handle_line(&line);
        let mut out = serde_json::to_string(&resp).context("encode response")?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).context("write stdout")?;
        stdout.flush().context("flush stdout")?;
    }
    Ok(())
}

pub fn handle_line(line: &str) -> Response {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => match dispatch(&req.method, req.params) {
            Ok(result) => Response::ok(req.id, result),
            Err(e) => Response::err(req.id, ERR_IO, e.to_string()),
        },
        Err(e) => Response::err(0, ERR_PARSE, format!("parse error: {e}")),
    }
}

fn dispatch(method: &str, params: Value) -> Result<Value> {
    match method {
        "hello" => hello(serde_json::from_value(params)?),
        "fs.resolve" => fs_resolve(serde_json::from_value(params)?),
        "fs.stat" => fs_stat(serde_json::from_value(params)?),
        "fs.read" => fs_read(serde_json::from_value(params)?),
        "fs.write" => fs_write(serde_json::from_value(params)?),
        "fs.list" => fs_list(serde_json::from_value(params)?),
        other => Err(anyhow!("unknown method: {other}")),
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand `~` and make relative paths home-relative (ssh convention), then
/// normalize `.` and `..` lexically. Never touches the filesystem.
fn expand(path: &str) -> PathBuf {
    let path = if path.is_empty() { "~" } else { path };
    let expanded = if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        home_dir().join(path)
    };
    let mut out = PathBuf::new();
    for comp in expanded.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

fn kind_of(path: &Path) -> &'static str {
    match fs::metadata(path) {
        Ok(m) if m.is_dir() => "dir",
        Ok(_) => "file",
        Err(_) => "missing",
    }
}

fn hello(p: HelloParams) -> Result<Value> {
    if p.proto != PROTO_VERSION {
        return Err(anyhow!(
            "protocol mismatch: client {} vs agent {}",
            p.proto,
            PROTO_VERSION
        ));
    }
    Ok(json!(HelloResult {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        proto: PROTO_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        home: home_dir().to_string_lossy().into_owned(),
    }))
}

fn fs_resolve(p: ResolveParams) -> Result<Value> {
    let abs = expand(&p.path);
    Ok(json!(ResolveResult {
        abs: abs.to_string_lossy().into_owned(),
        kind: kind_of(&abs).to_string(),
    }))
}

fn fs_stat(p: StatParams) -> Result<Value> {
    let path = expand(&p.path);
    let kind = kind_of(&path);
    let size = if kind == "file" {
        fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    Ok(json!(StatResult {
        kind: kind.to_string(),
        size
    }))
}

fn fs_read(p: ReadParams) -> Result<Value> {
    let path = expand(&p.path);
    let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(json!(ReadResult {
        content_b64: B64.encode(&data),
        size: data.len() as u64
    }))
}

fn fs_write(p: WriteParams) -> Result<Value> {
    let path = expand(&p.path);
    let data = B64
        .decode(p.content_b64.as_bytes())
        .context("decode content")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
    Ok(json!(WriteResult {
        bytes: data.len() as u64
    }))
}

fn fs_list(p: ListParams) -> Result<Value> {
    let path = expand(&p.path);
    let mut entries = Vec::new();
    for entry in fs::read_dir(&path).with_context(|| format!("list {}", path.display()))? {
        let entry = entry?;
        let kind = if entry.path().is_dir() { "dir" } else { "file" };
        entries.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind: kind.to_string(),
        });
    }
    entries.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
    Ok(json!(ListResult { entries }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Response {
        let req = json!({ "id": 7, "method": method, "params": params }).to_string();
        handle_line(&req)
    }

    #[test]
    fn hello_checks_proto() {
        let resp = call(
            "hello",
            json!({ "client_version": "0.1.0", "proto": PROTO_VERSION }),
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let r: HelloResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(r.proto, PROTO_VERSION);

        let resp = call("hello", json!({ "client_version": "0.1.0", "proto": 999 }));
        assert!(resp.error.is_some());
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested/hello.txt");

        let resp = call(
            "fs.write",
            json!({ "path": file.to_str().unwrap(), "content_b64": B64.encode("hi rnvim\n") }),
        );
        assert!(resp.error.is_none(), "{:?}", resp.error);

        let resp = call("fs.read", json!({ "path": file.to_str().unwrap() }));
        let r: ReadResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(B64.decode(r.content_b64).unwrap(), b"hi rnvim\n");
    }

    #[test]
    fn stat_and_list() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();

        let resp = call("fs.stat", json!({ "path": dir.path().to_str().unwrap() }));
        let r: StatResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(r.kind, "dir");

        let resp = call("fs.list", json!({ "path": dir.path().to_str().unwrap() }));
        let r: ListResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        let names: Vec<_> = r.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt"], "dirs sort first");

        let resp = call(
            "fs.stat",
            json!({ "path": dir.path().join("nope").to_str().unwrap() }),
        );
        let r: StatResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(r.kind, "missing");
    }

    #[test]
    fn expand_paths() {
        let home = home_dir();
        assert_eq!(expand("~"), home);
        assert_eq!(expand("~/x"), home.join("x"));
        assert_eq!(expand("proj/x"), home.join("proj/x"));
        assert_eq!(expand("/a/b/../c/./d"), PathBuf::from("/a/c/d"));
        assert_eq!(expand(""), home);
    }

    #[test]
    fn unknown_method_and_bad_json() {
        let resp = call("fs.nope", json!({}));
        assert!(resp.error.unwrap().message.contains("unknown method"));

        let resp = handle_line("not json");
        assert_eq!(resp.error.unwrap().code, ERR_PARSE);
    }
}
