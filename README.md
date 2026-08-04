# rnvim

Remote development tool: **edit locally, files and intelligence live remotely** — VSCode Remote's architecture with Neovim at the core.

```bash
rnvim dev-box:~/project
```

One command: downloads a pinned Neovim version (fully isolated from any Neovim you already have), deploys the remote agent over SSH automatically, and opens the remote workspace locally. Buffers are local, so typing has zero latency; files, LSP, and toolchains stay on the remote.

## Design principles

- **Local frontend + remote workspace backend.** Not `--remote-ui` (typing latency = network RTT), and not sshfs (a consistency swamp). The authoritative files always live on the remote; the local side only holds buffer copies of open files.
- **Locked runtime.** Each rnvim version pins one Neovim version (currently v0.12.4). Client, agent, and the bundled Lua runtime ship as one atomic unit with exact protocol-version matching — no compatibility matrix.
- **No accommodation of the legacy plugin ecosystem.** Workspace capabilities (finder, git, LSP integration) are first-party remote-native implementations, not compatibility shims for existing plugins. Pure-buffer plugins (surround, textobjects, colorschemes) work naturally and load through the user overlay (`~/.config/rnvim/user/init.lua`).

## Current status (MVP: M0 + M1)

- [x] Managed Neovim: pinned version auto-downloaded on first run, launched isolated under `NVIM_APPNAME=rnvim`
- [x] Bundled Lua runtime, shipped inside the binary, unpacked at startup
- [x] Automatic agent deploy over SSH: same platform pushes our own binary; cross-platform pulls the musl static build from this version's GitHub Release (cached locally under `~/.rnvim/dist/`; cross-platform deploys therefore require that version's release to be published — part of the release-train discipline)
- [x] Protocol handshake + version check (JSON lines over stdio)
- [x] Remote file open / edit / save (BufReadCmd/BufWriteCmd → agent fs service)
- [x] Remote directory browsing (`<CR>` to enter, `-` to go up)
- [x] Creating remote files (with automatic parent-directory creation)
- [x] `local:` loopback mode (development/testing path that needs no sshd)

### Try it

```bash
cargo build --release

# Remote session (requires passwordless ssh)
./target/release/rnvim dev-box:~/project

# Local loopback (no remote machine needed)
./target/release/rnvim local:/tmp/somedir
```

## Architecture

```
┌─ local ───────────────────────────┐      ┌─ remote ────────────┐
│  Neovim v0.12.4 (managed, isolated)│      │                     │
│    └─ bundled Lua runtime          │      │  rnvim agent        │
│         │ unix socket (JSON lines) │ ssh  │   fs.read/write/    │
│  rnvim client (broker)  ───────────┼──────┼─  list/stat/resolve │
│    version mgmt / deploy / pumps   │      │                     │
└───────────────────────────────────┘      └─────────────────────┘
```

- `crates/rnvim-proto` — protocol types, the single shared source of truth between client and agent
- `crates/rnvim-agent` — the remote agent (`rnvim agent --stdio`)
- `crates/rnvim` — the client: CLI, Neovim version management, transport, deploy, session broker
- `runtime/` — bundled Neovim Lua runtime (rpc client + virtual filesystem)

Path model: remote absolute paths are mounted under the local prefix `~/.rnvim/ws/<host>/` (prefix mapping, not a URL scheme) — a deliberate head start for the LSP proxy's URI rewriting, which degenerates to pure prefix replacement.

## Roadmap

- [x] **M2**: LSP support
  - `rnvim lsp-proxy`: an LSP stdio proxy doing Content-Length-framed JSON rewriting (local prefix ↔ remote path, both directions; the remote root needed for the reverse direction is captured automatically from the `initialize` request)
  - Servers start on the remote through the user's login shell (full PATH); `exec.which` probes availability and warns when needed
  - `fs.findroot`: root markers probed on the remote filesystem
  - Built-in server set: gopls, rust-analyzer, clangd, pyright, ts_ls, lua_ls (first-party configs, no lspconfig dependency); LSP-adjacent plugins (completion / diagnostics UI) work naturally through `vim.lsp`
  - `:RnvimTerm`: remote terminal, cd'd to the current buffer's directory
  - Known limits: paths embedded mid-string are not rewritten (only values starting at a path boundary); percent-encoded URIs unhandled; file watching disabled for now (remote watcher comes with the QUIC milestone)
