//! End-to-end test of `rnvim lsp-proxy`: spawn the real binary against a
//! fake LSP server and assert both rewrite directions across the wire.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

const WS: &str = "/virt/ws/dev-box";
const REMOTE_ROOT: &str = "/home/q/proj";

fn write_frame(w: &mut impl Write, msg: &Value) {
    let body = serde_json::to_vec(msg).unwrap();
    write!(w, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    w.write_all(&body).unwrap();
    w.flush().unwrap();
}

fn read_frame(r: &mut impl BufRead) -> Value {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        assert!(
            r.read_line(&mut line).unwrap() > 0,
            "unexpected EOF from proxy"
        );
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap();
        }
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).unwrap();
    serde_json::from_slice(&buf).unwrap()
}

#[test]
fn proxy_rewrites_both_directions() {
    let fake_lsp = format!("{}/tests/fake_lsp.py", env!("CARGO_MANIFEST_DIR"));
    let mut proxy = Command::new(env!("CARGO_BIN_EXE_rnvim"))
        .args([
            "lsp-proxy",
            "--host",
            "local",
            "--ws-root",
            WS,
            "--",
            "python3",
            &fake_lsp,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn proxy");
    let mut stdin = proxy.stdin.take().unwrap();
    let mut stdout = BufReader::new(proxy.stdout.take().unwrap());

    // nvim -> server: workspace paths must arrive as remote paths.
    write_frame(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "rootUri": format!("file://{WS}{REMOTE_ROOT}"),
                "rootPath": format!("{WS}{REMOTE_ROOT}"),
                "capabilities": {}
            }
        }),
    );
    let resp = read_frame(&mut stdout);
    assert_eq!(
        resp["result"]["sawRootUri"],
        format!("saw:file://{REMOTE_ROOT}")
    );
    assert_eq!(resp["result"]["sawRootPath"], format!("saw:{REMOTE_ROOT}"));

    // server -> nvim: URIs and plain remote-root paths come back localized.
    write_frame(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "textDocument/definition",
            "params": { "textDocument": { "uri": format!("file://{WS}{REMOTE_ROOT}/main.go") } }
        }),
    );
    let resp = read_frame(&mut stdout);
    assert_eq!(
        resp["result"]["uri"],
        format!("file://{WS}{REMOTE_ROOT}/lib/def.go")
    );
    assert_eq!(
        resp["result"]["detail"],
        format!("{WS}{REMOTE_ROOT}/lib/def.go")
    );

    write_frame(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);
    let status = proxy.wait().unwrap();
    assert!(status.success(), "proxy exit: {status}");
}
