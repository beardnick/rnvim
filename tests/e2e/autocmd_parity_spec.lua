-- Autocommand-trace parity: run the same file lifecycle (open → edit →
-- write → reload) on a NATIVE buffer and on a VIRTUAL-FS buffer, record
-- which autocommands fire and what option state each phase establishes,
-- and require the traces to be identical. The native buffer is the
-- oracle: rnvim's *Cmd seam must preserve the full observable contract
-- of a normal file, or an ecosystem of hooks (LSP didSave, format-on-
-- save, watchers, lens refreshers) silently dies — the event-plane bug
-- class this spec exists to catch systematically.
--
-- INTENDED DIVERGENCES (the whitelist — every entry needs a reason):
--   * BufReadCmd/BufWriteCmd are not in the catalog: they ARE the seam,
--     native buffers never fire them.
--   * 'swapfile' is not snapshotted: the virtual fs disables swap
--     deliberately (there is no local file to guard).
--   * Directory buffers are out of scope: the native oracle there is
--     netrw, a whole application — no meaningful contract to mirror.
--
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/autocmd_parity_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")
local boot = dofile(here .. "/boot.lua")

-- Every buffer-lifecycle event a plugin might reasonably hook. Growing
-- this list strengthens the net; the native side of the diff defines
-- what "correct" means for whatever is added.
local CATALOG = {
  "BufNew",
  "BufAdd",
  "BufReadPre",
  "BufReadPost",
  "FileType",
  "BufEnter",
  "BufWinEnter",
  "BufWritePre",
  "BufWritePost",
  "BufModifiedSet",
}

local function snapshot(buf)
  return {
    modified = vim.bo[buf].modified,
    endofline = vim.bo[buf].endofline,
    fixendofline = vim.bo[buf].fixendofline,
    filetype = vim.bo[buf].filetype,
    fileformat = vim.bo[buf].fileformat,
    buftype = vim.bo[buf].buftype,
  }
end

--- Run the lifecycle on `path`, recording catalog events for the buffer
--- it loads into. Returns { events, load, write, reload } where `events`
--- is the ordered list of event names and the rest are option snapshots.
local function run_lifecycle(path)
  vim.cmd.enew() -- leave from a scratch buffer so Enter/Leave noise is symmetric

  local record, target = {}, nil
  local group = vim.api.nvim_create_augroup("ParityRecorder", { clear = true })
  vim.api.nvim_create_autocmd(CATALOG, {
    group = group,
    callback = function(ev)
      record[#record + 1] = { event = ev.event, buf = ev.buf }
    end,
  })

  vim.cmd.edit(vim.fn.fnameescape(path))
  target = vim.api.nvim_get_current_buf()
  local load = snapshot(target)

  vim.api.nvim_buf_set_lines(target, 0, 0, false, { "added line" })
  vim.cmd.write()
  local write = snapshot(target)

  vim.cmd("edit!") -- reload from source of truth
  local reload = snapshot(target)
  T.eq(vim.api.nvim_buf_get_lines(target, 0, 1, false)[1], "added line", "reload sees the written content")

  vim.api.nvim_del_augroup_by_id(group)

  local events = {}
  for _, r in ipairs(record) do
    if r.buf == target then
      events[#events + 1] = r.event
    end
  end
  return { events = events, load = load, write = write, reload = reload }
end

T.scenario("autocmd-trace parity: native vs virtual fs", {
  { "boot the remote personality", function(ctx)
    local b = boot()
    ctx.workdir, ctx.ws = b.workdir, b.ws
    ctx.native_root = vim.fn.tempname()
    vim.fn.mkdir(ctx.native_root, "p")
    for _, root in ipairs({ ctx.native_root, ctx.workdir }) do
      local f = assert(io.open(root .. "/doc.txt", "w"))
      f:write("hello parity\n")
      f:close()
    end
  end },

  { "run the lifecycle on a NATIVE buffer (the oracle)", function(ctx)
    ctx.native = run_lifecycle(ctx.native_root .. "/doc.txt")
    -- sanity: the oracle itself must look like a normal file load
    T.truthy(vim.list_contains(ctx.native.events, "BufReadPost"), "oracle fired BufReadPost")
    T.truthy(vim.list_contains(ctx.native.events, "BufWritePost"), "oracle fired BufWritePost")
  end },

  { "run the same lifecycle on a VIRTUAL buffer", function(ctx)
    ctx.virtual = run_lifecycle(ctx.ws.ws_root .. ctx.workdir .. "/doc.txt")
  end },

  { "event traces are identical", function(ctx)
    T.eq(ctx.virtual.events, ctx.native.events, "autocmd sequence")
  end },

  { "option state matches at every phase", function(ctx)
    for _, phase in ipairs({ "load", "write", "reload" }) do
      T.eq(ctx.virtual[phase], ctx.native[phase], "snapshot after " .. phase)
    end
  end },

  { "the written content reached the remote disk", function(ctx)
    local f = assert(io.open(ctx.workdir .. "/doc.txt", "r"))
    local content = f:read("*a")
    f:close()
    T.contains(content, "added line", "remote file content")
  end },
})
