-- First-party LSP integration, multi-workspace: each connected workspace
-- registers its own copies of the server configs (name-suffixed by slug),
-- every server runs on that workspace's host behind `rnvim lsp-proxy`.

local rpc = require("rnvim.rpc")
local workspaces = require("rnvim.workspaces")

local M = {}

local servers = {
  gopls = {
    cmd = { "gopls" },
    filetypes = { "go", "gomod", "gowork", "gotmpl" },
    markers = { "go.work", "go.mod", ".git" },
  },
  rust_analyzer = {
    cmd = { "rust-analyzer" },
    filetypes = { "rust" },
    markers = { "Cargo.toml", ".git" },
  },
  clangd = {
    cmd = { "clangd" },
    filetypes = { "c", "cpp", "objc", "objcpp" },
    markers = { "compile_commands.json", "compile_flags.txt", ".git" },
  },
  pyright = {
    cmd = { "pyright-langserver", "--stdio" },
    filetypes = { "python" },
    markers = { "pyproject.toml", "setup.py", "setup.cfg", "requirements.txt", ".git" },
  },
  ts_ls = {
    cmd = { "typescript-language-server", "--stdio" },
    filetypes = { "javascript", "javascriptreact", "typescript", "typescriptreact" },
    markers = { "tsconfig.json", "package.json", ".git" },
  },
  lua_ls = {
    cmd = { "lua-language-server" },
    filetypes = { "lua" },
    markers = { ".luarc.json", ".luarc.jsonc", ".git" },
  },
}

local rnvim_bin
local which_cache = {}
local warned = {}

local function is_null(v)
  return v == nil or v == vim.NIL
end

local function server_available(bin, host)
  local key = host .. ":" .. bin
  if which_cache[key] == nil then
    local ok, res = pcall(rpc.request, host, "exec.which", { name = bin })
    which_cache[key] = ok and not is_null(res.path)
  end
  if not which_cache[key] and not warned[key] then
    warned[key] = true
    vim.notify(
      ("[rnvim] %s not found on %s — install it there to enable LSP"):format(bin, host),
      vim.log.levels.WARN
    )
  end
  return which_cache[key]
end

--- Register + enable the server set for one workspace.
function M.register_workspace(ws)
  if ws.lsp_registered then
    return
  end
  ws.lsp_registered = true
  if not rnvim_bin or rnvim_bin == "" then
    vim.notify("[rnvim] RNVIM_BIN not set; LSP disabled", vim.log.levels.WARN)
    return
  end

  local suffix = ws.slug:gsub("[^%w_]", "_")
  local names = {}
  for name, def in pairs(servers) do
    local proxy_cmd = { rnvim_bin, "lsp-proxy", "--host", ws.host, "--ws-root", ws.ws_root, "--" }
    vim.list_extend(proxy_cmd, def.cmd)
    local cfg_name = ("%s_%s"):format(name, suffix)
    names[#names + 1] = cfg_name

    vim.lsp.config(cfg_name, {
      cmd = proxy_cmd,
      filetypes = def.filetypes,
      root_dir = function(bufnr, on_dir)
        local file = vim.api.nvim_buf_get_name(bufnr)
        if workspaces.of_file(file) ~= ws then
          return -- another workspace's copy will pick this buffer up
        end
        if not server_available(def.cmd[1], ws.host) then
          return
        end
        local remote = workspaces.remote_path(file, ws)
        local root
        local ok, res =
          pcall(rpc.request, ws.host, "fs.findroot", { path = remote, markers = def.markers })
        if ok and not is_null(res.root) then
          root = res.root
        else
          root = vim.fs.dirname(remote)
        end
        on_dir(ws.ws_root .. root)
      end,
      capabilities = {
        workspace = {
          didChangeWatchedFiles = { dynamicRegistration = false },
        },
      },
    })
  end
  vim.lsp.enable(names)
end

function M.setup(opts)
  rnvim_bin = opts.rnvim_bin

  -- Definition-jump keymaps nvim does not ship by default (gd is the old
  -- "local declaration" motion). grr/gri/grn/gra/K/CTRL-] are built-ins.
  vim.api.nvim_create_autocmd("LspAttach", {
    group = vim.api.nvim_create_augroup("RnvimLspKeymaps", { clear = true }),
    callback = function(ev)
      local function map(lhs, fn, desc)
        vim.keymap.set("n", lhs, fn, { buffer = ev.buf, desc = "rnvim: " .. desc })
      end
      map("gd", vim.lsp.buf.definition, "go to definition")
      map("gD", vim.lsp.buf.declaration, "go to declaration")
      map("gy", vim.lsp.buf.type_definition, "go to type definition")
    end,
  })
end

return M
