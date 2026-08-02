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
    let url =
        format!("https://github.com/neovim/neovim/releases/download/{NVIM_VERSION}/{asset}.tar.gz");

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
    eprintln!(
        "[rnvim] Neovim {NVIM_VERSION} installed at {}",
        version_dir.display()
    );
    Ok(bin)
}

pub struct LaunchOpts {
    pub socket: Option<PathBuf>,
    /// Parent directory of every workspace prefix (~/.rnvim/ws).
    pub ws_base: Option<PathBuf>,
    /// Workspace prefix of the initial workspace, if any.
    pub ws_root: Option<PathBuf>,
    pub host: Option<String>,
    /// Resolved remote absolute path the session opened at (picker root discovery).
    pub remote_entry: Option<String>,
    /// Candidate list for the in-editor connect picker.
    pub targets_file: Option<PathBuf>,
    /// File or directory to open (already mapped into the workspace prefix).
    pub entry: Option<PathBuf>,
    /// The instance starts without a chosen session root: the Lua runtime
    /// opens the remote directory browser on startup instead of a file.
    pub pending_root: bool,
    /// Daemon session id (lets the instance identify itself to the broker).
    pub instance: Option<u64>,
    /// nvim --listen socket (must precede positional args, or nvim treats
    /// it as a filename).
    pub listen: Option<PathBuf>,
    pub headless_cmds: Vec<String>,
}

/// Everything needed to spawn the managed nvim, regardless of who spawns it
/// (a foreground Command or a daemon-held PTY).
pub struct LaunchPlan {
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
}

pub fn plan(opts: &LaunchOpts) -> Result<LaunchPlan> {
    let nvim = ensure_nvim()?;
    let runtime_dir = crate::runtime::ensure_runtime()?;

    let mut envs = vec![
        ("NVIM_APPNAME".to_string(), "rnvim".to_string()),
        (
            "RNVIM_RUNTIME".to_string(),
            runtime_dir.to_string_lossy().into_owned(),
        ),
    ];
    if let Ok(exe) = std::env::current_exe() {
        // The Lua runtime builds LSP proxy commands with this.
        envs.push(("RNVIM_BIN".to_string(), exe.to_string_lossy().into_owned()));
    }
    let mut env_path = |key: &str, value: Option<&PathBuf>| {
        if let Some(v) = value {
            envs.push((key.to_string(), v.to_string_lossy().into_owned()));
        }
    };
    env_path("RNVIM_SOCKET", opts.socket.as_ref());
    env_path("RNVIM_WS_ROOT", opts.ws_root.as_ref());
    env_path("RNVIM_TARGETS", opts.targets_file.as_ref());
    env_path("RNVIM_WS_BASE", opts.ws_base.as_ref());
    if let Some(host) = &opts.host {
        envs.push(("RNVIM_HOST".to_string(), host.clone()));
    }
    if let Some(entry) = &opts.remote_entry {
        envs.push(("RNVIM_REMOTE_ENTRY".to_string(), entry.clone()));
    }
    if opts.pending_root {
        envs.push(("RNVIM_PENDING_ROOT".to_string(), "1".to_string()));
    }
    if let Some(instance) = opts.instance {
        envs.push(("RNVIM_INSTANCE".to_string(), instance.to_string()));
    }

    let mut args = vec![
        "-u".to_string(),
        runtime_dir.join("init.lua").to_string_lossy().into_owned(),
    ];
    if let Some(listen) = &opts.listen {
        args.push("--listen".to_string());
        args.push(listen.to_string_lossy().into_owned());
    }
    if !opts.headless_cmds.is_empty() {
        args.push("--headless".to_string());
    }
    if let Some(entry) = &opts.entry {
        args.push(entry.to_string_lossy().into_owned());
    }
    for c in &opts.headless_cmds {
        args.push(format!("+{c}"));
    }

    Ok(LaunchPlan {
        bin: nvim,
        args,
        envs,
    })
}

/// Launch the managed nvim in the foreground and wait for it to exit.
pub fn launch(opts: LaunchOpts) -> Result<i32> {
    let plan = plan(&opts)?;
    let mut cmd = Command::new(&plan.bin);
    cmd.args(&plan.args)
        .envs(plan.envs.iter().map(|(k, v)| (k, v)));
    let status = cmd
        .status()
        .with_context(|| format!("launch {}", plan.bin.display()))?;
    Ok(status.code().unwrap_or(1))
}
