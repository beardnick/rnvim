//! `rnvim-agent` — the remote agent binary. Serves the rnvim protocol as
//! JSON lines on stdin/stdout; spawned over ssh by the client plugin (or
//! as a plain subprocess for `local:` loopback sessions and tests).

fn main() -> anyhow::Result<()> {
    // The only mode is stdio; `--stdio` is accepted for explicitness and
    // forward compatibility.
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        None | Some("--stdio") => rnvim_agent::run_stdio(),
        Some("--version") | Some("-V") => {
            println!("rnvim-agent {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("rnvim-agent: unknown argument {other:?} (expected --stdio)");
            std::process::exit(2);
        }
    }
}
