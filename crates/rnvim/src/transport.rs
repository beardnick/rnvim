//! Agent transports. MVP: everything runs over the agent's stdio, carried by
//! ssh (remote) or a plain subprocess (loopback). QUIC comes later behind the
//! same seam.

use std::io::BufReader;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};

pub struct AgentConn {
    /// Kept only so the process isn't detached from us; the agent exits by
    /// itself on stdin EOF, which happens when this process ends.
    #[allow(dead_code)]
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: BufReader<ChildStdout>,
}

impl AgentConn {
    fn from_command(mut cmd: Command, what: &str) -> Result<AgentConn> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawn {what}"))?;
        let stdin = child.stdin.take().context("agent stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("agent stdout")?);
        Ok(AgentConn {
            child,
            stdin,
            stdout,
        })
    }

    /// Loopback agent: this same binary in `agent --stdio` mode.
    pub fn spawn_local() -> Result<AgentConn> {
        let exe = std::env::current_exe().context("current_exe")?;
        let mut cmd = Command::new(exe);
        cmd.args(["agent", "--stdio"]);
        Self::from_command(cmd, "local agent")
    }

    /// Remote agent over ssh. `remote_cmd` is produced by deploy::ensure_remote_agent.
    pub fn spawn_ssh(host: &str, remote_cmd: &str) -> Result<AgentConn> {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", host, remote_cmd]);
        Self::from_command(cmd, "ssh agent")
    }
}
