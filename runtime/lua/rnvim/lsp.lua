-- First-party LSP integration: every server runs on the remote host behind
-- `rnvim lsp-proxy` (prefix path rewriting), root detection and availability
-- probing go through the agent.

local rpc = require("rnvim.rpc")

local M = {}

-- Built-in server set. Deliberately small and first-party (no lspconfig
-- dependency); each entry is cmd + filetypes + remote root markers.
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
local warned = {}

local function is_null(v)
  return v == nil or v == vim.NIL
end

local function server_available(bin, host)
  if which_cache[bin] == nil then
    local ok, res = pcall(rpc.request, "exec.which", { name = bin })
    which_cache[bin] = ok and not is_null(res.path)
  end
  if not which_cache[bin] and not warned[bin] then
    warned[bin] = true
    vim.notify(
      ("[rnvim] %s not found on %s — install it there to enable LSP"):format(bin, host),
      vim.log.levels.WARN
    )
  end
  return which_cache[bin]
end

function M.setup(opts)
  if not opts.rnvim_bin or opts.rnvim_bin == "" then
    vim.notify("[rnvim] RNVIM_BIN not set; LSP disabled", vim.log.levels.WARN)
    return
  end

  for name, def in pairs(servers) do
    local proxy_cmd = { opts.rnvim_bin, "lsp-proxy", "--host", opts.host, "--ws-root", opts.ws_root, "--" }
    vim.list_extend(proxy_cmd, def.cmd)

    vim.lsp.config(name, {
      cmd = proxy_cmd,
      filetypes = def.filetypes,
      root_dir = function(bufnr, on_dir)
        if not server_available(def.cmd[1], opts.host) then
          return
        end
        local file = vim.api.nvim_buf_get_name(bufnr)
        if not vim.startswith(file, opts.ws_root) then
          return
        end
        local remote = file:sub(#opts.ws_root + 1)
        local root
        local ok, res = pcall(rpc.request, "fs.findroot", { path = remote, markers = def.markers })
        if ok and not is_null(res.root) then
          root = res.root
        else
          root = vim.fs.dirname(remote)
        end
        on_dir(opts.ws_root .. root)
      end,
      capabilities = {
        workspace = {
          -- Local file watching would watch the (empty) local prefix; the
          -- remote watcher lands with the QUIC transport milestone.
          didChangeWatchedFiles = { dynamicRegistration = false },
        },
      },
    })
  end

  vim.lsp.enable(vim.tbl_keys(servers))

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
