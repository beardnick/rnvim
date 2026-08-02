//! The rnvim daemon: a PTY host that owns nvim instances (one per session)
//! and the shared agent router. Clients attach over a control socket,
//! stream the active session's PTY, and can detach/reattach at will.
//!
//! Control protocol (JSON lines on ~/.rnvim/run/daemon.sock):
//!   client → daemon:
//!     {t:"attach", cols, rows, target?}   take over as the (single) client
//!     {t:"input", b64}                    keystrokes for the active session
//!     {t:"resize", cols, rows}
//!     {t:"focus", id} | {t:"cycle", dir}  switch sessions
//!     {t:"list"} {t:"kill", id} {t:"new", target?} {t:"redraw"} {t:"detach"}
//!   daemon → client:
//!     {t:"output", b64}                   active session PTY bytes
//!     {t:"sessions", items:[{id,title,active}]}
//!     {t:"switched", id, title} {t:"error", msg} {t:"empty"}
//!
//! Rendering: every session's PTY output feeds a vt100 virtual screen; the
//! client is painted exclusively from that state — minimal diffs while
//! streaming, a full deterministic frame on attach/switch. nvim is never
//! asked to redraw, and no escape sequence can be cut mid-stream.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, PtySize};
use serde_json::{json, Value};

use crate::nvim::{self, LaunchOpts};
use crate::remotes;
use crate::session::Router;
use crate::target::Target;

/// Version-stamped socket name: a client always talks to a daemon of its
/// own version — after an upgrade the new client simply starts a fresh
/// daemon instead of speaking a stale protocol to an old one.
pub fn control_socket_path() -> Result<std::path::PathBuf> {
    Ok(nvim::rnvim_home()?
        .join("run")
        .join(format!("daemon-{}.sock", env!("CARGO_PKG_VERSION"))))
}

struct Session {
    id: u64,
    title: Mutex<String>,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send>>,
    /// The session's virtual screen: every PTY byte goes through this
    /// terminal emulator, and clients are painted purely from its state.
    /// Always full frames — vt100's contents_diff has known correctness
    /// holes (Google's shpool forked the crate over them; we saw floats
    /// leave stale rectangles), so no incremental path exists to get
    /// wrong. Frames are wrapped in synchronized-output (CSI ?2026) so
    /// supporting terminals apply them atomically, flicker-free.
    parser: Mutex<vt100::Parser>,
    /// The frame as last painted, to skip sends when nothing changed.
    last_frame: Mutex<Option<Vec<u8>>>,
    /// (rows, cols) last applied to the PTY + parser, for apply_size.
    applied_size: Mutex<(u16, u16)>,
}

/// Width of the session sidebar (including the separator column).
const SIDEBAR_WIDTH: u16 = 26;
/// Below this total width the sidebar auto-hides.
const SIDEBAR_MIN_COLS: u16 = SIDEBAR_WIDTH + 40;

fn push_color(sgr: &mut String, color: vt100::Color, fg: bool) {
    use std::fmt::Write;
    match color {
        vt100::Color::Default => {
            let _ = write!(sgr, ";{}", if fg { 39 } else { 49 });
        }
        vt100::Color::Idx(n) => {
            let _ = write!(sgr, ";{};5;{}", if fg { 38 } else { 48 }, n);
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(sgr, ";{};2;{};{};{}", if fg { 38 } else { 48 }, r, g, b);
        }
    }
}

/// Full-screen frame, serialized cell by cell: every cell — blanks
/// included — is written explicitly with its own colors. This deliberately
/// avoids EL/BCE (erase-with-background), which vt100's own serializer
/// leans on and which weaker embedded terminals don't implement (blank
/// regions rendered as default background). Wrapped in synchronized
/// output so capable terminals apply the frame atomically.
#[cfg_attr(not(test), allow(dead_code))] // exercised by the rendering tests
fn full_frame(screen: &vt100::Screen) -> Vec<u8> {
    let mut out: Vec<u8> = b"\x1b[?2026h\x1b[2J".to_vec();
    render_grid(&mut out, screen, 0);
    finish_frame(&mut out, screen, 0);
    out
}

