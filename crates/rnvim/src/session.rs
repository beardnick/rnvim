//! A remote session: handshake with the agent, then broker the nvim-side
//! unix socket onto the agent's stdio while nvim runs in the foreground.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;

use anyhow::{bail, Context, Result};
use rnvim_proto::*;
use serde_json::json;

use crate::deploy;
use crate::nvim::{self, LaunchOpts};
use crate::remotes;
use crate::target::Target;
use crate::transport::AgentConn;

/// What a finished editor session asks the outer loop to do next.
pub struct Outcome {
    pub code: i32,
    /// Target written by the in-editor connect picker; the outer loop
    /// reopens straight into it.
    pub next_target: Option<String>,
}

/// Per-session files consumed by the connect picker: the candidate list
/// (recent + ssh hosts) and the handoff file it writes its choice to.
struct ConnectFiles {
    targets: Option<std::path::PathBuf>,
    handoff: std::path::PathBuf,
}

fn prepare_connect_files() -> Result<ConnectFiles> {
    let targets = match remotes::write_targets_file() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[rnvim] target list unavailable: {e}");
            None
        }
    };
    let run_dir = nvim::rnvim_home()?.join("run");
    std::fs::create_dir_all(&run_dir)?;
    let handoff = run_dir.join(format!("handoff-{}", std::process::id()));
    let _ = std::fs::remove_file(&handoff);
    Ok(ConnectFiles { targets, handoff })
}

fn finish(code: i32, files: ConnectFiles) -> Outcome {
    let next_target = std::fs::read_to_string(&files.handoff)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let _ = std::fs::remove_file(&files.handoff);
    if let Some(t) = &files.targets {
        let _ = std::fs::remove_file(t);
    }
    Outcome { code, next_target }
}

pub fn run_local_editor(headless_cmds: &[String]) -> Result<Outcome> {
    let connect = prepare_connect_files()?;
    let code = nvim::launch(LaunchOpts {
        socket: None,
        ws_root: None,
        host: None,
        remote_entry: None,
        targets_file: connect.targets.clone(),
        handoff_file: Some(connect.handoff.clone()),
        entry: None,
        headless_cmds: headless_cmds.to_vec(),
    })?;
    Ok(finish(code, connect))
}

pub fn run_remote(target: &str, headless_cmds: &[String]) -> Result<Outcome> {
    let target = Target::parse(target);

    // 1. Connect (deploying the agent first if needed).
    let mut agent = if target.is_local() {
        AgentConn::spawn_local()?
    } else {
        let remote_cmd = deploy::ensure_remote_agent(&target.host)?;
        AgentConn::spawn_ssh(&target.host, &remote_cmd)?
    };

    // 2. Handshake: verify protocol compatibility before anything else.
    let hello = agent.request(&Request {
        id: 0,
        method: "hello".into(),
        params: json!(HelloParams {
            client_version: env!("CARGO_PKG_VERSION").into(),
            proto: PROTO_VERSION,
        }),
    })?;
    if let Some(err) = hello.error {
        bail!("agent handshake failed: {}", err.message);
    }

    // 3. Resolve the requested path to a remote absolute path.
    let resolved = agent.request(&Request {
        id: 1,
        method: "fs.resolve".into(),
        params: json!(ResolveParams {
            path: target.path.clone()
        }),
    })?;
    if let Some(err) = resolved.error {
        bail!(
            "cannot resolve remote path {:?}: {}",
            target.path,
            err.message
        );
    }
    let resolved: ResolveResult =
        serde_json::from_value(resolved.result.context("fs.resolve result")?)?;
    remotes::record_recent(&target.host, &resolved.abs);

    // 4. Map it under the local workspace prefix.
    let ws_root = nvim::rnvim_home()?.join("ws").join(target.host_slug());
    let entry = PathBuf::from(format!(
        "{}{}",
        ws_root.display(),
        resolved.abs.trim_end_matches('/')
    ));

    // 5. Session socket for the nvim-side Lua runtime.
    let run_dir = nvim::rnvim_home()?.join("run");
    std::fs::create_dir_all(&run_dir)?;
    let socket_path = run_dir.join(format!("session-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;

    // 6. Broker: pump lines between nvim's socket connection and the agent.
    let AgentConn {
        stdin: mut agent_in,
        stdout: mut agent_out,
        ..
    } = agent;
    thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let Ok(read_half) = stream.try_clone() else {
            return;
        };
        let mut write_half = stream;

        let to_agent = thread::spawn(move || {
            for line in BufReader::new(read_half).lines() {
                let Ok(line) = line else { break };
                if agent_in.write_all(line.as_bytes()).is_err() {
                    break;
                }
                if agent_in.write_all(b"\n").is_err() {
                    break;
                }
                if agent_in.flush().is_err() {
                    break;
                }
            }
        });

        let mut line = String::new();
        loop {
            line.clear();
            match agent_out.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if write_half.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_agent.join();
    });

    // 7. Run nvim in the foreground; the session lives as long as it does.
    //    NOTE: `agent`'s Child was moved apart above, so the agent process is
    //    reaped when this function returns and the process exits.
    let connect = prepare_connect_files()?;
    let code = nvim::launch(LaunchOpts {
        socket: Some(socket_path.clone()),
        ws_root: Some(ws_root),
        host: Some(target.host.clone()),
        remote_entry: Some(resolved.abs.clone()),
        targets_file: connect.targets.clone(),
        handoff_file: Some(connect.handoff.clone()),
        entry: Some(entry),
        headless_cmds: headless_cmds.to_vec(),
    })?;

    let _ = std::fs::remove_file(&socket_path);
    Ok(finish(code, connect))
}
