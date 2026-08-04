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
    host: String,
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
    /// Broker-internal waiters, tagged with the host they're waiting on so
    /// they can be failed fast when that agent's connection dies.
    pending: Mutex<HashMap<u64, (String, mpsc::Sender<Response>)>>,
    next_internal_id: AtomicU64,
    conns: Mutex<HashMap<u64, UnixStream>>,
    next_conn_id: AtomicU64,
    fwd: Mutex<HashMap<u64, (u64, u64, String)>>, // fwd_id → (conn_id, original id, host)
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

    /// Drop a dead agent connection so the next use of its host redials
    /// (deploy + ssh + handshake) instead of failing forever on a stale
    /// pipe. Everything still waiting on that agent is failed immediately —
    /// broker-internal waiters get an error response, forwarded nvim
    /// requests get an error back on their own connection.
    fn evict_agent(&self, agent: &Arc<Agent>) {
        let lost = |id| {
            Response::err(
                id,
                ERR_IO,
                format!("agent connection to {} lost", agent.host),
            )
        };
        {
            let mut agents = self.agents.lock().unwrap();
            // only evict ourselves — a fresh redial may already be installed
            if agents
                .get(&agent.host)
                .is_some_and(|cur| Arc::ptr_eq(cur, agent))
            {
                agents.remove(&agent.host);
            }
        }
        let waiters: Vec<(u64, mpsc::Sender<Response>)> = {
            let mut pending = self.pending.lock().unwrap();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, (host, _))| *host == agent.host)
                .map(|(&id, _)| id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id).map(|(_, tx)| (id, tx)))
                .collect()
        };
        for (id, tx) in waiters {
            let _ = tx.send(lost(id));
        }
        let forwarded: Vec<(u64, u64)> = {
            let mut fwd = self.fwd.lock().unwrap();
            let ids: Vec<u64> = fwd
                .iter()
                .filter(|(_, (_, _, host))| *host == agent.host)
                .map(|(&id, _)| id)
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    fwd.remove(&id)
                        .map(|(conn_id, orig_id, _)| (conn_id, orig_id))
                })
                .collect()
        };
        for (conn_id, orig_id) in forwarded {
            if let Ok(line) = serde_json::to_string(&lost(orig_id)) {
                self.write_conn(conn_id, &line);
            }
        }
    }

    /// One reader per agent: broker-internal replies go to their waiters,
    /// forwarded-id replies are rewritten and returned to the right nvim.
    /// EOF means the agent (or its ssh carrier) died — evict it so the
    /// host can reconnect.
    fn spawn_agent_reader(self: &Arc<Self>, agent: Arc<Agent>, stdout: BufReader<ChildStdout>) {
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
                            if let Some((_, tx)) = router.pending.lock().unwrap().remove(&id) {
                                if let Ok(resp) = serde_json::from_str::<Response>(trimmed) {
                                    let _ = tx.send(resp);
                                }
                            }
                            continue;
                        }
                        if id >= FWD_ID_BASE {
                            if let Some((conn_id, orig_id, _)) =
                                router.fwd.lock().unwrap().remove(&id)
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
            router.evict_agent(&agent);
        });
    }

    /// Blocking request from the broker itself to an agent.
    fn agent_request(&self, agent: &Arc<Agent>, method: &str, params: Value) -> Result<Value> {
        self.agent_request_timeout(agent, method, params, Duration::from_secs(120))
    }

    fn agent_request_timeout(
        &self,
        agent: &Arc<Agent>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_internal_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id, (agent.host.clone(), tx));
        let written = {
            let mut stdin = agent.stdin.lock().unwrap();
            let mut line = serde_json::to_string(&Request {
                id,
                method: method.into(),
                params,
            })?;
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush())
        };
        if let Err(e) = written {
            // dead pipe: drop the cached connection so the next attempt
            // redials instead of hitting the same corpse forever
            self.pending.lock().unwrap().remove(&id);
            self.evict_agent(agent);
            return Err(anyhow!(e).context(format!(
                "agent connection to {} lost (will redial)",
                agent.host
            )));
        }
        let resp = rx.recv_timeout(timeout).map_err(|_| {
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
            host: target.host.clone(),
            stdin: Mutex::new(conn.stdin),
        });
        self.spawn_agent_reader(Arc::clone(&agent), conn.stdout);
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

    /// Install a tool on `host`. Downloads happen on the remote through
    /// the agent's native HTTP client (fetch.url — no curl involved), then
    /// a local unpack script links the binary. npm/golang plans use the
    /// remote package manager, which follows the remote's own mirrors.
    fn install_on(self: &Arc<Self>, host: &str, name: &str) -> Result<Value> {
        let agent = self.ensure_agent(&Target::parse(host))?;
        let uname = self.agent_request(&agent, "exec.run", json!({ "script": "uname -sm" }))?;
        let uname_sm = uname
            .get("stdout")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("could not detect remote platform")?
            .to_string();

        let script = match crate::registry::plan_for(name, &uname_sm)? {
            crate::registry::InstallPlan::Remote { script } => script,
            crate::registry::InstallPlan::Staged { url, file, script } => {
                self.agent_request_timeout(
                    &agent,
                    "fetch.url",
                    json!({ "url": url, "path": format!("~/.rnvim/stage/{file}") }),
                    Duration::from_secs(20 * 60),
                )
                .with_context(|| format!("remote download of {url}"))?;
                script
            }
        };

        let out = self.agent_request_timeout(
            &agent,
            "exec.run",
            json!({ "script": script }),
            Duration::from_secs(20 * 60),
        )?;
        let code = out.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            let stderr = out.get("stderr").and_then(Value::as_str).unwrap_or("");
            bail!(
                "install script failed: {}",
                stderr.lines().last().unwrap_or("unknown error").trim()
            );
        }
        let path = out
            .get("stdout")
            .and_then(Value::as_str)
            .and_then(|s| s.lines().rev().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("?")
            .to_string();
        Ok(json!({ "path": path }))
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
                "session.install" => match (param("host"), param("name")) {
                    (Some(h), Some(n)) => router.install_on(&h, &n),
                    _ => Err(anyhow!("missing host/name")),
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
                    self.fwd
                        .lock()
                        .unwrap()
                        .insert(fwd_id, (conn_id, id, host.clone()));
                    msg["id"] = json!(fwd_id);
                    let written = serde_json::to_string(&msg)
                        .map_err(std::io::Error::other)
                        .and_then(|out| {
                            let mut stdin = agent.stdin.lock().unwrap();
                            stdin
                                .write_all(out.as_bytes())
                                .and_then(|()| stdin.write_all(b"\n"))
                                .and_then(|()| stdin.flush())
                        });
                    if written.is_err() {
                        // evicting fails this fwd entry back to nvim too
                        self.evict_agent(&agent);
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
        self.fwd
            .lock()
            .unwrap()
            .retain(|_, (c, _, _)| *c != conn_id);
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

#[cfg(test)]
mod eviction_tests {
    use super::*;

    /// An Agent whose stdin pipe is already dead (child exited).
    fn dead_agent(host: &str) -> Arc<Agent> {
        let mut child = std::process::Command::new("true")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn true");
        let stdin = child.stdin.take().unwrap();
        child.wait().unwrap();
        Arc::new(Agent {
            host: host.to_string(),
            stdin: Mutex::new(stdin),
        })
    }

    #[test]
    fn dead_pipe_evicts_agent_and_fails_waiters() {
        let router = Router::new(std::env::temp_dir().join("rnvim-evict-test"));
        let agent = dead_agent("deadhost");
        router
            .agents
            .lock()
            .unwrap()
            .insert("deadhost".into(), Arc::clone(&agent));

        // a broker-internal waiter already in flight on that host
        let (tx, rx) = mpsc::channel();
        router
            .pending
            .lock()
            .unwrap()
            .insert(INTERNAL_ID_BASE + 7, ("deadhost".into(), tx));

        let err = router
            .agent_request_timeout(&agent, "fs.stat", json!({}), Duration::from_secs(1))
            .unwrap_err();
        assert!(err.to_string().contains("lost"), "err: {err:#}");

        // cache no longer holds the corpse → next ensure_agent redials
        assert!(!router.agents.lock().unwrap().contains_key("deadhost"));

        // the in-flight waiter was failed immediately, not left to time out
        let resp = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter got response");
        assert!(resp.error.is_some());
    }

    #[test]
    fn eviction_spares_a_fresh_replacement() {
        let router = Router::new(std::env::temp_dir().join("rnvim-evict-test2"));
        let old = dead_agent("host");
        let fresh = dead_agent("host");
        router
            .agents
            .lock()
            .unwrap()
            .insert("host".into(), Arc::clone(&fresh));

        // evicting the OLD agent must not remove the freshly redialed one
        router.evict_agent(&old);
        assert!(router.agents.lock().unwrap().contains_key("host"));
    }
}
