# rnvim

Remote development for Neovim: **edit locally, files and intelligence live remotely** — VSCode Remote's architecture, as a plain Neovim plugin.

```bash
rnvim dev-box:~/project
```

One command: opens a fresh Neovim instance (a new tmux window when you're inside tmux), deploys the remote agent over SSH automatically, and mounts the remote workspace. Buffers are local, so typing has zero latency; files, LSP servers, and toolchains stay on the remote.

## How it works

rnvim is a Lua plugin plus a small Rust agent:

- **The plugin** runs in your own Neovim. An instance is either *local* (your full config, plus `:RnvimConnect`) or *remote* (one workspace, for the instance's whole life) — decided before your config loads, never switched in place.
- **The agent** (`rnvim-agent`, a static Rust binary) is deployed to the remote automatically and serves files, search, and installs over ssh's stdio. Remotes never need GitHub access or any runtime installed.

A remote instance is started with the flag set on the command line:

```bash
nvim --cmd 'lua vim.g.rnvim = { target = "dev-box:~/project" }'
```

`bin/rnvim` and the tmux driver do this for you. Because `vim.g.rnvim` exists before `init.lua` runs, your config can branch on it exactly like `vim.g.vscode` (the vscode-neovim convention) and skip plugins that assume a local filesystem — nothing ever needs to be unloaded:

```lua
-- lazy.nvim spec
{ "lewis6991/gitsigns.nvim", cond = not vim.g.rnvim },
{ "nvim-telescope/telescope.nvim", cond = not vim.g.rnvim },
```

Plugins to exclude are the ones coupled to the local filesystem: file finders and explorers, git integrations, mason/local LSP management, session managers. Everything pure-buffer (surround, textobjects, themes, treesitter, completion, copilot) works unchanged.

## Install

With lazy.nvim:

```lua
{
  "beardnick/rnvim",
  -- optional: put bin/rnvim on your PATH for the shell entry point
}
```

Requirements: Neovim 0.11+, ssh with key auth to your hosts. tmux is optional — with it you get multi-workspace switching and detachable sessions; without it, instances run in the foreground.

## Sessions and multiplexers

Session *logic* lives in the plugin (`~/.rnvim/sessions.json`, swept by pid liveness); session *lifetime* lives in your multiplexer. Drivers only need two operations — spawn and focus — and are auto-detected from the environment (`vim.g.rnvim_driver` overrides):

| Driver | Spawn | Focus | Sessions survive terminal restart |
|---|---|---|---|
| **tmux** | new window | ✓ | ✓ |
| **screen** | new window (4.0-compatible) | ✓ | ✓ |
| **zellij** | new tab (generated layout) | ✓ by tab name | ✓ |
| **herdr** | new tab (socket API) | ✓ | ✓ |
| **kitty** | new tab (needs `allow_remote_control yes`) | ✓ | ✗ |
| **ghostty** | new OS window | ✗ (no remote control) | ✗ |
| **warp** | new tab (`warp://launch` config) | ✗ (no remote control) | ✗ |
| **none** | — start from a shell: `rnvim <target>` | ✗ | ✗ |

True multiplexers are preferred over plain terminals when nested (tmux inside kitty → tmux). A driver is ~40 lines — contributions welcome.

`:RnvimConnect` from any instance lists open sessions (switch), recent workspaces, and ssh-config hosts (spawn). Picking a bare host runs a directory-selection stage inside the new instance before anything becomes a workspace.

## What you get in a remote instance

- **Virtual filesystem**: buffers under `~/.rnvim/ws/<host>/…` read and write through the agent. Directory browsing with `<CR>` / `-`.
- **LSP**: servers run on the remote through your login shell; a pure-Lua transport rewrites paths both ways (prefix mapping, no proxy binary). Built-in configs: gopls, rust-analyzer, clangd, pyright, ts_ls, lua_ls. Settings you register under the standard names flow into the remote variants.
- **Auto-install**: a missing server is installed on the remote under `~/.rnvim/tools/`, resolved from a mason-registry snapshot (versions pinned, github artifacts downloaded by the agent's native HTTP client, npm/go through the remote's own toolchain). Override or extend with `vim.g.rnvim_lsp_recipes`.
- **Finder / grep**: `<C-p>` fuzzy files, `<C-g>` live grep — walking, ignoring, and scoring all run on the agent; only the top results cross the wire.
- **`:RnvimTerm`**: a terminal on the workspace host, cd'd to the current buffer's directory.
- **`local:` loopback**: `rnvim local:/some/dir` runs the agent as a subprocess — the whole stack without ssh, for development and tests.

## Architecture

```
┌─ local ─────────────────────────────┐      ┌─ remote ────────────┐
│  your Neovim (your config)          │      │                     │
│    └─ rnvim plugin (Lua)            │ ssh  │  rnvim-agent (Rust) │
│        agent rpc / virtual fs  ─────┼──────┼─  fs / find / exec /│
│        LSP rewrite / installer      │      │   fetch             │
└─────────────────────────────────────┘      └─────────────────────┘
```

- `lua/rnvim/` — the plugin: agent transport, virtual fs, LSP transport + rewrite, deploy, registry, sessions, drivers, pickers
- `crates/rnvim-agent` — the remote agent (JSON lines over stdio)
- `crates/rnvim-proto` — protocol types shared by agent and tests
- `bin/rnvim` — shell launcher

Path model: remote absolute paths are mounted under the local prefix `~/.rnvim/ws/<host>/` (prefix mapping, not a URL scheme), so LSP URI rewriting degenerates to pure prefix replacement.

Versioning: the plugin (`lua/rnvim/meta.lua`) and the agent release share one version. Agents are version-stamped on the remote (`~/.rnvim/bin/rnvim-agent-<version>`), deployed from the matching GitHub release — plugin and agent can never skew.

## Development

```bash
# everything: cargo tests, Lua unit specs, loopback e2e
./tests/run.sh

# just the plugin against a specific nvim
NVIM=/path/to/nvim ./tests/run.sh
```

## Roadmap

- LSP watched-files (remote watcher)
- git integration (read-only trio: blame, diff, log)
- protocol snapshot tests, docker sshd integration tests
