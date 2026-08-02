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
    /// terminal emulator, and clients are painted purely from its state
    /// (full frame on switch/attach, minimal diffs while streaming). This
    /// is what makes detach/switch rendering deterministic — no reliance
    /// on nvim redrawing, no escape sequences cut mid-stream.
    parser: Mutex<vt100::Parser>,
    /// Screen state as last painted to the client, if any.
    last_sent: Mutex<Option<vt100::Screen>>,
}

struct Daemon {
    router: Arc<Router>,
    sessions: Mutex<Vec<Arc<Session>>>,
    active: AtomicU64,
    next_id: AtomicU64,
    /// Bounded queue to the attached client's writer thread. A slow or
    /// stalled client must never block a PTY reader (that would freeze
    /// nvim); on overflow the client is dropped instead — it can reattach.
    client: Mutex<Option<std::sync::mpsc::SyncSender<String>>>,
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
            use std::sync::mpsc::TrySendError;
            match tx.try_send(line) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    // Writer thread exits when its receiver is dropped.
                    *guard = None;
                }
            }
        }
    }

    /// Install `stream` as the attached client; returns when replaced.
    fn install_client(&self, stream: UnixStream) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(512);
        *self.client.lock().unwrap() = Some(tx);
        thread::spawn(move || {
            let mut stream = stream;
            for line in rx {
                if stream.write_all(line.as_bytes()).is_err() {
                    break;
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

    /// Paint the session's full screen to the client from the daemon's own
    /// virtual-screen state. Purely local — needs nothing from nvim.
    fn repaint(&self, session: &Session) {
        let screen = session.parser.lock().unwrap().screen().clone();
        let mut bytes = b"\x1b[2J\x1b[H".to_vec();
        bytes.extend_from_slice(&screen.contents_formatted());
        *session.last_sent.lock().unwrap() = Some(screen);
        self.send_client(&json!({ "t": "output", "b64": B64.encode(&bytes) }));
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
        self.repaint(&session);
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
        for session in self.sessions.lock().unwrap().iter() {
            let _ = session.master.lock().unwrap().resize(size);
            session
                .parser
                .lock()
                .unwrap()
                .set_size(size.rows, size.cols);
            *session.last_sent.lock().unwrap() = None;
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
            last_sent: Mutex::new(None),
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
                        let screen = {
                            let mut parser = sess.parser.lock().unwrap();
                            parser.process(&buf[..n]);
                            parser.screen().clone()
                        };
                        if daemon.active.load(Ordering::SeqCst) != id {
                            continue;
                        }
                        let mut last = sess.last_sent.lock().unwrap();
                        let bytes = match last.as_ref() {
                            Some(prev) => screen.contents_diff(prev),
                            None => {
                                let mut b = b"\x1b[2J\x1b[H".to_vec();
                                b.extend_from_slice(&screen.contents_formatted());
                                b
                            }
                        };
                        *last = Some(screen);
                        drop(last);
                        if !bytes.is_empty() {
                            daemon.send_client(&json!({
                                "t": "output",
                                "b64": B64.encode(&bytes),
                            }));
                        }
                    }
                }
            }
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket_to_clean);
            daemon.remove_session(id);
        });

        self.send_client(&self.sessions_msg());
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
                    let data = B64.decode(b64).unwrap_or_default();
                    let _ = session.writer.lock().unwrap().write_all(&data);
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
            }
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