fn finish_frame(out: &mut Vec<u8>, screen: &vt100::Screen, col_offset: u16) {
    let (cursor_row, cursor_col) = screen.cursor_position();
    out.extend_from_slice(
        format!(
            "\x1b[m\x1b[{};{}H",
            cursor_row + 1,
            cursor_col + 1 + col_offset
        )
        .as_bytes(),
    );
    out.extend_from_slice(if screen.hide_cursor() {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
    out.extend_from_slice(b"\x1b[?2026l");
}

/// Paint the whole grid, every cell explicit, shifted right by col_offset.
fn render_grid(out: &mut Vec<u8>, screen: &vt100::Screen, col_offset: u16) {
    let (rows, cols) = screen.size();
    let mut current_sgr = String::new();

    for row in 0..rows {
        out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col_offset + 1).as_bytes());
        let mut col = 0;
        while col < cols {
            let Some(cell) = screen.cell(row, col) else {
                break;
            };
            let mut sgr = String::from("0");
            if cell.bold() {
                sgr.push_str(";1");
            }
            if cell.italic() {
                sgr.push_str(";3");
            }
            if cell.underline() {
                sgr.push_str(";4");
            }
            if cell.inverse() {
                sgr.push_str(";7");
            }
            push_color(&mut sgr, cell.fgcolor(), true);
            push_color(&mut sgr, cell.bgcolor(), false);
            if sgr != current_sgr {
                out.extend_from_slice(format!("\x1b[{sgr}m").as_bytes());
                current_sgr = sgr;
            }
            let contents = cell.contents();
            if contents.is_empty() {
                out.push(b' ');
            } else {
                out.extend_from_slice(contents.as_bytes());
            }
            // A wide glyph occupies the next cell too; don't overwrite it.
            col += if cell.is_wide() { 2 } else { 1 };
        }
    }
}

