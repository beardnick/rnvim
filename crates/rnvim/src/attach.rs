//! The attach client: puts the real terminal in raw mode and streams the
//! daemon's active session PTY. `Ctrl-\` is the prefix key:
//!
//!   Ctrl-\ d   detach (daemon and all sessions keep running)
//!   Ctrl-\ n/p next / previous session
//!   Ctrl-\ s   session list (j/k move, Enter switch, x kill, q back)
//!   Ctrl-\ c   new scratch session
//!   Ctrl-\ Ctrl-\  send a literal Ctrl-\ to nvim

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

use crate::daemon;
use crate::nvim;

const PREFIX: u8 = 0x1c; // Ctrl-\

struct RawGuard {
    orig: libc::termios,
}

impl RawGuard {
    fn enter() -> Result<RawGuard> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                bail!("stdin is not a terminal");
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            // Alternate screen + the input modes nvim expects (it enabled
            // them against the PTY at startup; a late-attaching terminal
            // never saw those sequences).
            print!("\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?2004h");
            let _ = std::io::stdout().flush();
            Ok(RawGuard { orig })
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        print!("\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1049l\x1b[?25h");
        let _ = std::io::stdout().flush();
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}

fn term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

/// Connect to the daemon, starting it (detached via setsid) if needed.
fn ensure_daemon() -> Result<UnixStream> {
    let path = daemon::control_socket_path()?;
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }

    let log_dir = nvim::rnvim_home()?.join("log");
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))?;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("spawn rnvim daemon")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("daemon did not come up (see ~/.rnvim/log/daemon.log)")
}

fn send(stream: &mut UnixStream, msg: &Value) -> Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).context("write to daemon")
}

enum Mode {
    Passthrough,
    Prefix,
    Manager {
        items: Vec<(u64, String, bool)>,
        selected: usize,
    },
    Detached,
}

fn draw_manager(items: &[(u64, String, bool)], selected: usize) {
    print!("\x1b[2J\x1b[H\x1b[1m rnvim sessions \x1b[0m  (Enter switch · x kill · c new · q back · d detach)\r\n\r\n");
    for (i, (_, title, active)) in items.iter().enumerate() {
        let cursor = if i == selected { ">" } else { " " };
        let mark = if *active { "*" } else { " " };
        if i == selected {
            print!("\x1b[7m{cursor} {mark} {title}\x1b[0m\r\n");
        } else {
            print!("{cursor} {mark} {title}\r\n");
        }
    }
    if items.is_empty() {
        print!("  (no sessions — press c to create one)\r\n");
    }
    let _ = std::io::stdout().flush();
}

