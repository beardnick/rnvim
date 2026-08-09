-- Differential LSP test: run the SAME editing session against a native
-- buffer and a virtual-fs buffer, each attached to the stub language
-- server, and compare the recorded traffic. The native buffer is the
-- oracle: whatever notifications a normal file's lifecycle produces, the
-- workspace buffer must produce too — same methods, same order, same
-- document text — with URIs rewritten to REMOTE paths on the virtual
-- side. This is the regression net for the whole event-plane bug class
-- (a missing didSave here is exactly the "no diagnostics on save" bug).
--
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/lsp_compare_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")
local boot = dofile(here .. "/boot.lua")
local STUB = here .. "/../stub_ls.lua"

local CONTENT = { "line one", "line two" }
local EDITED = { "line one EDITED", "line two" }

--- Attach the stub to `bufnr` and run the shared scenario: open (didOpen
--- fires on attach), edit, save. Returns once didSave has been recorded.
local function drive(bufnr, client_cfg, logfile)
  local client_id = vim.lsp.start(client_cfg, { bufnr = bufnr })
  T.truthy(client_id, "lsp client started")
  T.eventually(10000, function()
    local c = vim.lsp.get_client_by_id(client_id)
    return c and c.initialized
  end, "client initialized")

  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, EDITED)
  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd.write()
  end)

  T.eventually(10000, function()
    local f = io.open(logfile, "r")
    if not f then
      return false
    end
    local text = f:read("*a")
    f:close()
    return text:find("didSave", 1, true) and text
  end, "didSave recorded in " .. logfile)
end

--- Parse the stub's log into a normalized trace: keep textDocument/*
--- notifications FOR THIS SIDE'S DOCUMENT (URI under `root`), stripped to
--- a workspace-relative path so both sides compare directly.
---
--- The filter is load-bearing, and its need was this test's first
--- discovery: nvim's changetracking fans textDocument/didSave out to
--- every client in the same sync group (sync kind + offset encoding)
--- regardless of which buffer each client is attached to
--- (_changetracking._send_did_save iterates the group's clients, not the
--- buffer's). Real servers silently ignore notifications for unknown
--- documents, so cross-client leakage is invisible in practice — but a
--- traffic-recording stub sees it.
local function trace(logfile, root)
  -- nvim resolves buffer paths (macOS: /var → /private/var), tempname()
  -- does not; match URIs against both spellings of the root
  local roots = { root }
  local real = vim.uv.fs_realpath(root)
  if real and real ~= root then
    roots[#roots + 1] = real
  end
  local entries = {}
  for line in io.lines(logfile) do
    local e = vim.json.decode(line)
    if e.method:find("^textDocument/") and e.uri then
      for _, r in ipairs(roots) do
        local rel = e.uri:match("^file://" .. vim.pesc(r) .. "(/.*)$")
        if rel then
          entries[#entries + 1] = { method = e.method, rel = rel, text = e.text }
          break
        end
      end
    end
  end
  return entries
end

T.scenario("lsp traffic parity: native vs virtual fs", {
  { "boot the remote personality", function(ctx)
    local b = boot()
    ctx.workdir, ctx.ws = b.workdir, b.ws
    -- the same document exists on both sides, at the same relative path
    ctx.native_root = vim.fn.tempname()
    vim.fn.mkdir(ctx.native_root, "p")
    for _, root in ipairs({ ctx.native_root, ctx.workdir }) do
      local f = assert(io.open(root .. "/doc.txt", "w"))
      f:write(table.concat(CONTENT, "\n") .. "\n")
      f:close()
    end
    ctx.logs = { native = vim.fn.tempname(), virtual = vim.fn.tempname() }
  end },

  { "drive the scenario on a NATIVE buffer (the oracle)", function(ctx)
    vim.cmd.edit(ctx.native_root .. "/doc.txt")
    drive(vim.api.nvim_get_current_buf(), {
      name = "stub_native",
      cmd = { vim.v.progpath, "-l", STUB, ctx.logs.native },
      root_dir = ctx.native_root,
    }, ctx.logs.native)
  end },

  { "drive the same scenario on a VIRTUAL buffer through the transport", function(ctx)
    vim.cmd.edit(ctx.ws.ws_root .. ctx.workdir .. "/doc.txt")
    drive(vim.api.nvim_get_current_buf(), {
      name = "stub_virtual",
      cmd = require("rnvim.lsp_transport").cmd(
        "local",
        ctx.ws.ws_root,
        { vim.v.progpath, "-l", STUB, ctx.logs.virtual }
      ),
      root_dir = ctx.ws.ws_root .. ctx.workdir,
    }, ctx.logs.virtual)
  end },

  { "traces match: same methods, same order, same text", function(ctx)
    ctx.native = trace(ctx.logs.native, ctx.native_root)
    -- the stub behind the transport sees REMOTE uris: strip the remote root
    ctx.virtual = trace(ctx.logs.virtual, ctx.workdir)

    local function methods(t)
      return vim.tbl_map(function(e)
        return e.method
      end, t)
    end
    T.eq(methods(ctx.virtual), methods(ctx.native), "notification sequence")
    for i, native in ipairs(ctx.native) do
      local virt = ctx.virtual[i]
      T.eq(virt.rel, native.rel, ("uri (workspace-relative) at #%d %s"):format(i, native.method))
      T.eq(virt.text, native.text, ("document text at #%d %s"):format(i, native.method))
    end
  end },

  { "virtual uris were rewritten to remote paths (not the ws prefix)", function(ctx)
    for line in io.lines(ctx.logs.virtual) do
      T.truthy(not line:find(ctx.ws.ws_root, 1, true), "ws prefix leaked to the server: " .. line)
    end
  end },

  { "the didSave the virtual fs used to swallow is present", function(ctx)
    local saves = vim.tbl_filter(function(e)
      return e.method == "textDocument/didSave"
    end, ctx.virtual)
    T.truthy(#saves >= 1, "didSave present")
    T.contains(saves[#saves].text or "", "EDITED", "didSave carries the saved text")
  end },
})
