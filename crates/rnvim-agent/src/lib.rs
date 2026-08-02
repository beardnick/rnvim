//! The rnvim remote agent: a JSON-lines RPC server over stdio.
//!
//! Runs on the remote machine (spawned via `ssh host rnvim agent --stdio`).
//! Also used verbatim for `rnvim local:` loopback sessions and tests.

mod finder;

mod exec;
pub use exec::TOOLS_PATH;

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rnvim_proto::*;
use serde_json::{json, Value};

fn write_response(stdout: &Mutex<io::Stdout>, resp: &Response) {
    if let Ok(mut out) = serde_json::to_string(resp) {
        out.push('\n');
        let mut stdout = stdout.lock().unwrap();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }
}

pub fn run_stdio() -> Result<()> {
    let stdin = io::stdin().lock();
    let stdout = Arc::new(Mutex::new(io::stdout()));
    for line in stdin.lines() {
        let line = line.context("read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        // Installs download for minutes; never block the request loop.
        // Responses are id-matched, so out-of-order delivery is fine.
        if let Ok(req) = serde_json::from_str::<Request>(&line) {
            if req.method == "exec.run" {
                let stdout = Arc::clone(&stdout);
                std::thread::spawn(move || {
                    let resp = match dispatch(&req.method, req.params) {
                        Ok(result) => Response::ok(req.id, result),
                        Err(e) => Response::err(req.id, ERR_IO, e.to_string()),
                    };
                    write_response(&stdout, &resp);
                });
                continue;
            }
        }
        let resp = handle_line(&line);
        write_response(&stdout, &resp);
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
        "fs.findroot" => fs_findroot(serde_json::from_value(params)?),
        "exec.which" => exec_which(serde_json::from_value(params)?),
        "exec.run" => exec_run(serde_json::from_value(params)?),
        "find.files" => finder::find_files(serde_json::from_value(params)?),
        "find.grep" => finder::find_grep(serde_json::from_value(params)?),
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
pub(crate) fn expand(path: &str) -> PathBuf {
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

/// Walk up from `path` looking for the nearest directory containing any of
/// the marker files/dirs (LSP root detection, done on the remote fs).
fn fs_findroot(p: FindrootParams) -> Result<Value> {
    let start = expand(&p.path);
    let mut dir = if fs::metadata(&start).map(|m| m.is_dir()).unwrap_or(false) {
        start
    } else {
        start.parent().map(Path::to_path_buf).unwrap_or(start)
    };
    loop {
        if p.markers.iter().any(|m| dir.join(m).exists()) {
            return Ok(json!(FindrootResult {
                root: Some(dir.to_string_lossy().into_owned()),
            }));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Ok(json!(FindrootResult { root: None })),
        }
    }
}

/// Locate a binary through the user's login shell, so PATH additions from
/// profile files (~/go/bin, ~/.cargo/bin, ...) are honored — matching how
/// the LSP proxy will actually launch the server.
fn exec_which(p: WhichParams) -> Result<Value> {
    if p.name.is_empty()
        || !p
            .name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
    {
        return Err(anyhow!("invalid binary name: {:?}", p.name));
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let out = std::process::Command::new(shell)
        .arg("-lc")
        .arg(format!(
            "PATH=\"{}:$PATH\" command -v '{}'",
            exec::TOOLS_PATH,
            p.name
        ))
        .output()
        .context("run login shell")?;
    // Profile files may print noise; the path is the last non-empty line.
    let path = if out.status.success() {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    } else {
        None
    };
    Ok(json!(WhichResult { path }))
}

/// Run a client-supplied script (see exec.rs). Dispatched on its own
/// thread by run_stdio — installs and builds can take minutes.
fn exec_run(p: RunParams) -> Result<Value> {
    let out = exec::run(&p.script)?;
    Ok(json!(RunResult {
        code: out.code,
        stdout: out.stdout,
        stderr: out.stderr,
    }))
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
    fn findroot_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/go.mod"), "module x").unwrap();
        fs::write(root.join("a/b/c/main.go"), "package x").unwrap();

        let resp = call(
            "fs.findroot",
            json!({ "path": root.join("a/b/c/main.go").to_str().unwrap(), "markers": ["go.work", "go.mod"] }),
        );
        let r: FindrootResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(r.root.as_deref(), root.join("a").to_str());

        let resp = call(
            "fs.findroot",
            json!({ "path": root.join("a/b/c/main.go").to_str().unwrap(), "markers": ["Cargo.toml"] }),
        );
        let r: FindrootResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(r.root.is_none());
    }

    #[test]
    fn which_finds_binaries() {
        let resp = call("exec.which", json!({ "name": "sh" }));
        let r: WhichResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(r.path.is_some(), "sh should exist everywhere");

        let resp = call(
            "exec.which",
            json!({ "name": "definitely-not-a-real-binary-xyz" }),
        );
        let r: WhichResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert!(r.path.is_none());

        let resp = call("exec.which", json!({ "name": "evil; rm -rf /" }));
        assert!(
            resp.error.is_some(),
            "shell metacharacters must be rejected"
        );
    }

    #[test]
    fn unknown_method_and_bad_json() {
        let resp = call("fs.nope", json!({}));
        assert!(resp.error.unwrap().message.contains("unknown method"));

        let resp = handle_line("not json");
        assert_eq!(resp.error.unwrap().code, ERR_PARSE);
    }
}