/// Paint the session sidebar into columns 1..=SIDEBAR_WIDTH: header, one
/// row per session (active inverted), separator column, every cell
/// explicit per the weakest-terminal rule.
fn render_sidebar(out: &mut Vec<u8>, items: &[(String, bool)], rows: u16) {
    let text_w = (SIDEBAR_WIDTH - 1) as usize;
    for row in 1..=rows {
        out.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        let (label, active) = match row {
            1 => (" SESSIONS".to_string(), false),
            r if (r as usize) >= 2 && (r as usize) - 2 < items.len() => {
                let idx = (r as usize) - 2;
                let (title, active) = &items[idx];
                let mut label = format!(" {} {}", idx + 1, title);
                if label.chars().count() > text_w {
                    let tail: String = label
                        .chars()
                        .rev()
                        .take(text_w - 2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    label = format!(" \u{2026}{tail}");
                }
                (label, *active)
            }
            _ => (String::new(), false),
        };
        if row == 1 {
            out.extend_from_slice(b"\x1b[0;1;38;5;245;48;5;236m");
        } else if active {
            out.extend_from_slice(b"\x1b[0;7m");
        } else {
            out.extend_from_slice(b"\x1b[0;38;5;250;48;5;236m");
        }
        let mut printed = 0usize;
        for ch in label.chars().take(text_w) {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            printed += 1;
        }
        for _ in printed..text_w {
            out.push(b' ');
        }
        out.extend_from_slice("\x1b[0;38;5;240m\u{2502}".as_bytes());
    }
}

/// Rewrite SGR mouse coordinates for the sidebar offset. Returns the bytes
/// to forward to nvim plus any left-button presses that landed inside the
/// sidebar (row-indexed, for session switching).
fn translate_sgr_mouse(data: &[u8], sidebar_w: u16) -> (Vec<u8>, Vec<u32>) {
    let mut out = Vec::with_capacity(data.len());
    let mut clicks = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let looks_like_mouse =
            data[i] == 0x1b && i + 2 < data.len() && data[i + 1] == b'[' && data[i + 2] == b'<';
        if looks_like_mouse {
            if let Some(end) = data[i + 3..]
                .iter()
                .position(|&b| b == b'M' || b == b'm')
                .map(|off| i + 3 + off)
            {
                let body = &data[i + 3..end];
                let parts: Vec<u32> = body
                    .split(|&b| b == b';')
                    .filter_map(|f| std::str::from_utf8(f).ok()?.parse().ok())
                    .collect();
                if parts.len() == 3 {
                    let (btn, x, y) = (parts[0], parts[1], parts[2]);
                    if x <= sidebar_w as u32 {
                        if data[end] == b'M' && btn == 0 {
                            clicks.push(y);
                        }
                        // swallow sidebar-area mouse events entirely
                    } else {
                        out.extend_from_slice(
                            format!(
                                "\x1b[<{};{};{}{}",
                                btn,
                                x - sidebar_w as u32,
                                y,
                                data[end] as char
                            )
                            .as_bytes(),
                        );
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    (out, clicks)
}

/// Channel to the attached client. Control messages queue (they are tiny
/// and every one matters); output frames coalesce in a latest-wins slot —
/// each frame is a complete screen, so a slow terminal skips straight to
/// the newest state instead of replaying a growing backlog (which froze
/// slow embedded terminals for minutes).
struct ClientTx {
    control: std::sync::mpsc::SyncSender<String>,
    frame: Arc<Mutex<Option<String>>>,
}

struct Daemon {
    router: Arc<Router>,
    sessions: Mutex<Vec<Arc<Session>>>,
    active: AtomicU64,
    next_id: AtomicU64,
    client: Mutex<Option<ClientTx>>,
    /// Session sidebar visibility (toggled with the prefix key).
    sidebar: std::sync::atomic::AtomicBool,
    size: Mutex<PtySize>,
    router_socket: std::path::PathBuf,
    targets_file: Option<std::path::PathBuf>,
}

impl Daemon {
    fn send_client(&self, msg: &Value) {
        let mut guard = self.client.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            let mut line = msg.to_string();
            line.push('\n');
            if msg.get("t").and_then(Value::as_str) == Some("output") {
                *tx.frame.lock().unwrap() = Some(line);
                return;
            }
            use std::sync::mpsc::TrySendError;
            match tx.control.try_send(line) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    // Control overflow only happens if the client is gone
                    // or wedged beyond saving; writer exits with the rx.
                    *guard = None;
                }
            }
        }
    }

    /// Install `stream` as the attached client; returns when replaced.
    fn install_client(&self, stream: UnixStream) {
        let (control_tx, control_rx) = std::sync::mpsc::sync_channel::<String>(256);
        let frame: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        *self.client.lock().unwrap() = Some(ClientTx {
            control: control_tx,
            frame: Arc::clone(&frame),
        });
        thread::spawn(move || {
            use std::sync::mpsc::RecvTimeoutError;
            let mut stream = stream;
            loop {
                match control_rx.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(line) => {
                        if stream.write_all(line.as_bytes()).is_err() {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        let pending = frame.lock().unwrap().take();
                        if let Some(line) = pending {
                            if stream.write_all(line.as_bytes()).is_err() {
                                return;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
    }

    fn session(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    fn active_session(&self) -> Option<Arc<Session>> {
        self.session(self.active.load(Ordering::SeqCst))
    }

    fn sessions_msg(&self) -> Value {
        let active = self.active.load(Ordering::SeqCst);
        let items: Vec<Value> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "title": s.title.lock().unwrap().clone(),
                    "active": s.id == active,
                })
            })
            .collect();
        json!({ "t": "sessions", "items": items })
    }

    fn sidebar_visible(&self, total: PtySize) -> bool {
        self.sidebar.load(Ordering::SeqCst) && total.cols >= SIDEBAR_MIN_COLS
    }

    /// The size the nvim PTYs actually get (total minus sidebar).
    fn inner_size(&self, total: PtySize) -> PtySize {
        if self.sidebar_visible(total) {
            PtySize {
                cols: total.cols - SIDEBAR_WIDTH,
                ..total
            }
        } else {
            total
        }
    }

    fn sidebar_items(&self) -> Vec<(String, bool)> {
        let active = self.active.load(Ordering::SeqCst);
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.title.lock().unwrap().clone(), s.id == active))
            .collect()
    }

    /// Keep the picker candidate file in sync with live sessions.
    fn update_targets(&self) {
        if let Some(path) = &self.targets_file {
            let active = self.active.load(Ordering::SeqCst);
            let open: Vec<remotes::OpenSession> = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .map(|s| remotes::OpenSession {
                    id: s.id,
                    title: s.title.lock().unwrap().clone(),
                    active: s.id == active,
                })
                .collect();
            let _ = remotes::rewrite_targets_file(path, &open);
        }
    }

    /// Compose the client-facing frame: sidebar (when visible) + the
    /// session grid shifted right, cursor adjusted to match.
    fn compose_frame(&self, session: &Session) -> Vec<u8> {
        let total = *self.size.lock().unwrap();
        let screen = session.parser.lock().unwrap().screen().clone();
        let mut out: Vec<u8> = b"\x1b[?2026h\x1b[2J".to_vec();
        let offset = if self.sidebar_visible(total) {
            render_sidebar(&mut out, &self.sidebar_items(), total.rows);
            SIDEBAR_WIDTH
        } else {
            0
        };
        render_grid(&mut out, &screen, offset);
        finish_frame(&mut out, &screen, offset);
        out
    }

    fn status(&self, msg: &str) {
        eprintln!("[rnvim] {msg}");
        self.send_client(&json!({ "t": "status", "msg": msg }));
    }

    /// Paint the session's full screen to the client from the daemon's own
    /// virtual-screen state. Purely local — needs nothing from nvim.
    fn repaint(&self, session: &Session) {
        let frame = self.compose_frame(session);
        *session.last_frame.lock().unwrap() = Some(frame.clone());
        self.send_client(&json!({ "t": "output", "b64": B64.encode(&frame) }));
    }

    /// Guarantee this session's PTY and virtual screen match the client
    /// size, whatever path it took to get here (belt and braces: a session
    /// stuck at a stale size renders as a fraction of the terminal).
    fn apply_size(&self, session: &Session) {
        let size = *self.size.lock().unwrap();
        let mut applied = session.applied_size.lock().unwrap();
        if *applied != (size.rows, size.cols) {
            let _ = session.master.lock().unwrap().resize(size);
            session
                .parser
                .lock()
                .unwrap()
                .set_size(size.rows, size.cols);
            *session.last_frame.lock().unwrap() = None;
            *applied = (size.rows, size.cols);
        }
    }

    fn focus(&self, id: u64) {
        let Some(session) = self.session(id) else {
            return;
        };
        self.active.store(id, Ordering::SeqCst);
        self.send_client(&json!({
            "t": "switched",
            "id": id,
            "title": session.title.lock().unwrap().clone(),
        }));
        self.apply_size(&session);
        self.repaint(&session);
        self.update_targets();
    }

    fn focus_index(&self, idx: usize) {
        let id = self.sessions.lock().unwrap().get(idx).map(|s| s.id);
        if let Some(id) = id {
            self.focus(id);
        }
    }

    fn cycle(&self, dir: i64) {
        let sessions = self.sessions.lock().unwrap();
        if sessions.is_empty() {
            return;
        }
        let active = self.active.load(Ordering::SeqCst);
        let idx = sessions.iter().position(|s| s.id == active).unwrap_or(0) as i64;
        let next = (idx + dir).rem_euclid(sessions.len() as i64) as usize;
        let id = sessions[next].id;
        drop(sessions);
        self.focus(id);
    }

    /// Apply a new client size everywhere. Diff baselines are invalidated:
    /// contents_diff across two different grid sizes produces garbage
    /// positioning, so the next paint after any resize must be a full frame.
    fn resize_all(&self, size: PtySize) {
        *self.size.lock().unwrap() = size;
        let inner = self.inner_size(size);
        for session in self.sessions.lock().unwrap().iter() {
            let _ = session.master.lock().unwrap().resize(inner);
            session
                .parser
                .lock()
                .unwrap()
                .set_size(inner.rows, inner.cols);
            *session.last_frame.lock().unwrap() = None;
            *session.applied_size.lock().unwrap() = (inner.rows, inner.cols);
        }
    }

    fn remove_session(self: &Arc<Self>, id: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|s| s.id != id);
        let fallback = sessions.last().map(|s| s.id);
        let was_active = self.active.load(Ordering::SeqCst) == id;
        drop(sessions);
        if was_active {
            match fallback {
                Some(next) => self.focus(next),
                None => self.send_client(&json!({ "t": "empty" })),
            }
        }
        self.send_client(&self.sessions_msg());
        self.update_targets();
    }

    /// Spawn a new nvim instance. `target` None → plain scratch editor;
    /// host without path → pending-root (directory picker on startup).
    fn create_session(self: &Arc<Self>, target: Option<&str>) -> Result<(u64, String)> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let ws_base = nvim::rnvim_home()?.join("ws");

        let mut opts = LaunchOpts {
            socket: Some(self.router_socket.clone()),
            ws_base: Some(ws_base),
            ws_root: None,
            host: None,
            remote_entry: None,
            targets_file: self.targets_file.clone(),
            entry: None,
            pending_root: false,
            instance: Some(id),
            listen: None,
            headless_cmds: vec![],
        };

        if let Some(t) = target {
            self.status(&format!("connecting to {t}..."));
        }
        if !nvim::nvim_installed() {
            self.status("downloading Neovim (first run, ~10MB)...");
        }

        let title = match target {
            Some(t) => {
                let parsed = Target::parse(t);
                let pending = parsed.path.is_empty();
                let info = self.router.connect_target(t)?;
                opts.ws_root = Some(info.ws_root.clone().into());
                opts.host = Some(info.host.clone());
                opts.remote_entry = Some(info.abs.clone());
                if pending {
                    opts.pending_root = true;
                    format!("{} (choose directory)", info.host)
                } else {
                    opts.entry =
                        Some(format!("{}{}", info.ws_root, info.abs.trim_end_matches('/')).into());
                    format!("{}:{}", info.host, info.abs)
                }
            }
            None => "scratch".to_string(),
        };

        let nvim_socket = nvim::rnvim_home()?
            .join("run")
            .join(format!("inst-{id}.nvim"));
        let _ = std::fs::remove_file(&nvim_socket);
        opts.listen = Some(nvim_socket.clone());
        let plan = nvim::plan(&opts)?;
        let size = *self.size.lock().unwrap();
        let pair = native_pty_system().openpty(size).context("openpty")?;

        let mut cmd = CommandBuilder::new(&plan.bin);
        cmd.args(&plan.args);
        for (k, v) in &plan.envs {
            cmd.env(k, v);
        }
        if let Some(home) = std::env::var_os("HOME") {
            cmd.cwd(home);
        }
        let mut child = pair.slave.spawn_command(cmd).context("spawn nvim in pty")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;
        let killer = child.clone_killer();

        let session = Arc::new(Session {
            id,
            title: Mutex::new(title.clone()),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            parser: Mutex::new(vt100::Parser::new(size.rows, size.cols, 0)),
            last_frame: Mutex::new(None),
            applied_size: Mutex::new((size.rows, size.cols)),
        });
        self.sessions.lock().unwrap().push(Arc::clone(&session));

        // Drain the PTY forever (nvim blocks on a full pipe otherwise) into
        // the virtual screen; while active, paint the client with minimal
        // diffs of that screen state.
        let daemon = Arc::clone(self);
        let sess = Arc::clone(&session);
        let socket_to_clean = nvim_socket.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        sess.parser.lock().unwrap().process(&buf[..n]);
                        if daemon.active.load(Ordering::SeqCst) != id {
                            continue;
                        }
                        let frame = daemon.compose_frame(&sess);
                        let mut last = sess.last_frame.lock().unwrap();
                        if last.as_deref() == Some(frame.as_slice()) {
                            continue;
                        }
                        *last = Some(frame.clone());
                        drop(last);
                        daemon.send_client(&json!({
                            "t": "output",
                            "b64": B64.encode(&frame),
                        }));
                    }
                }
            }
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket_to_clean);
            daemon.remove_session(id);
        });

        self.send_client(&self.sessions_msg());
        self.update_targets();
        Ok((id, title))
    }

    fn handle_client_msg(self: &Arc<Self>, msg: &Value) -> Result<()> {
        let t = msg.get("t").and_then(Value::as_str).unwrap_or("");
        match t {
            "input" => {
                if let (Some(b64), Some(session)) = (
                    msg.get("b64").and_then(Value::as_str),
                    self.active_session(),
                ) {
                    let mut data = B64.decode(b64).unwrap_or_default();
                    let total = *self.size.lock().unwrap();
                    if self.sidebar_visible(total) {
                        // Shift mouse coordinates past the sidebar; clicks
                        // inside it switch sessions instead of reaching nvim.
                        let (fwd, clicks) = translate_sgr_mouse(&data, SIDEBAR_WIDTH);
                        data = fwd;
                        for y in clicks {
                            if y >= 2 {
                                self.focus_index((y - 2) as usize);
                            }
                        }
                    }
                    if !data.is_empty() {
                        let _ = session.writer.lock().unwrap().write_all(&data);
                    }
                }
            }
            "resize" => {
                let cols = msg.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
                let rows = msg.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                self.resize_all(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                // Full frame right away — the real terminal may hold stale
                // content outside the new grid, and nvim's own SIGWINCH
                // redraw may lag (or never come while it's idle).
                if let Some(session) = self.active_session() {
                    self.repaint(&session);
                }
            }
            "focus" => {
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    self.focus(id);
                }
            }
            "cycle" => {
                self.cycle(msg.get("dir").and_then(Value::as_i64).unwrap_or(1));
            }
            "list" => self.send_client(&self.sessions_msg()),
            "sidebar" => {
                self.sidebar.fetch_xor(true, Ordering::SeqCst);
                let total = *self.size.lock().unwrap();
                self.resize_all(total);
                if let Some(session) = self.active_session() {
                    self.repaint(&session);
                }
            }
            "focus_index" => {
                if let Some(i) = msg.get("i").and_then(Value::as_u64) {
                    if i >= 1 {
                        self.focus_index((i - 1) as usize);
                    }
                }
            }
            "redraw" => {
                if let Some(session) = self.active_session() {
                    self.repaint(&session);
                }
            }
            "kill" => {
                if let Some(session) = msg
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|id| self.session(id))
                {
                    let _ = session.killer.lock().unwrap().kill();
                }
            }
            "new" => {
                let target = msg
                    .get("target")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match self.create_session(target.as_deref()) {
                    Ok((id, _)) => self.focus(id),
                    Err(e) => self.send_client(&json!({ "t": "error", "msg": e.to_string() })),
                }
            }
            "detach" => bail!("detach"),
            other => self.send_client(&json!({ "t": "error", "msg": format!("unknown: {other}") })),
        }
        Ok(())
    }

    fn serve_client(self: &Arc<Self>, stream: UnixStream) -> Result<()> {
        let reader = stream.try_clone().context("clone client stream")?;
        self.install_client(stream);
        let mut lines = BufReader::new(reader).lines();

        // First message must be attach.
        let first = lines.next().context("client hung up")??;
        let attach: Value = serde_json::from_str(&first).context("attach message")?;
        if attach.get("t").and_then(Value::as_str) != Some("attach") {
            bail!("expected attach");
        }
        let cols = attach.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;
        let rows = attach.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
        self.resize_all(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });

        if let Some(target) = attach.get("target").and_then(Value::as_str) {
            match self.create_session(Some(target)) {
                Ok((id, _)) => self.focus(id),
                Err(e) => {
                    self.send_client(&json!({ "t": "error", "msg": e.to_string() }));
                    if self.sessions.lock().unwrap().is_empty() {
                        self.send_client(&json!({ "t": "empty" }));
                    }
                }
            }
        } else if self.sessions.lock().unwrap().is_empty() {
            match self.create_session(None) {
                Ok((id, _)) => self.focus(id),
                Err(e) => self.send_client(&json!({ "t": "error", "msg": e.to_string() })),
            }
        } else {
            let active = self.active.load(Ordering::SeqCst);
            self.focus(active);
        }

        for line in lines {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if self.handle_client_msg(&msg).is_err() {
                break; // detach
            }
        }
        *self.client.lock().unwrap() = None;
        Ok(())
    }
}

