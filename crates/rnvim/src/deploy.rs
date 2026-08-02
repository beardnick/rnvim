//! Remote agent deployment over ssh.
//!
//! Preference order:
//! 1. remote platform == local platform → push our own binary
//! 2. prebuilt agent for the remote target, fetched from this version's
//!    GitHub release into ~/.rnvim/dist/ (authenticated locally via `gh`;
//!    the remote never needs GitHub access) → push it
//!
//! Cross-platform deploys therefore require this version's release to be
//! published (release-train discipline); same-platform pushes and `local:`
//! loopback sessions never need one.
//!
//! Artifacts are version-stamped filenames under ~/.rnvim/bin on the remote,
//! so a pure existence check is enough — client and agent can never skew.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

const REPO: &str = "beardnick/rnvim";

struct Probe {
    uname: String, // e.g. "Linux x86_64" / "Darwin arm64"
    have_bin: bool,
}

fn bin_path() -> String {
    format!("$HOME/.rnvim/bin/rnvim-agent-{}", env!("CARGO_PKG_VERSION"))
}

fn ssh_run(host: &str, script: &str, stdin_data: Option<&[u8]>) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", host, script])
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn ssh")?;
    if let Some(data) = stdin_data {
        child
            .stdin
            .take()
            .context("ssh stdin")?
            .write_all(data)
            .context("stream to ssh")?;
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
        "uname -sm; test -x {bin} && echo bin=yes || echo bin=no",
        bin = bin_path(),
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

/// Rust target triple for a remote's `uname -sm`, matching release assets.
fn remote_target(uname_sm: &str) -> Option<&'static str> {
    match uname_sm {
        "Linux x86_64" => Some("x86_64-unknown-linux-musl"),
        "Linux aarch64" => Some("aarch64-unknown-linux-musl"),
        "Darwin arm64" => Some("aarch64-apple-darwin"),
        "Darwin x86_64" => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// Fetch the prebuilt agent for `target` from this version's GitHub release,
/// caching it under ~/.rnvim/dist/<version>/. The cache also makes previously
/// used targets available offline.
fn fetch_agent_dist(target: &str) -> Result<PathBuf> {
    let version = env!("CARGO_PKG_VERSION");
    let dist_dir = crate::nvim::rnvim_home()?.join("dist").join(version);
    let asset = format!("rnvim-{target}");
    let path = dist_dir.join(&asset);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(&dist_dir)?;
    let tag = format!("v{version}");

    // `gh` carries auth for the private repo; anonymous curl works once the
    // repo is public. Either way the download happens locally, never remotely.
    let gh_ok = Command::new("gh")
        .args([
            "release",
            "download",
            &tag,
            "--repo",
            REPO,
            "--pattern",
            &asset,
            "--dir",
        ])
        .arg(&dist_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !gh_ok {
        let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
        let curl_ok = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&path)
            .arg(&url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !curl_ok {
            let _ = std::fs::remove_file(&path);
            bail!("no prebuilt agent {asset} for release {tag} (gh and curl both failed)");
        }
    }
    if !path.exists() {
        bail!(
            "download reported success but {} is missing",
            path.display()
        );
    }
    Ok(path)
}

fn push_agent_binary(host: &str, data: &[u8]) -> Result<()> {
    let script = format!(
        "mkdir -p $HOME/.rnvim/bin && cat > {bin}.tmp && chmod +x {bin}.tmp && mv {bin}.tmp {bin}",
        bin = bin_path()
    );
    ssh_run(host, &script, Some(data))?;
    Ok(())
}

/// Make sure a compatible agent exists on the remote; return the shell
/// command that starts it (passed to ssh by the transport).
pub fn ensure_remote_agent(host: &str) -> Result<String> {
    let p = probe(host)?;
    let bin_cmd = format!("{} agent --stdio", bin_path());

    if p.have_bin {
        return Ok(bin_cmd);
    }

    // 1. Same platform: our own binary IS the agent.
    if p.uname == local_uname_sm() {
        eprintln!("[rnvim] deploying agent binary to {host} ({})...", p.uname);
        let exe = std::env::current_exe().context("current_exe")?;
        let data = std::fs::read(&exe).context("read own binary")?;
        push_agent_binary(host, &data)?;
        return Ok(bin_cmd);
    }

    // 2. Prebuilt agent from this version's release.
    let Some(target) = remote_target(&p.uname) else {
        bail!(
            "remote {host} is {} — no agent build exists for this platform \
             (supported: Linux/macOS on x86_64/aarch64)",
            p.uname
        );
    };
    let dist = fetch_agent_dist(target).with_context(|| {
        format!(
            "no agent available for {host} ({}): the v{} release must be published \
             for cross-platform deploys",
            p.uname,
            env!("CARGO_PKG_VERSION")
        )
    })?;
    eprintln!("[rnvim] deploying prebuilt agent ({target}) to {host}...");
    let data = std::fs::read(&dist).with_context(|| format!("read {}", dist.display()))?;
    push_agent_binary(host, &data)?;
    Ok(bin_cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_targets_map_to_release_assets() {
        assert_eq!(
            remote_target("Linux x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            remote_target("Linux aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(remote_target("Darwin arm64"), Some("aarch64-apple-darwin"));
        assert_eq!(remote_target("Darwin x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(remote_target("SunOS sparc64"), None);
    }
}
