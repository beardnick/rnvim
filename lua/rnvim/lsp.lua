-- First-party LSP integration: every server runs on the workspace host
-- through the pure-Lua rewriting transport (lsp_transport). Configs are
-- registered under `<name>_rnvim` so a user config loaded alongside can
-- still define the standard names for local use; anything the user
-- registered under the standard name flows into the rnvim variant.

local rpc = require("rnvim.rpc")
local workspace = require("rnvim.workspace")

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

local which_cache = {}
local installing = {}

local function is_null(v)
  return v == nil or v == vim.NIL
end

--- Install a missing server on the workspace host. A user recipe (full-
--- control script) runs directly via exec.run; otherwise the registry
--- module orchestrates: mason-registry plan resolved locally, artifact
--- fetched by the agent's native HTTP client on the remote, then unpacked
--- by a network-free script. One attempt per binary; attach is re-
--- triggered on success.
local function try_install(bin, host, bufnr)
  if installing[bin] then
    return
  end
  installing[bin] = true

  local function finish(err, path)
    if err then
      vim.notify(("[rnvim] could not install %s on %s: %s"):format(bin, host, err), vim.log.levels.WARN)
      return
    end
    which_cache[bin] = true
    vim.notify(("[rnvim] %s installed on %s (%s)"):format(bin, host, path or "?"))
    if bufnr and vim.api.nvim_buf_is_valid(bufnr) then
      -- Re-run the FileType machinery so vim.lsp.enable attaches now.
      vim.api.nvim_exec_autocmds("FileType", { buffer = bufnr })
    end
  end

  vim.notify(("[rnvim] installing %s on %s (first use)..."):format(bin, host))
  local user_script = require("rnvim.recipes").user_script(bin)
  if user_script then
    rpc.request_async("exec.run", { script = user_script }, function(err, res)
      if err or res.code ~= 0 then
        finish(err or (res.stderr:match("([^\n]+)%s*$") or ("exit " .. res.code)))
      else
        finish(nil, res.stdout:match("([^\n]+)%s*\n?$"))
      end
    end)
    return
  end

  require("rnvim.registry").install(bin, finish)
end

local function server_available(bin, host, bufnr)
  if which_cache[bin] == nil then
    local ok, res = pcall(rpc.request, "exec.which", { name = bin })
    which_cache[bin] = ok and not is_null(res.path)
  end
  if not which_cache[bin] then
    try_install(bin, host, bufnr)
  end
  return which_cache[bin]
end

--- Register + enable the server set for the instance's workspace.
function M.register_workspace(ws)
  if ws.lsp_registered then
    return
  end
  ws.lsp_registered = true

  local transport = require("rnvim.lsp_transport")
  local names = {}
  for name, def in pairs(servers) do
    local cfg_name = name .. "_rnvim"
    names[#names + 1] = cfg_name

    -- Anything the user registered under the STANDARD server name — via
    -- vim.lsp.config("gopls", { settings = ... }) or an lsp/gopls.lua on
    -- their rtp — flows into the rnvim variant: settings, handlers,
    -- init_options... cmd/root_dir/capabilities stay rnvim's.
    local ok_base, user_base = pcall(function()
      return vim.lsp.config[name]
    end)
    if ok_base and type(user_base) == "table" then
      local inherit = vim.deepcopy(user_base)
      inherit.cmd = nil
      inherit.root_dir = nil
      inherit.root_markers = nil
      inherit.filetypes = nil
      vim.lsp.config(cfg_name, inherit)
    end

    vim.lsp.config(cfg_name, {
      cmd = transport.cmd(ws.host, ws.ws_root, def.cmd),
      filetypes = def.filetypes,
      root_dir = function(bufnr, on_dir)
        local file = vim.api.nvim_buf_get_name(bufnr)
        if not workspace.of_file(file) then
          return -- a local buffer in a remote instance: leave it alone
        end
        if not server_available(def.cmd[1], ws.host, bufnr) then
          return
        end
        local remote = workspace.remote_path(file)
        local root
        local ok, res = pcall(rpc.request, "fs.findroot", { path = remote, markers = def.markers })
        if ok and not is_null(res.root) then
          root = res.root
        else
          root = vim.fs.dirname(remote)
        end
        on_dir(ws.ws_root .. root)
      end,
      -- NOTE: passing capabilities REPLACES nvim's defaults, so always
      -- start from make_client_capabilities() — losing the defaults kills
      -- workDoneProgress (fidget's progress UI), snippets, inlay hints...
      capabilities = (function()
        local caps = vim.lsp.protocol.make_client_capabilities()
        -- If the user's config brought a completion engine, layer its
        -- capabilities on top for the remote servers too.
        local ok, blink = pcall(require, "blink.cmp")
        if ok and blink.get_lsp_capabilities then
          caps = blink.get_lsp_capabilities(caps)
        end
        return vim.tbl_deep_extend("force", caps, {
          workspace = {
            didChangeWatchedFiles = { dynamicRegistration = false },
          },
        })
      end)(),
    })
  end
  vim.lsp.enable(names)
end

function M.setup()
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