pub fn run(target: Option<&str>) -> Result<i32> {
    let mut stream = ensure_daemon()?;
    stream.set_nonblocking(true)?;

    let _guard = RawGuard::enter()?;
    let (cols, rows) = term_size();
    let mut attach = json!({ "t": "attach", "cols": cols, "rows": rows });
    if let Some(t) = target {
        attach["target"] = json!(t);
    }
    send(&mut stream, &attach)?;

    let mut stdout = std::io::stdout();
    let mut mode = Mode::Passthrough;
    let mut sock_buf: Vec<u8> = Vec::new();
    let mut last_size = (cols, rows);
    let mut last_size_check = Instant::now();
    let mut stdin = std::io::stdin();

    // Non-blocking stdin as well; poll both with a small sleep.
    unsafe {
        let flags = libc::fcntl(0, libc::F_GETFL);
        libc::fcntl(0, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    loop {
        let mut progressed = false;

        // socket → screen / state
        let mut chunk = [0u8; 16 * 1024];
        match stream.read(&mut chunk) {
            Ok(0) => bail!("daemon closed the connection"),
            Ok(n) => {
                sock_buf.extend_from_slice(&chunk[..n]);
                progressed = true;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }
        while let Some(pos) = sock_buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = sock_buf.drain(..=pos).collect();
            let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            match msg.get("t").and_then(Value::as_str).unwrap_or("") {
                "output" => {
                    if !matches!(mode, Mode::Manager { .. }) {
                        if let Some(b64) = msg.get("b64").and_then(Value::as_str) {
                            if let Ok(data) = B64.decode(b64) {
                                let _ = stdout.write_all(&data);
                                let _ = stdout.flush();
                            }
                        }
                    }
                }
                "switched" => {
                    print!("\x1b[2J\x1b[H");
                    let _ = stdout.flush();
                    mode = Mode::Passthrough;
                    // Belt and braces: make sure the daemon has our real
                    // size for the session we just landed on.
                    let size = term_size();
                    last_size = size;
                    send(
                        &mut stream,
                        &json!({ "t": "resize", "cols": size.0, "rows": size.1 }),
                    )?;
                }
                "status" => {
                    if let Some(text) = msg.get("msg").and_then(Value::as_str) {
                        print!("\r\x1b[2K\x1b[90m[rnvim] {text}\x1b[0m");
                        let _ = stdout.flush();
                    }
                }
                "sessions" => {
                    if let Mode::Manager { items, selected } = &mut mode {
                        *items = parse_sessions(&msg);
                        *selected = (*selected).min(items.len().saturating_sub(1));
                        draw_manager(items, *selected);
                    }
                }
                "empty" => {
                    return Ok(0);
                }
                "error" => {
                    let text = msg.get("msg").and_then(Value::as_str).unwrap_or("unknown");
                    print!("\r\n\x1b[31m[rnvim] {text}\x1b[0m\r\n");
                    let _ = stdout.flush();
                }
                _ => {}
            }
        }

        // keyboard → daemon / mode machine
        let mut keys = [0u8; 1024];
        match stdin.read(&mut keys) {
            Ok(0) => {}
            Ok(n) => {
                progressed = true;
                handle_keys(&keys[..n], &mut mode, &mut stream)?;
                if matches!(mode, Mode::Detached) {
                    return Ok(0);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.into()),
        }

        // terminal resize (polled — no signal handler needed)
        if last_size_check.elapsed() > Duration::from_millis(300) {
            last_size_check = Instant::now();
            let size = term_size();
            if size != last_size {
                last_size = size;
                send(
                    &mut stream,
                    &json!({ "t": "resize", "cols": size.0, "rows": size.1 }),
                )?;
            }
        }

        if !progressed {
            std::thread::sleep(Duration::from_millis(8));
        }
    }
}

fn parse_sessions(msg: &Value) -> Vec<(u64, String, bool)> {
    msg.get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|s| {
                    Some((
                        s.get("id")?.as_u64()?,
                        s.get("title")?.as_str()?.to_string(),
                        s.get("active")?.as_bool().unwrap_or(false),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn handle_keys(keys: &[u8], mode: &mut Mode, stream: &mut UnixStream) -> Result<()> {
    let mut pending: Vec<u8> = Vec::new();
    for &b in keys {
        match mode {
            Mode::Passthrough => {
                if b == PREFIX {
                    *mode = Mode::Prefix;
                } else {
                    pending.push(b);
                }
            }
            Mode::Prefix => {
                *mode = Mode::Passthrough;
                match b {
                    b'd' => {
                        send(stream, &json!({ "t": "detach" }))?;
                        *mode = Mode::Detached;
                        return Ok(());
                    }
                    b'n' => send(stream, &json!({ "t": "cycle", "dir": 1 }))?,
                    b'p' => send(stream, &json!({ "t": "cycle", "dir": -1 }))?,
                    b'c' => send(stream, &json!({ "t": "new" }))?,
                    b's' => {
                        *mode = Mode::Manager {
                            items: vec![],
                            selected: 0,
                        };
                        send(stream, &json!({ "t": "list" }))?;
                    }
                    PREFIX => pending.push(PREFIX),
                    _ => {}
                }
            }
            Mode::Manager { items, selected } => match b {
                b'j' | 14 => {
                    if !items.is_empty() {
                        *selected = (*selected + 1) % items.len();
                        draw_manager(items, *selected);
                    }
                }
                b'k' | 16 => {
                    if !items.is_empty() {
                        *selected = (*selected + items.len() - 1) % items.len();
                        draw_manager(items, *selected);
                    }
                }
                b'\r' => {
                    if let Some((id, _, _)) = items.get(*selected) {
                        let id = *id;
                        send(stream, &json!({ "t": "focus", "id": id }))?;
                    }
                    *mode = Mode::Passthrough;
                }
                b'x' => {
                    if let Some((id, _, _)) = items.get(*selected) {
                        let id = *id;
                        send(stream, &json!({ "t": "kill", "id": id }))?;
                        send(stream, &json!({ "t": "list" }))?;
                    }
                }
                b'c' => {
                    send(stream, &json!({ "t": "new" }))?;
                    *mode = Mode::Passthrough;
                }
                b'd' => {
                    send(stream, &json!({ "t": "detach" }))?;
                    *mode = Mode::Detached;
                    return Ok(());
                }
                b'q' | 0x1b => {
                    send(stream, &json!({ "t": "redraw" }))?;
                    *mode = Mode::Passthrough;
                }
                _ => {}
            },
            Mode::Detached => return Ok(()),
        }
    }
    if !pending.is_empty() {
        send(
            stream,
            &json!({ "t": "input", "b64": B64.encode(&pending) }),
        )?;
    }
    Ok(())
}
