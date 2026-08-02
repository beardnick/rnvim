-- rnvim managed runtime entrypoint. Loaded via `nvim -u` by the rnvim client.

-- Public contract, set before anything user-visible loads: user configs
-- branch on this exactly like vim.g.vscode (vscode-neovim convention).
vim.g.rnvim = true
vim.g.rnvim_version = vim.env.RNVIM_VERSION

local runtime = vim.env.RNVIM_RUNTIME
if runtime and runtime ~= "" then
  vim.opt.runtimepath:prepend(runtime)
end

vim.o.number = true
vim.o.termguicolors = true
vim.o.mouse = "a"
vim.o.ignorecase = true
vim.o.smartcase = true
vim.o.signcolumn = "yes"
vim.o.updatetime = 300
-- Truncate long messages instead of blocking on hit-enter prompts; a
-- prompt pending across detach/reattach also stalls the repaint RPC.
vim.opt.shortmess:append("aT")

require("rnvim").setup()

-- Eager-load every rnvim module: user plugin managers may reset the
-- runtimepath, and cached modules keep our autocmd callbacks working
-- regardless.
for _, mod in ipairs({ "workspaces", "rpc", "fs", "lsp", "term", "picker" }) do
  pcall(require, "rnvim." .. mod)
end

-- Explicit opt-in (user_config in ~/.rnvim/config.json): load the user's
-- own nvim config. It sees vim.g.rnvim and takes its rnvim branch; plugin
-- data still lives under the rnvim NVIM_APPNAME, fully isolated.
if vim.env.RNVIM_USER_CONFIG == "1" then
  local user_init = vim.fn.expand("~/.config/nvim/init.lua")
  if vim.uv.fs_stat(user_init) then
    local ok, err = pcall(dofile, user_init)
    if not ok then
      vim.notify("[rnvim] user config error: " .. tostring(err), vim.log.levels.ERROR)
    end
  else
    vim.notify("[rnvim] user_config enabled but ~/.config/nvim/init.lua not found", vim.log.levels.WARN)
  end
end

-- Overlay: small additions without adopting a whole config.
-- Lives outside the managed runtime so upgrades never touch it.
local overlay = vim.fn.stdpath("config") .. "/user/init.lua"
if vim.uv.fs_stat(overlay) then
  local ok, err = pcall(dofile, overlay)
  if not ok then
    vim.notify("[rnvim] user overlay error: " .. tostring(err), vim.log.levels.ERROR)
  end
end
