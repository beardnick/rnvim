mod deploy;
mod lsp_proxy;
mod nvim;
mod runtime;
mod session;
mod target;
mod transport;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// rnvim — remote development with a managed Neovim.
///
/// `rnvim host:path` opens a remote workspace: a version-pinned Neovim runs
/// locally (zero-latency editing), files live on the remote machine, served
/// by an auto-deployed agent over ssh.
#[derive(Parser)]
#[command(name = "rnvim", version, about)]
struct Cli {
    /// Remote target: [user@]host[:path]. Path defaults to the remote home.
    /// Use "local[:path]" for a loopback session (dev/testing).
    /// Omit entirely to open the managed editor on local files.
    target: Option<String>,

    /// (testing) run nvim headless, executing these ex-commands in order
    #[arg(long = "headless-cmd", hide = true)]
    headless_cmd: Vec<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the remote agent (spawned over ssh; not for interactive use)
    Agent {
        /// Serve the rnvim protocol on stdin/stdout
        #[arg(long)]
        stdio: bool,
    },
    /// LSP stdio proxy: rewrite paths and run the server on the remote host
    /// (spawned by the managed nvim; not for interactive use)
    LspProxy {
        /// Remote host ("local" runs the server as a plain subprocess)
        #[arg(long)]
        host: String,
        /// Local workspace prefix to strip/add when rewriting paths
        #[arg(long = "ws-root")]
        ws_root: String,
        /// Language server command, after `--`
        #[arg(last = true)]
        server: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Some(Cmd::Agent { stdio: _ }) => rnvim_agent::run_stdio(),
        Some(Cmd::LspProxy {
            host,
            ws_root,
            server,
        }) => {
            let code = lsp_proxy::run(&host, &ws_root, &server)?;
            std::process::exit(code);
        }
        None => {
            let code = match cli.target {
                Some(t) => session::run_remote(&t, &cli.headless_cmd)?,
                None => session::run_local_editor(&cli.headless_cmd)?,
            };
            std::process::exit(code);
        }
    }
}
