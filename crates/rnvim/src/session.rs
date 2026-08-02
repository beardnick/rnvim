//! Session runtime: a unix-socket broker routing one nvim instance onto any
//! number of remote agents (multi-workspace).
//!
//! Message flow (JSON lines):
//!   nvim → broker: {id, host, method, params}
//!     - `session.connect` is handled by the broker itself (deploy agent,
//!       handshake, resolve path) — never forwarded
//!     - everything else routes to the agent for `host`
//!   agent → broker → nvim: responses forwarded verbatim; replies to the
//!     broker's own control requests (id >= INTERNAL_ID_BASE) are consumed
//!     internally instead.

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

/// Lua request ids count up from 1; broker-internal ids live far above.
const INTERNAL_ID_BASE: u64 = 1 << 62;

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

struct Router {
    ws_base: PathBuf,
    agents: Mutex<HashMap<String, Arc<Agent>>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Response>>>,
    next_internal_id: AtomicU64,
    nvim: Mutex<Option<UnixStream>>,
}

impl Router {
    fn new(ws_base: PathBuf) -> Arc<Router> {
        Arc::new(Router {
            ws_base,
            agents: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            next_internal_id: AtomicU64::new(INTERNAL_ID_BASE),
            nvim: Mutex::new(None),
        })
    }

    fn write_nvim(&self, line: &str) {
        let mut guard = self.nvim.lock().unwrap();
        if let Some(stream) = guard.as_mut() {
            let _ = stream.write_all(line.as_bytes());
            let _ = stream.write_all(b"\n");
        }
    }

    /// One reader per agent: broker-internal replies are matched to their
    /// waiters, everything else is forwarded to nvim.
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
                        let id = serde_json::from_str::<Value>(trimmed)
                            .ok()
                            .and_then(|v| v.get("id").and_then(Value::as_u64));
                        if let Some(id) = id {
                            if id >= INTERNAL_ID_BASE {
                                if let Some(tx) = router.pending.lock().unwrap().remove(&id) {
                                    if let Ok(resp) = serde_json::from_str::<Response>(trimmed) {
                                        let _ = tx.send(resp);
                                    }
                                    continue;
                                }
                            }
                        }
                        router.write_nvim(trimmed);
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
        remotes::record_recent(&target.host, &resolved.abs);
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

    /// Serve the nvim connection: session.* handled here, the rest routed.
    fn serve_nvim(self: Arc<Self>, stream: UnixStream) {
        let reader = match stream.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        *self.nvim.lock().unwrap() = Some(stream);

        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = msg.get("id").and_then(Value::as_u64).unwrap_or(0);
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

            if method == "session.connect" {
                let target = msg
                    .get("params")
                    .and_then(|p| p.get("target"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let router = Arc::clone(&self);
                // Deploy + handshake can take a while on a fresh host; never
                // block the routing loop on it.
                thread::spawn(move || {
                    let resp = match target
                        .context("session.connect: missing target")
                        .and_then(|t| router.connect_target(&t))
                    {
                        Ok(info) => Response::ok(
                            id,
                            json!({
                                "host": info.host,
                                "slug": info.slug,
                                "ws_root": info.ws_root,
                                "abs": info.abs,
                                "kind": info.kind,
                            }),
                        ),
                        Err(e) => Response::err(id, ERR_IO, e.to_string()),
                    };
                    if let Ok(line) = serde_json::to_string(&resp) {
                        router.write_nvim(&line);
                    }
                });
                continue;
            }

            let host = msg.get("host").and_then(Value::as_str).unwrap_or("");
            let agent = self.agents.lock().unwrap().get(host).cloned();
            match agent {
                Some(agent) => {
                    let mut stdin = agent.stdin.lock().unwrap();
                    let _ = stdin.write_all(line.as_bytes());
                    let _ = stdin.write_all(b"\n");
                    let _ = stdin.flush();
                }
                None => {
                    let resp =
                        Response::err(id, ERR_IO, format!("no connected workspace for {host:?}"));
                    if let Ok(line) = serde_json::to_string(&resp) {
                        self.write_nvim(&line);
                    }
                }
            }
        }
    }
}

/// Run one editor lifetime: broker + optional initial workspace + nvim.
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

    {
        let router = Arc::clone(&router);
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                router.serve_nvim(stream);
            }
        });
    }

    let code = nvim::launch(opts)?;

    let _ = std::fs::remove_file(&socket_path);
    if let Some(t) = targets_file {
        let _ = std::fs::remove_file(t);
    }
    Ok(code)
}