pub fn run_daemon() -> Result<()> {
    let run_dir = nvim::rnvim_home()?.join("run");
    std::fs::create_dir_all(&run_dir)?;

    let ctl_path = control_socket_path()?;
    if UnixStream::connect(&ctl_path).is_ok() {
        bail!("daemon already running on {}", ctl_path.display());
    }
    let _ = std::fs::remove_file(&ctl_path);

    let router_socket = run_dir.join("daemon-router.sock");
    let _ = std::fs::remove_file(&router_socket);
    let router_listener = UnixListener::bind(&router_socket)
        .with_context(|| format!("bind {}", router_socket.display()))?;

    let ws_base = nvim::rnvim_home()?.join("ws");
    let router = Router::new(ws_base);
    router.serve_listener(router_listener);

    let daemon = Arc::new(Daemon {
        router: Arc::clone(&router),
        sessions: Mutex::new(Vec::new()),
        active: AtomicU64::new(0),
        next_id: AtomicU64::new(1),
        client: Mutex::new(None),
        sidebar: std::sync::atomic::AtomicBool::new(true),
        size: Mutex::new(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }),
        router_socket,
        targets_file: remotes::write_targets_file().ok(),
    });

    // :RnvimConnect inside any instance → a new instance, focused.
    {
        let daemon = Arc::clone(&daemon);
        router.set_connect_hook(Box::new(move |target| {
            let (id, title) = daemon.create_session(Some(target))?;
            daemon.focus(id);
            Ok(json!({ "instance": id, "title": title }))
        }));
    }
    // session.rooted → retitle that instance after directory selection.
    {
        let daemon = Arc::clone(&daemon);
        router.set_rooted_hook(Box::new(move |params| {
            let instance = params.get("instance").and_then(Value::as_u64);
            let host = params.get("host").and_then(Value::as_str).unwrap_or("?");
            let path = params.get("path").and_then(Value::as_str).unwrap_or("?");
            if let Some(session) = instance.and_then(|id| daemon.session(id)) {
                *session.title.lock().unwrap() = format!("{host}:{path}");
                daemon.send_client(&daemon.sessions_msg());
                daemon.update_targets();
                if let Some(active) = daemon.active_session() {
                    daemon.repaint(&active);
                }
            }
        }));
    }

    // Picker "open sessions" entries switch instances via session.focus.
    {
        let daemon = Arc::clone(&daemon);
        router.set_focus_hook(Box::new(move |id| {
            daemon.focus(id);
        }));
    }

    let listener =
        UnixListener::bind(&ctl_path).with_context(|| format!("bind {}", ctl_path.display()))?;
    eprintln!("[rnvim] daemon listening on {}", ctl_path.display());

    for stream in listener.incoming().flatten() {
        // One *active* client at a time — a new attach takes over the
        // output stream — but each connection gets its own thread so a
        // half-dead old client can never block a new one.
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || {
            if let Err(e) = daemon.serve_client(stream) {
                eprintln!("[rnvim] client session ended: {e}");
            }
        });
    }
    Err(anyhow!("control listener closed"))
}

