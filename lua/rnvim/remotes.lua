-- Remote target discovery: ssh-config host aliases (with non-glob
-- Includes) and recently opened workspaces (~/.rnvim/recent.json).

local util = require("rnvim.util")

local M = {}

local function recent_file()
  return util.home("recent.json")
end

function M.load_recent()
  return util.read_json(recent_file()) or {}
end

--- Remember a successfully opened workspace (most recent first, deduped,
--- capped at 50).
function M.record_recent(host, path)
  if host == "local" then
    return
  end
  local entries = M.load_recent()
  local kept = {}
  for _, e in ipairs(entries) do
    if not (e.host == host and e.path == path) then
      kept[#kept + 1] = e
    end
  end
  table.insert(kept, 1, { host = host, path = path, ts = os.time() })
  while #kept > 50 do
    table.remove(kept)
  end
  util.write_json(recent_file(), kept)
end

local function collect_hosts(path, home, out, depth)
  if depth > 3 then
    return
  end
  local f = io.open(path, "r")
  if not f then
    return
  end
  local text = f:read("*a")
  f:close()
  for line in text:gmatch("[^\n]+") do
    line = vim.trim(line)
    if line ~= "" and not vim.startswith(line, "#") then
      local words = vim.split(line, "%s+", { trimempty = true })
      local key = (words[1] or ""):lower()
      if key == "host" then
        for i = 2, #words do
          local alias = words[i]
          if not alias:find("[*?!]") then
            out[#out + 1] = alias
          end
        end
      elseif key == "include" then
        for i = 2, #words do
          local inc = words[i]
          if not inc:find("*", 1, true) then -- glob includes unsupported
            local p
            if vim.startswith(inc, "~/") then
              p = vim.fs.joinpath(home, inc:sub(3))
            elseif vim.startswith(inc, "/") then
              p = inc
            else
              p = vim.fs.joinpath(home, ".ssh", inc)
            end
            collect_hosts(p, home, out, depth + 1)
          end
        end
      end
    end
  end
end

--- Host aliases from ~/.ssh/config; wildcard patterns are configuration,
--- not targets.
function M.ssh_hosts()
  local home = vim.uv.os_homedir()
  local hosts = {}
  collect_hosts(vim.fs.joinpath(home, ".ssh", "config"), home, hosts, 0)
  local seen, out = {}, {}
  for _, h in ipairs(hosts) do
    if not seen[h] then
      seen[h] = true
      out[#out + 1] = h
    end
  end
  return out
end

-- exposed for tests
M._collect_hosts = collect_hosts

return M