- [x] **M3**: workspace navigation
  - `<C-p>` / `:RnvimFiles`: fuzzy file jumping — file walking (ignore, respects .gitignore) and fuzzy scoring (nucleo) all run on the remote agent; only the top N results cross the wire per keystroke, so huge repos don't suffer from file count
  - `<C-g>` / `:RnvimGrep`: live grep (ripgrep engine used as a library, smart-case, invalid regexes fall back to literal search); `<C-q>` sends all results to quickfix
  - `:RnvimConnect` + the bare `rnvim` picker: remote target management — parses `~/.ssh/config` (with Include) into a host list; `~/.rnvim/recent.json` remembers recent workspaces (host + directory, deduped, capped at 50); selecting a target inside the editor hands the session over seamlessly (handoff + client outer loop)
  - Agent file listings carry a 10s cache; a hard cap of 200k files keeps pathological directories from dragging the agent down
- [x] **Multi-workspace routing**: the broker is a router across any number of agents and nvim connections (messages routed by a host field + id remapping; `session.*` control methods handled by the broker itself). Each workspace registers its own LSP configs (`gopls_<slug>`), zero external dependencies
- [x] **Session sidebar + picker integration**: a persistent left rail lists every session (numbered, active marker, mouse-clickable, `Ctrl-\ b` toggles, `Ctrl-\ 1..9` jumps by number, auto-hides on narrow terminals); `:RnvimConnect`'s candidate list shows open sessions at the top — Enter switches to the instance instead of creating a duplicate
- [x] **Daemonized PTY host (herdr-shaped)**: `rnvim daemon` starts automatically (setsid, detached from the terminal) and owns every nvim instance (one PTY per session) plus the shared agent router. The client is a raw-mode passthrough; **detach/reattach keeps sessions alive** — close the terminal, reopen, and buffers/LSP/undo are all still there
  - Prefix key `Ctrl-\`: `d` detach · `n`/`p` cycle instances · `s` session list · `c` new session · `Ctrl-\ Ctrl-\` sends the literal key
  - Rendering is painted from a per-session virtual screen (vt100), with row-level diffs and native scroll ops; slow clients are protected by coalescing and can never freeze nvim
  - `:RnvimConnect` now opens a **new instance** (daemon session) instead of a tab inside the current editor
  - `--headless-cmd` keeps the legacy direct mode (for tests/scripting, bypasses the daemon)
- [x] **Directory-selection stage on connect**: `rnvim host` (no path) first enters a remote directory browser (`<CR>` descends · `<C-s>` picks the current directory as the session root); only then does it become a session. Picking a bare host in `:RnvimConnect` goes through the same browsing stage. Recents record the chosen project directory, not the home dir
- **M4**: QUIC transport (0-RTT reconnect, roaming) + SSH stdio fallback, port forwarding, read-only git trio
- [x] Release engineering: CI (fmt/clippy/test) + tag-triggered four-platform builds (including the musl static agent) published to GitHub Releases; the client pulls the prebuilt agent for the remote platform on demand (downloaded locally via `gh` auth, cached under `~/.rnvim/dist/`, pushed over SSH — the remote machine never needs GitHub access)
- [x] **Automatic remote LSP install (mason-registry data source)**: missing servers are installed on the remote under `~/.rnvim/tools/`. Layering: the agent only provides a generic `exec.run` (login shell, tools prefix, runs on its own thread) — **what to install and how lives entirely in the client**: `rnvim registry script <name>` resolves the package spec from a mason-registry snapshot (cached locally, refreshed with `rnvim registry update`) into a self-contained POSIX script, with versions pinned by the registry (never `latest`); supports pkg:github/npm/golang sources; users can override or add any recipe via `vim.g.rnvim_lsp_recipes`. npm/go go through their ecosystems' own integrity checks; GitHub assets are TLS + version-pinned (the registry carries no hashes)
- [x] **User config integration (vscode-neovim style)**: `vim.g.rnvim` is the public contract; writing `{"user_config": true}` to `~/.rnvim/config.json` explicitly opts in to loading `~/.config/nvim` (plugin data stays isolated under the rnvim APPNAME); user configs branch on `vim.g.rnvim`
- To do: protocol snapshot tests, docker sshd integration tests, hash verification for GitHub assets (once the registry provides them)
