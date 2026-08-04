-- rnvim: remote development as a plain Neovim plugin.
--
-- Two personalities, decided before config load and fixed for the
-- instance's life:
--
--   vim.g.rnvim = { target = "host:path" }  → a REMOTE instance: connects
--     the agent, mounts the workspace, remote LSP/finder/grep. Set via
--     `--cmd` (the bin/rnvim launcher or the tmux driver does this), so
--     user configs can branch on vim.g.rnvim and skip local-fs-coupled
--     plugins entirely — nothing ever needs to be unloaded.
--
--   vim.g.rnvim unset → a LOCAL instance: only :RnvimConnect, which opens
--     targets as fresh instances through the multiplexer driver.

local M = {}

local function boot_remote(target_str)
  local util = require("rnvim.util")
  local rpc = require("rnvim.rpc")
  local workspace = require("rnvim.workspace")
  local target = util.parse_target(target_str)

  local ok, err = pcall(rpc.connect, target.host)
  if not ok then
    util.notify("connect failed: " .. tostring(err), vim.log.levels.ERROR)
    return
  end

  local function mount(abs)
    local ws = workspace.attach(target.host, abs)
    require("rnvim.fs").setup()
    require("rnvim.lsp").register_workspace(ws)
    require("rnvim.remotes").record_recent(target.host, abs)
    require("rnvim.sessions").retarget(target.host .. ":" .. abs)
    vim.cmd.edit(vim.fn.fnameescape(ws.ws_root .. abs:gsub("/+$", "")))
  end

  require("rnvim.lsp").setup()
  require("rnvim.term").setup()
  require("rnvim.picker").setup({ workspace = true })
  require("rnvim.sessions").register(target_str, require("rnvim.drivers").get().self_handle())

  if target.path ~= "" then
    local res = rpc.request("fs.resolve", { path = target.path })
    mount(res.abs)
  else
    -- Bare host: directory-selection stage before anything is a workspace.
    require("rnvim.fs").setup()
    vim.schedule(function()
      require("rnvim.picker").open_browse(target.host, mount)
    end)
  end
end

function M.setup()
  local flag = vim.g.rnvim
  if type(flag) == "table" and type(flag.target) == "string" and flag.target ~= "" then
    vim.g.rnvim_version = require("rnvim.util").version()
    boot_remote(flag.target)
  else
    require("rnvim.picker").setup({ workspace = false })
  end

  vim.api.nvim_create_user_command("RnvimRegistryUpdate", function()
    require("rnvim.registry").update()
  end, { desc = "rnvim: refresh the cached mason-registry snapshot" })
end

return M
