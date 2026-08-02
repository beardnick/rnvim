//! Managed Neovim: download the pinned version on first use, launch isolated
//! under NVIM_APPNAME so the user's own Neovim setup is never touched.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The Neovim version this rnvim release is pinned to and tested against.
pub const NVIM_VERSION: &str = "v0.12.4";

pub fn rnvim_home() -> Result<PathBuf> {
    let home = env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".rnvim"))
}

fn asset_name() -> Result<&'static str> {
    Ok(match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => "nvim-macos-arm64",
        ("macos", "x86_64") => "nvim-macos-x86_64",
        ("linux", "x86_64") => "nvim-linux-x86_64",
        ("linux", "aarch64") => "nvim-linux-arm64",
        (os, arch) => bail!("unsupported platform for managed nvim: {os}/{arch}"),
    })
}

/// Return the managed nvim binary, downloading it on first use.
/// RNVIM_NVIM_BIN overrides (escape hatch / CI).
pub fn ensure_nvim() -> Result<PathBuf> {
    if let Some(bin) = env::var_os("RNVIM_NVIM_BIN") {
        return Ok(PathBuf::from(bin));
    }

    let asset = asset_name()?;
    let version_dir = rnvim_home()?.join("versions").join(NVIM_VERSION);
    let bin = version_dir.join(asset).join("bin").join("nvim");
    if bin.exists() {
        return Ok(bin);
    }

    eprintln!("[rnvim] downloading Neovim {NVIM_VERSION} ({asset})...");
    let tmp_dir = rnvim_home()?.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tarball = tmp_dir.join(format!("{asset}.tar.gz"));
    let url = format!(
        "https://github.com/neovim/neovim/releases/download/{NVIM_VERSION}/{asset}.tar.gz"
    );

    let status = Command::new("curl")
        .args(["-fL", "--progress-bar", "-o"])
        .arg(&tarball)
        .arg(&url)
        .status()
        .context("spawn curl (is curl installed?)")?;
    if !status.success() {
        bail!("download failed: {url}");
    }

    std::fs::create_dir_all(&version_dir)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&version_dir)
        .status()
        .context("spawn tar")?;
    if !status.success() {
        bail!("extract failed: {}", tarball.display());
    }
    let _ = std::fs::remove_file(&tarball);

    if !bin.exists() {
        bail!("nvim binary missing after extract: {}", bin.display());
    }
    eprintln!("[rnvim] Neovim {NVIM_VERSION} installed at {}", version_dir.display());
    Ok(bin)
}

pub struct LaunchOpts {
    pub socket: Option<PathBuf>,
    pub ws_root: Option<PathBuf>,
    pub host: Option<String>,
    /// File or directory to open (already mapped into the workspace prefix).
    pub entry: Option<PathBuf>,
    pub headless_cmds: Vec<String>,
}

/// Launch the managed nvim in the foreground and wait for it to exit.
pub fn launch(opts: LaunchOpts) -> Result<i32> {
    let nvim = ensure_nvim()?;
    let runtime_dir = crate::runtime::ensure_runtime()?;

    let mut cmd = Command::new(&nvim);
    cmd.env("NVIM_APPNAME", "rnvim")
        .env("RNVIM_RUNTIME", &runtime_dir)
        .arg("-u")
        .arg(runtime_dir.join("init.lua"));

    if let Some(socket) = &opts.socket {
        cmd.env("RNVIM_SOCKET", socket);
    }
    if let Some(ws_root) = &opts.ws_root {
        cmd.env("RNVIM_WS_ROOT", ws_root);
    }
    if let Some(host) = &opts.host {
        cmd.env("RNVIM_HOST", host);
    }

    if !opts.headless_cmds.is_empty() {
        cmd.arg("--headless");
    }
    if let Some(entry) = &opts.entry {
        cmd.arg(entry);
    }
    for c in &opts.headless_cmds {
        cmd.arg(format!("+{c}"));
    }

    let status = cmd.status().with_context(|| format!("launch {}", nvim.display()))?;
    Ok(status.code().unwrap_or(1))
}
