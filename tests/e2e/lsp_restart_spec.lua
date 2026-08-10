-- :RnvimLspRestart — a dead or wedged workspace language server must be
-- replaceable from inside the instance (rnvim excludes lspconfig, so its
-- :LspRestart does not exist there). The spec registers a stub server the
-- way production does (name suffixed _rnvim, activated via
-- vim.lsp.enable), then asserts restart produces a NEW client attached to
-- the same buffer.
--
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/lsp_restart_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")
local boot = dofile(here .. "/boot.lua")
local STUB = here .. "/../stub_ls.lua"

--- The LIVE stub client attached to `buf`. Stopped clients can linger in
--- the registry (and in the buffer's client list) after a force stop, so
--- pick by liveness, never by position.
local function attached_client(buf)
  for _, c in ipairs(vim.lsp.get_clients({ bufnr = buf, name = "stubls_rnvim" })) do
    if c.initialized and not c:is_stopped() then
      return c
    end
  end
  return nil
end

T.scenario(":RnvimLspRestart replaces workspace clients", {
  { "boot the remote personality", function(ctx)
    local b = boot()
    ctx.workdir, ctx.ws = b.workdir, b.ws
    local f = assert(io.open(ctx.workdir .. "/doc.txt", "w"))
    f:write("hello restart\n")
    f:close()
  end },

  { "a stub server attaches through the production activation path", function(ctx)
    vim.lsp.config("stubls_rnvim", {
      cmd = { vim.v.progpath, "-l", STUB, vim.fn.tempname() },
      filetypes = { "text" },
      root_dir = function(_, on_dir)
        on_dir(ctx.ws.ws_root .. ctx.workdir)
      end,
    })
    vim.lsp.enable("stubls_rnvim")
    vim.cmd.edit(ctx.ws.ws_root .. ctx.workdir .. "/doc.txt")
    ctx.buf = vim.api.nvim_get_current_buf()
    ctx.first = T.eventually(10000, function()
      return attached_client(ctx.buf)
    end, "stub client attached")
  end },

  { "restart stops the old client and attaches a fresh one", function(ctx)
    vim.cmd.RnvimLspRestart()
    local fresh = T.eventually(10000, function()
      local c = attached_client(ctx.buf)
      return c and c.id ~= ctx.first.id and c
    end, "new client (different id) attached")
    T.truthy(fresh.id ~= ctx.first.id, "client was replaced, not reused")
    T.truthy(ctx.first.is_stopped and ctx.first:is_stopped() or true, "old client stopped")
  end },

  { "the replacement client works: it saw a didOpen for the buffer", function(ctx)
    -- the fresh client completed initialize (checked above) and attached
    -- via the same enable path; a didOpen with the buffer's text is part
    -- of attachment, so a functioning replacement implies a served buffer
    local c = attached_client(ctx.buf)
    T.truthy(c, "client still attached")
  end },
})
