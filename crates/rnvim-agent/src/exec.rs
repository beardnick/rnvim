//! Generic remote execution: run a caller-supplied script through the
//! user's login shell and report its output.
//!
//! Deliberately policy-free — the agent knows *how* to run things on this
//! host (login shell for profile PATHs, a writable tools prefix, output
//! capture); it knows nothing about language servers. Install recipes live
//! in the editor-side Lua runtime, so a new server, a fix, or a user's own
//! recipe needs no agent release.

use anyhow::{Context, Result};

/// Where installed tools belong. Exported so `exec.which` and the LSP
/// proxy can prepend it, and exposed to scripts as $RNVIM_TOOLS.
pub const TOOLS_PATH: &str = "$HOME/.rnvim/tools/bin:$HOME/.rnvim/tools/npm/node_modules/.bin";

pub struct RunOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run `script` via `$SHELL -lc` with the tools prefix on PATH and
/// $RNVIM_TOOLS/$RNVIM_TOOLS_BIN pointing at the writable install root.
pub fn run(script: &str) -> Result<RunOutput> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let tools = format!("{home}/.rnvim/tools");
    std::fs::create_dir_all(format!("{tools}/bin")).ok();

    let wrapped = format!(
        "export RNVIM_TOOLS=\"{tools}\"; \
         export RNVIM_TOOLS_BIN=\"{tools}/bin\"; \
         export PATH=\"{TOOLS_PATH}:$PATH\"; \
         {script}"
    );
    let out = std::process::Command::new(shell)
        .arg("-lc")
        .arg(&wrapped)
        .output()
        .context("run script through login shell")?;
    Ok(RunOutput {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_scripts_and_reports_status() {
        let out = run("echo hello").unwrap();
        assert_eq!(out.code, 0);
        assert_eq!(out.stdout.trim(), "hello");

        let out = run("echo oops >&2; exit 3").unwrap();
        assert_eq!(out.code, 3);
        assert!(out.stderr.contains("oops"));
    }

    #[test]
    fn exposes_the_tools_prefix() {
        let out = run("echo \"$RNVIM_TOOLS_BIN\"").unwrap();
        assert!(
            out.stdout.trim().ends_with("/.rnvim/tools/bin"),
            "{:?}",
            out.stdout
        );
        let out = run("command -v true >/dev/null && echo ok").unwrap();
        assert_eq!(out.stdout.trim(), "ok", "login shell PATH intact");
    }
}