#[cfg(test)]
mod tests {
    use super::full_frame;

    /// The frame must not depend on BCE (erase-with-background): weak
    /// embedded terminals render EL'd regions with the DEFAULT background,
    /// so every blank cell has to be painted explicitly.
    #[test]
    fn full_frame_paints_blanks_explicitly() {
        let mut parser = vt100::Parser::new(6, 12, 0);
        // nvim-style clear: truecolor background + erase display + a word
        parser.process(b"\x1b[48;2;40;44;52m\x1b[2J\x1b[HHi");
        let frame = full_frame(parser.screen());
        let text = String::from_utf8_lossy(&frame);

        assert!(!text.contains("\x1b[K"), "must not rely on EL/BCE");
        assert!(
            !text.contains("\x1b[J\x1b"),
            "must not rely on ED for content"
        );
        assert!(text.contains("48;2;40;44;52"), "background color present");
        // 6 rows * 12 cols, every cell written (2 chars are 'H','i', rest blanks)
        let spaces = frame.iter().filter(|&&b| b == b' ').count();
        assert_eq!(spaces, 6 * 12 - 2, "every blank cell painted as a space");
        assert!(
            text.starts_with("\x1b[?2026h\x1b[2J"),
            "sync + defensive clear"
        );
        assert!(text.ends_with("\x1b[?2026l"), "sync end");
    }

