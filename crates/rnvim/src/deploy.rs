//! Remote agent deployment over ssh.
//!
//! Same platform as the local machine → push our own binary (fast path).
//! Different platform → push the portable python agent (works everywhere
//! python3 exists; release builds will ship musl agent binaries instead).
//!
//! Artifacts are version-stamped filenames under ~/.rnvim/bin on the remote,
//! so a pure existence check is enough — client and agent can never skew.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const PY_AGENT: &str = include_str!("py_agent.py");

struct Probe {
    uname: String, // e.g. "Linux x86_64" / "Darwin arm64"
    have_bin: bool,
    have_py: bool,
    have_python3: bool,
}

fn bin_path() -> String {
    format!("$HOME/.rnvim/bin/rnvim-agent-{}", env!("CARGO_PKG_VERSION"))
}

fn py_path() -> String {
    format!("$HOME/.rnvim/bin/rnvim-agent-{}.py", env!("CARGO_PKG_VERSION"))
}

fn ssh_run(host: &str, script: &str, stdin_data: Option<&[u8]>) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", host, script])
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn ssh")?;
    if let Some(data) = stdin_data {
        child.stdin.take().context("ssh stdin")?.write_all(data).context("stream to ssh")?;
        // stdin drops here, closing the pipe so the remote `cat` finishes
    }
    let out = child.wait_with_output().context("wait ssh")?;
    if !out.status.success() {
        bail!(
            "ssh to {host} failed: {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn probe(host: &str) -> Result<Probe> {
    let script = format!(
        "uname -sm; \
         test -x {bin} && echo bin=yes || echo bin=no; \
         test -f {py} && echo py=yes || echo py=no; \
         command -v python3 >/dev/null 2>&1 && echo python3=yes || echo python3=no",
        bin = bin_path(),
        py = py_path(),
    );
    let out = ssh_run(host, &script, None)?;
    let mut lines = out.lines();
    let uname = lines.next().unwrap_or_default().trim().to_string();
    if uname.is_empty() {
        bail!("could not probe {host}: empty uname output");
    }
    let rest: Vec<&str> = lines.map(str::trim).collect();
    Ok(Probe {
        uname,
        have_bin: rest.contains(&"bin=yes"),
        have_py: rest.contains(&"py=yes"),
        have_python3: rest.contains(&"python3=yes"),
    })
}

/// Local platform in `uname -sm` terms, for comparison with the remote.
fn local_uname_sm() -> String {
    let os = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let arch = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, "aarch64") => "aarch64",
        (_, a) => a,
    };
    format!("{os} {arch}")
}

/// Make sure a compatible agent exists on the remote; return the shell
/// command that starts it (passed to ssh by the transport).
pub fn ensure_remote_agent(host: &str) -> Result<String> {
    let p = probe(host)?;

    if p.uname == local_uname_sm() {
        if !p.have_bin {
            eprintln!("[rnvim] deploying agent binary to {host} ({})...", p.uname);
            let exe = std::env::current_exe().context("current_exe")?;
            let data = std::fs::read(&exe).context("read own binary")?;
            let script = format!(
                "mkdir -p $HOME/.rnvim/bin && cat > {bin}.tmp && chmod +x {bin}.tmp && mv {bin}.tmp {bin}",
                bin = bin_path()
            );
            ssh_run(host, &script, Some(&data))?;
        }
        return Ok(format!("{} agent --stdio", bin_path()));
    }

    // Cross-platform: fall back to the portable python agent.
    if !p.have_python3 {
        bail!(
            "remote {host} is {} (local is {}) and has no python3; \
             cannot deploy an agent yet. Release builds will ship musl binaries for this.",
            p.uname,
            local_uname_sm()
        );
    }
    if !p.have_py {
        eprintln!("[rnvim] deploying portable agent to {host} ({})...", p.uname);
        let script = format!(
            "mkdir -p $HOME/.rnvim/bin && cat > {py}.tmp && mv {py}.tmp {py}",
            py = py_path()
        );
        ssh_run(host, &script, Some(PY_AGENT.as_bytes()))?;
    }
    Ok(format!("python3 {}", py_path()))
}
