//! Session runtime: a unix-socket broker routing any number of nvim
//! instances onto any number of remote agents.
//!
//! Message flow (JSON lines):
//!   nvim → broker: {id, host, method, params}
//!     - `session.*` is handled by the broker itself (connect/browse/rooted)
//!     - everything else routes to the agent for `host`, with the id
//!       remapped so responses find their way back to the right nvim
//!   agent → broker → nvim: forwarded-id responses are rewritten back and
//!     sent to the owning connection; replies to the broker's own control
//!     requests (id >= INTERNAL_ID_BASE) are consumed internally.
//!
//! The daemon reuses this router across many nvim instances and overrides
//! `session.connect` (new instance instead of in-editor workspace) via hooks.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rnvim_proto::*;
use serde_json::{json, Value};

use crate::deploy;
use crate::nvim::{self, LaunchOpts};
use crate::remotes;
use crate::target::Target;
use crate::transport::AgentConn;

/// Lua request ids count up from 1. Forwarded requests are remapped into
/// [FWD_ID_BASE, INTERNAL_ID_BASE); the broker's own requests live above.
const INTERNAL_ID_BASE: u64 = 1 << 62;
const FWD_ID_BASE: u64 = 1 << 61;

struct Agent {
    stdin: Mutex<ChildStdin>,
}

pub struct WorkspaceInfo {
    pub host: String,
    pub slug: String,
    pub ws_root: String,
    pub abs: String,
    pub kind: String,
}

type ConnectHook = Box<dyn Fn(&str) -> Result<Value> + Send + Sync>;
type RootedHook = Box<dyn Fn(&Value) + Send + Sync>;
type FocusHook = Box<dyn Fn(u64) + Send + Sync>;

pub struct Router {
    ws_base: PathBuf,
    agents: Mutex<HashMap<String, Arc<Agent>>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Response>>>,
    next_internal_id: AtomicU64,
    conns: Mutex<HashMap<u64, UnixStream>>,
    next_conn_id: AtomicU64,
    fwd: Mutex<HashMap<u64, (u64, u64)>>, // fwd_id → (conn_id, original id)
    next_fwd_id: AtomicU64,
    /// Daemon override for session.connect (open a new instance). Without
    /// it, connect answers with workspace info for the in-editor flow.
    connect_hook: Mutex<Option<ConnectHook>>,
    /// Extra observer for session.rooted (the daemon retitles the session).
    rooted_hook: Mutex<Option<RootedHook>>,
    /// Daemon override for session.focus (switch to an open instance).
    focus_hook: Mutex<Option<FocusHook>>,
}

impl Router {
    pub fn new(ws_base: PathBuf) -> Arc<Router> {
        Arc::new(Router {
            ws_base,
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_internal_id: AtomicU64::new(INTERNAL_ID_BASE),
            conns: Mutex::new(HashMap::new()),
            next_conn_id: AtomicU64::new(1),
            fwd: Mutex::new(HashMap::new()),
            next_fwd_id: AtomicU64::new(FWD_ID_BASE),
            connect_hook: Mutex::new(None),
            rooted_hook: Mutex::new(None),
            focus_hook: Mutex::new(None),
        })
    }

    pub fn set_connect_hook(&self, hook: ConnectHook) {
        *self.connect_hook.lock().unwrap() = Some(hook);
    }

    pub fn set_rooted_hook(&self, hook: RootedHook) {
        *self.rooted_hook.lock().unwrap() = Some(hook);
    }

    pub fn set_focus_hook(&self, hook: FocusHook) {
        *self.focus_hook.lock().unwrap() = Some(hook);
    }

    fn write_conn(&self, conn_id: u64, line: &str) {
        let mut conns = self.conns.lock().unwrap();
        if let Some(stream) = conns.get_mut(&conn_id) {
            let _ = stream.write_all(line.as_bytes());
            let _ = stream.write_all(b"\n");
        }
    }