    #[test]
    fn full_frame_positions_cursor() {
        let mut parser = vt100::Parser::new(4, 10, 0);
        parser.process(b"abc");
        let frame = full_frame(parser.screen());
        let text = String::from_utf8_lossy(&frame);
        assert!(text.contains("\x1b[1;4H"), "cursor after 'abc': {text:?}");
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::{render_sidebar, translate_sgr_mouse, SIDEBAR_WIDTH};

    #[test]
    fn sidebar_renders_titles_and_active_marker() {
        let items = vec![
            ("home:/proj/a".to_string(), true),
            ("local scratch".to_string(), false),
        ];
        let mut out = Vec::new();
        render_sidebar(&mut out, &items, 10);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("SESSIONS"));
        assert!(text.contains("home:/proj/a"));
        assert!(text.contains("local scratch"));
        assert!(text.contains("\x1b[0;7m"), "active entry inverted");
        assert!(!text.contains("\x1b[K"), "weakest-terminal rule holds");
    }

    #[test]
    fn mouse_coords_shift_and_sidebar_clicks_are_captured() {
        // click at x=30 (inside nvim area) shifts left by the sidebar width
        let (fwd, clicks) = translate_sgr_mouse(b"\x1b[<0;30;5M", SIDEBAR_WIDTH);
        assert_eq!(fwd, format!("\x1b[<0;{};5M", 30 - SIDEBAR_WIDTH).as_bytes());
        assert!(clicks.is_empty());

        // press on sidebar row 4 (session index 2) is swallowed + reported
        let (fwd, clicks) = translate_sgr_mouse(b"\x1b[<0;3;4M", SIDEBAR_WIDTH);
        assert!(fwd.is_empty());
        assert_eq!(clicks, vec![4]);

        // release in the sidebar is swallowed but not a click
        let (fwd, clicks) = translate_sgr_mouse(b"\x1b[<0;3;4m", SIDEBAR_WIDTH);
        assert!(fwd.is_empty());
        assert!(clicks.is_empty());

        // plain keys pass through untouched
        let (fwd, clicks) = translate_sgr_mouse(b"hello", SIDEBAR_WIDTH);
        assert_eq!(fwd, b"hello");
        assert!(clicks.is_empty());
    }
}
