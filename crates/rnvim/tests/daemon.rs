//! Integration test for the daemon: attach over the control socket, type
//! into the PTY-hosted nvim, verify the edit lands on disk through the
//! agent, then detach and reattach.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct DaemonUnderTest {
    child: Child,
    home: tempfile::TempDir,
}

impl Drop for DaemonUnderTest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn real_nvim() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let versions = Path::new(&home).join(".rnvim/versions");
    for entry in std::fs::read_dir(versions).ok()?.flatten() {
        for sub in std::fs::read_dir(entry.path()).ok()?.flatten() {
            let bin = sub.path().join("bin/nvim");
            if bin.exists() {
                return Some(bin);
            }
        }
    }
    None
}

fn start_daemon() -> DaemonUnderTest {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rnvim"));
    let log = std::fs::File::create("/tmp/rnvim-test-daemon.log").expect("log file");
    cmd.arg("daemon")
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    if let Some(nvim) = real_nvim() {
        cmd.env("RNVIM_NVIM_BIN", nvim);
    }
    let child = cmd.spawn().expect("spawn daemon");
    DaemonUnderTest { child, home }
}

fn connect(home: &Path) -> UnixStream {
    let sock = home.join(format!(
        ".rnvim/run/daemon-{}.sock",
        env!("CARGO_PKG_VERSION")
    ));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            return s;
        }
        assert!(Instant::now() < deadline, "daemon socket never appeared");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn send(stream: &mut UnixStream, msg: &Value) {
    let mut line = msg.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .expect("write control msg");
}

fn send_keys(stream: &mut UnixStream, keys: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(keys.as_bytes());
    send(stream, &json!({ "t": "input", "b64": b64 }));
}

/// Buffered message reader that survives read timeouts mid-line (output
/// frames are >20KB single lines; BufReader::read_line would lose the
/// partial data on a timeout tick).
struct MsgReader {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl MsgReader {
    fn new(stream: UnixStream) -> MsgReader {
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        MsgReader {
            stream,
            buf: Vec::new(),
        }
    }

    /// Read control messages until `pred` matches (or panic on timeout).
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        use std::io::Read;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut chunk = [0u8; 64 * 1024];
        while Instant::now() < deadline {
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                if let Ok(msg) = serde_json::from_slice::<Value>(&line) {
                    if pred(&msg) {
                        return msg;
                    }
                }
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("daemon closed while waiting for {what}"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => {} // timeout tick; keep polling until deadline
            }
        }
        panic!("timed out waiting for {what}");
    }
}

fn is_type(msg: &Value, t: &str) -> bool {
    msg.get("t").and_then(Value::as_str) == Some(t)
}

#[test]
fn attach_edit_detach_reattach() {
    let daemon = start_daemon();
    let workdir = tempfile::tempdir().expect("workdir");

    // --- attach with a target: session opens on the loopback agent
    let mut stream = connect(daemon.home.path());
    let mut reader = MsgReader::new(stream.try_clone().unwrap());
    send(
        &mut stream,
        &json!({
            "t": "attach", "cols": 100, "rows": 30,
            "target": format!("local:{}", workdir.path().display()),
        }),
    );
    let switched = reader.wait_for("switched", |m| is_type(m, "switched"));
    let session_id = switched["id"].as_u64().expect("session id");
    reader.wait_for("first output frame", |m| is_type(m, "output"));

    // --- type an edit through the PTY and save it via the virtual fs
    // (`%` = the directory buffer's workspace path, so the new file goes
    // through the ws prefix and the agent, not nvim's local cwd).
    // Generous waits: without a real terminal answering nvim's startup DSR
    // queries, nvim starts slower (E1568) and early keys would be lost.
    std::thread::sleep(Duration::from_secs(5));
    send_keys(&mut stream, ":e %/note.txt\r");
    std::thread::sleep(Duration::from_millis(1500));
    send_keys(&mut stream, "ihello from the daemon\x1b");
    std::thread::sleep(Duration::from_millis(800));
    send_keys(&mut stream, ":w\r");

    let note = workdir.path().join("note.txt");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(content) = std::fs::read_to_string(&note) {
            assert!(
                content.contains("hello from the daemon"),
                "content: {content:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "note.txt never written");
        std::thread::sleep(Duration::from_millis(200));
    }

    // --- detach; the daemon and the session must survive
    send(&mut stream, &json!({ "t": "detach" }));
    drop(reader);
    drop(stream);
    std::thread::sleep(Duration::from_millis(500));

    // --- reattach: same session still there, still rendering
    let mut stream = connect(daemon.home.path());
    let mut reader = MsgReader::new(stream.try_clone().unwrap());
    send(
        &mut stream,
        &json!({ "t": "attach", "cols": 100, "rows": 30 }),
    );
    let switched = reader.wait_for("reattach switch", |m| is_type(m, "switched"));
    assert_eq!(
        switched["id"].as_u64(),
        Some(session_id),
        "same session after reattach"
    );
    reader.wait_for("post-reattach output", |m| is_type(m, "output"));

    // --- resize invalidates diff baselines: the next paint must be a full
    // frame (2J-prefixed), never a cross-size diff
    send(
        &mut stream,
        &json!({ "t": "resize", "cols": 120, "rows": 40 }),
    );
    reader.wait_for("full frame after resize", |m| {
        use base64::Engine;
        is_type(m, "output")
            && m["b64"]
                .as_str()
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .map(|bytes| bytes.starts_with(b"\x1b[?2026h\x1b[2J"))
                .unwrap_or(false)
    });

    // --- slow-consumer coalescing: burst input without reading, then
    // drain — frames must coalesce to the latest state, not replay a
    // backlog (a slow terminal must never fall minutes behind)
    for _ in 0..40 {
        send_keys(&mut stream, "j");
        std::thread::sleep(Duration::from_millis(20));
        send_keys(&mut stream, "k");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(1));
    let mut output_frames = 0;
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < drain_deadline {
        use std::io::Read;
        let mut chunk = [0u8; 256 * 1024];
        match reader.stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => reader.buf.extend_from_slice(&chunk[..n]),
            Err(_) => {}
        }
        while let Some(pos) = reader.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = reader.buf.drain(..=pos).collect();
            if let Ok(msg) = serde_json::from_slice::<Value>(&line) {
                if is_type(&msg, "output") {
                    output_frames += 1;
                }
            }
        }
    }
    assert!(
        output_frames < 15,
        "slow consumer should get coalesced frames, got {output_frames}"
    );

    // --- second session via the connect path the picker uses
    send(
        &mut stream,
        &json!({ "t": "new", "target": format!("local:{}", workdir.path().display()) }),
    );
    let msg = reader.wait_for("two sessions listed", |m| {
        is_type(m, "sessions") && m["items"].as_array().map(|a| a.len()) == Some(2)
    });
    let titles: Vec<&str> = msg["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["title"].as_str())
        .collect();
    assert!(
        titles.iter().all(|t| t.contains("local:")),
        "titles: {titles:?}"
    );
}