    /// One reader per agent: broker-internal replies go to their waiters,
    /// forwarded-id replies are rewritten and returned to the right nvim.
    fn spawn_agent_reader(self: &Arc<Self>, stdout: BufReader<ChildStdout>) {
        let router = Arc::clone(self);
        thread::spawn(move || {
            let mut stdout = stdout;
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(mut msg) = serde_json::from_str::<Value>(trimmed) else {
                            continue;
                        };
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        if id >= INTERNAL_ID_BASE {
                            if let Some(tx) = router.pending.lock().unwrap().remove(&id) {
                                if let Ok(resp) = serde_json::from_str::<Response>(trimmed) {
                                    let _ = tx.send(resp);
                                }
                            }
                            continue;
                        }
                        if id >= FWD_ID_BASE {
                            if let Some((conn_id, orig_id)) = router.fwd.lock().unwrap().remove(&id)
                            {
                                msg["id"] = json!(orig_id);
                                if let Ok(out) = serde_json::to_string(&msg) {
                                    router.write_conn(conn_id, &out);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    /// Blocking request from the broker itself to an agent.
    fn agent_request(&self, agent: &Agent, method: &str, params: Value) -> Result<Value> {
        let id = self.next_internal_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        {
            let mut stdin = agent.stdin.lock().unwrap();
            let mut line = serde_json::to_string(&Request {
                id,
                method: method.into(),
                params,
            })?;
            line.push('\n');
            stdin.write_all(line.as_bytes()).context("write to agent")?;
            stdin.flush()?;
        }
        let resp = rx.recv_timeout(Duration::from_secs(120)).map_err(|_| {
            self.pending.lock().unwrap().remove(&id);
            anyhow!("{method} timed out (agent unresponsive)")
        })?;
        if let Some(err) = resp.error {
            bail!("{method}: {}", err.message);
        }
        resp.result
            .with_context(|| format!("{method}: missing result"))
    }

    /// Get (or create, deploying if needed) the agent for a target's host.
    fn ensure_agent(self: &Arc<Self>, target: &Target) -> Result<Arc<Agent>> {
        if let Some(agent) = self.agents.lock().unwrap().get(&target.host) {
            return Ok(Arc::clone(agent));
        }
        let conn = if target.is_local() {
            AgentConn::spawn_local()?
        } else {
            let remote_cmd = deploy::ensure_remote_agent(&target.host)?;
            AgentConn::spawn_ssh(&target.host, &remote_cmd)?
        };
        let agent = Arc::new(Agent {
            stdin: Mutex::new(conn.stdin),
        });
        self.spawn_agent_reader(conn.stdout);
        // conn.child intentionally dropped unkilled: the agent exits on its
        // own when this process (and thus its stdin pipe) goes away.

        self.agent_request(
            &agent,
            "hello",
            json!(HelloParams {
                client_version: env!("CARGO_PKG_VERSION").into(),
                proto: PROTO_VERSION,
            }),
        )
        .with_context(|| format!("agent handshake with {}", target.host))?;

        self.agents
            .lock()
            .unwrap()
            .insert(target.host.clone(), Arc::clone(&agent));
        Ok(agent)
    }

    /// Open a workspace: connect/reuse the host's agent, resolve the path.
    pub fn connect_target(self: &Arc<Self>, target_str: &str) -> Result<WorkspaceInfo> {
        let target = Target::parse(target_str);
        let agent = self.ensure_agent(&target)?;
        let resolved: ResolveResult = serde_json::from_value(self.agent_request(
            &agent,
            "fs.resolve",
            json!(ResolveParams {
                path: target.path.clone()
            }),
        )?)?;
        if !target.path.is_empty() {
            remotes::record_recent(&target.host, &resolved.abs);
        }
        let slug = target.host_slug();
        let ws_root = self.ws_base.join(&slug);
        Ok(WorkspaceInfo {
            host: target.host,
            slug,
            ws_root: ws_root.to_string_lossy().into_owned(),
            abs: resolved.abs,
            kind: resolved.kind,
        })
    }

    /// Directory listing on a (possibly not-yet-connected) host, for the
    /// connect flow's directory-selection stage.
    fn browse(self: &Arc<Self>, host: &str, path: &str) -> Result<Value> {
        let target = Target::parse(host);
        let agent = self.ensure_agent(&target)?;
        let resolved: ResolveResult = serde_json::from_value(self.agent_request(
            &agent,
            "fs.resolve",
            json!(ResolveParams {
                path: path.to_string()
            }),
        )?)?;
        let listing = self.agent_request(
            &agent,
            "fs.list",
            json!(ListParams {
                path: resolved.abs.clone()
            }),
        )?;
        Ok(json!({ "abs": resolved.abs, "entries": listing.get("entries") }))
    }

    fn handle_session_method(
        self: &Arc<Self>,
        conn_id: u64,
        id: u64,
        method: &str,
        params: &Value,
    ) {
        let router = Arc::clone(self);
        let method = method.to_string();
        let params = params.clone();
        // Deploy + handshake can take a while on a fresh host; never block
        // the routing loop on it.
        thread::spawn(move || {
            let param = |k: &str| params.get(k).and_then(Value::as_str).map(str::to_string);
            let result = match method.as_str() {
                "session.connect" => param("target").context("missing target").and_then(|t| {
                    let hook = router.connect_hook.lock().unwrap();
                    if let Some(hook) = hook.as_ref() {
                        hook(&t)
                    } else {
                        router.connect_target(&t).map(|info| {
                            json!({
                                "host": info.host,
                                "slug": info.slug,
                                "ws_root": info.ws_root,
                                "abs": info.abs,
                                "kind": info.kind,
                            })
                        })
                    }
                }),
                "session.browse" => match (param("host"), param("path")) {
                    (Some(h), Some(p)) => router.browse(&h, &p),
                    _ => Err(anyhow!("missing host/path")),
                },
                "session.focus" => match params.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        if let Some(hook) = router.focus_hook.lock().unwrap().as_ref() {
                            hook(id);
                            Ok(json!({ "ok": true }))
                        } else {
                            Err(anyhow!("session.focus unavailable outside the daemon"))
                        }
                    }
                    None => Err(anyhow!("missing id")),
                },
                "session.rooted" => match (param("host"), param("path")) {
                    (Some(h), Some(p)) => {
                        remotes::record_recent(&h, &p);
                        if let Some(hook) = router.rooted_hook.lock().unwrap().as_ref() {
                            hook(&params);
                        }
                        Ok(json!({ "ok": true }))
                    }
                    _ => Err(anyhow!("missing host/path")),
                },
                other => Err(anyhow!("unknown session method: {other}")),
            };
            let resp = match result {
                Ok(v) => Response::ok(id, v),
                Err(e) => Response::err(id, ERR_IO, e.to_string()),
            };
            if let Ok(line) = serde_json::to_string(&resp) {
                router.write_conn(conn_id, &line);
            }
        });
    }

    /// Serve one nvim connection until it closes.
    pub fn serve_nvim(self: Arc<Self>, stream: UnixStream) {
        let reader = match stream.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
        self.conns.lock().unwrap().insert(conn_id, stream);

        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(mut msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
            let method = msg
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if let Some(rest) = method.strip_prefix("session.") {
                let _ = rest;
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                self.handle_session_method(conn_id, id, &method, &params);
                continue;
            }

            let host = msg
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let agent = self.agents.lock().unwrap().get(&host).cloned();
            match agent {
                Some(agent) => {
                    let fwd_id = self.next_fwd_id.fetch_add(1, Ordering::SeqCst);
                    self.fwd.lock().unwrap().insert(fwd_id, (conn_id, id));
                    msg["id"] = json!(fwd_id);
                    if let Ok(out) = serde_json::to_string(&msg) {
                        let mut stdin = agent.stdin.lock().unwrap();
                        let _ = stdin.write_all(out.as_bytes());
                        let _ = stdin.write_all(b"\n");
                        let _ = stdin.flush();
                    }
                }
                None => {
                    let resp =
                        Response::err(id, ERR_IO, format!("no connected workspace for {host:?}"));
                    if let Ok(line) = serde_json::to_string(&resp) {
                        self.write_conn(conn_id, &line);
                    }
                }
            }
        }

        self.conns.lock().unwrap().remove(&conn_id);
        self.fwd.lock().unwrap().retain(|_, (c, _)| *c != conn_id);
    }

    /// Accept nvim connections forever (each served on its own thread).
    pub fn serve_listener(self: &Arc<Self>, listener: UnixListener) {
        let router = Arc::clone(self);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let router = Arc::clone(&router);
                thread::spawn(move || router.serve_nvim(stream));
            }
        });
    }
}

/// Legacy direct mode (used for --headless-cmd runs and tests): one nvim in
/// the foreground of this process, broker in-process, no daemon.
pub fn run(initial_target: Option<&str>, headless_cmds: &[String]) -> Result<i32> {
    let ws_base = nvim::rnvim_home()?.join("ws");
    let router = Router::new(ws_base.clone());
    let targets_file = remotes::write_targets_file().ok();

    let mut opts = LaunchOpts {
        socket: None,
        ws_base: Some(ws_base),
        ws_root: None,
        host: None,
        remote_entry: None,
        targets_file: targets_file.clone(),
        entry: None,
        pending_root: false,
        instance: None,
        listen: None,
        headless_cmds: headless_cmds.to_vec(),
    };

    if let Some(target) = initial_target {
        let info = router.connect_target(target)?;
        opts.entry = Some(PathBuf::from(format!(
            "{}{}",
            info.ws_root,
            info.abs.trim_end_matches('/')
        )));
        opts.ws_root = Some(PathBuf::from(&info.ws_root));
        opts.host = Some(info.host.clone());
        opts.remote_entry = Some(info.abs.clone());
    }

    let run_dir = nvim::rnvim_home()?.join("run");
    std::fs::create_dir_all(&run_dir)?;
    let socket_path = run_dir.join(format!("session-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    opts.socket = Some(socket_path.clone());

    router.serve_listener(listener);

    let code = nvim::launch(opts)?;

    let _ = std::fs::remove_file(&socket_path);
    if let Some(t) = targets_file {
        let _ = std::fs::remove_file(t);
    }
    Ok(code)
}
