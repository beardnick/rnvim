-- zellij driver: one workspace per tab. `zellij action new-tab` cannot run
-- a command directly, so each spawn goes through a generated one-tab
-- layout file; focus is by tab name (the only by-name navigation zellij
-- offers), so spawned tab names must stay unique.

local util = require("rnvim.util")

local M = {}

local function kdl_escape(s)
  return (s:gsub("\\", "\\\\"):gsub('"', '\\"'))
end

function M.spawn(name, target)
  local argv = util.instance_argv(target)
  local args = {}
  for i = 2, #argv do
    args[#args + 1] = ('"%s"'):format(kdl_escape(argv[i]))
  end
  local layout = ([[layout {
    tab name="%s" focus=true {
        pane command="%s" {
            args %s
        }
    }
}
]]):format(kdl_escape(name), kdl_escape(argv[1]), table.concat(args, " "))

  local dir = util.ensure_dir(util.home("tmp"))
  local file = vim.fs.joinpath(dir, ("zellij-%d.kdl"):format(vim.uv.os_getpid()))
  local f, err = io.open(file, "w")
  if not f then
    return nil, "cannot write zellij layout: " .. tostring(err)
  end
  f:write(layout)
  f:close()

  local res = vim.system({ "zellij", "action", "new-tab", "--layout", file, "--name", name }):wait()
  os.remove(file)
  if res.code ~= 0 then
    return nil, (res.stderr or "zellij new-tab failed"):gsub("%s+$", "")
  end
  return name
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no zellij tab handle"
  end
  local res = vim.system({ "zellij", "action", "go-to-tab-name", handle }):wait()
  if res.code ~= 0 then
    return false, (res.stderr or "zellij go-to-tab-name failed"):gsub("%s+$", "")
  end
  return true
end

function M.self_handle()
  return nil -- zellij has no "current tab name" query
end

return M
