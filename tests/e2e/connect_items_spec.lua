-- The connect picker's candidate list: a host that already has recent
-- workspaces must STILL be listed bare — the bare entry is the only way
-- to open a NEW directory on that host from inside the editor (it runs
-- the directory-selection stage in the spawned instance). Regression net
-- for the inherited de-duplication that suppressed exactly that entry.
--
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/connect_items_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")

T.scenario("connect picker candidates", {
  { "seed recents and an ssh config in the isolated HOME", function()
    local home = vim.uv.os_homedir()
    vim.fn.mkdir(home .. "/.ssh", "p")
    local f = assert(io.open(home .. "/.ssh/config", "w"))
    f:write("Host home\nHost dev-box\n")
    f:close()
    require("rnvim.remotes").record_recent("home", "/data/proj")
    require("rnvim.remotes").record_recent("home", "/data/other")
  end },

  { "a host with recents is still listed bare, after its recents", function()
    local items = require("rnvim.picker")._connect_items()
    local targets = vim.tbl_map(function(i)
      return i.target
    end, items)
    T.eq(targets, {
      "home:/data/other", -- most recent first
      "home:/data/proj",
      "home", -- bare entry NOT suppressed by the recents above
      "dev-box",
    }, "candidate order")
  end },
})
