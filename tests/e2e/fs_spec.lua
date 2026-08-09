-- Virtual filesystem e2e over the local: loopback (the whole stack minus
-- ssh): read, write-to-disk, finder, session registry, directory buffer.
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/fs_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")
local boot = dofile(here .. "/boot.lua")

T.scenario("virtual fs", {
  { "boot the remote personality", function(ctx)
    local b = boot()
    ctx.workdir, ctx.ws = b.workdir, b.ws
  end },

  { "read an existing remote file through the virtual fs", function(ctx)
    vim.cmd.edit(ctx.ws.ws_root .. ctx.workdir .. "/hello.txt")
    T.eq(vim.api.nvim_buf_get_lines(0, 0, -1, false), { "hello from disk" })
  end },

  { "create and write a new remote file", function(ctx)
    vim.cmd.edit(ctx.ws.ws_root .. ctx.workdir .. "/sub/note.txt")
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { "written through rnvim" })
    vim.cmd.write()
    local f = assert(io.open(ctx.workdir .. "/sub/note.txt", "r"), "note.txt not created on disk")
    local content = f:read("*a")
    f:close()
    T.contains(content, "written through rnvim", "note.txt content")
  end },

  { "write fires the Pre/Post events BufWriteCmd swallows", function(ctx)
    local fired = {}
    local group = vim.api.nvim_create_augroup("E2eWriteEvents", { clear = true })
    for _, ev in ipairs({ "BufWritePre", "BufWritePost" }) do
      vim.api.nvim_create_autocmd(ev, {
        group = group,
        callback = function()
          fired[#fired + 1] = ev
        end,
      })
    end
    vim.bo.modified = true
    vim.cmd.write()
    vim.api.nvim_del_augroup_by_id(group)
    T.eq(fired, { "BufWritePre", "BufWritePost" }, "write events")
  end },

  { "remote finder sees the new file", function(ctx)
    local res = require("rnvim.rpc").request("find.files", { root = ctx.workdir, query = "note", limit = 10 })
    local found = vim.tbl_filter(function(f)
      return f:find("note.txt", 1, true) ~= nil
    end, res.files or {})
    T.truthy(#found > 0, "find.files returns note.txt: " .. vim.inspect(res))
  end },

  { "the session registered itself", function(ctx)
    local sessions = require("rnvim.sessions").list()
    T.eq(#sessions, 1, "registered session count")
    T.contains(sessions[1].target, ctx.workdir, "session target")
  end },

  { "directory buffer lists entries", function(ctx)
    vim.cmd.edit(ctx.ws.ws_root .. ctx.workdir)
    local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
    T.truthy(vim.list_contains(lines, "sub/"), "sub/ in dir listing: " .. vim.inspect(lines))
  end },
})
