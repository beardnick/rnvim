-- Small shared helpers: paths, JSON files, shell quoting.

local M = {}

M.PROTO_VERSION = 5

function M.version()
  return require("rnvim.meta").version
end

--- Version of the agent this plugin deploys (decoupled from the plugin
--- version so Lua-only changes never force a release).
function M.agent_version()
  return require("rnvim.meta").agent_version
end

--- ~/.rnvim, created on demand.
function M.home(...)
  local p = vim.fs.joinpath(vim.uv.os_homedir(), ".rnvim", ...)
  return p
end

function M.ensure_dir(path)
  vim.fn.mkdir(path, "p")
  return path
end

function M.read_json(path)
  local f = io.open(path, "r")
  if not f then
    return nil
  end
  local text = f:read("*a")
  f:close()
  local ok, data = pcall(vim.json.decode, text)
  return ok and data or nil
end

function M.write_json(path, data)
  M.ensure_dir(vim.fs.dirname(path))
  local f, err = io.open(path, "w")
  if not f then
    return nil, err
  end
  f:write(vim.json.encode(data))
  f:close()
  return true
end

--- POSIX single-quote escaping (matches the Rust client's shell_quote).
function M.shell_quote(s)
  return "'" .. s:gsub("'", [['\'']]) .. "'"
end

--- Parse `[user@]host[:path]`; host "local" is the loopback pseudo-host.
function M.parse_target(s)
  local host, path = s:match("^([^:]+):(.*)$")
  if not host then
    host, path = s, ""
  end
  return { host = host, path = path, is_local = host == "local" }
end

--- Directory-name-safe slug for the workspace prefix.
function M.host_slug(host)
  return (host:gsub("[:/]", "_"))
end

--- Local platform in `uname -sm` terms.
function M.local_uname_sm()
  local sys = vim.uv.os_uname()
  local arch = sys.machine
  if sys.sysname == "Darwin" and arch == "aarch64" then
    arch = "arm64"
  end
  return sys.sysname .. " " .. arch
end

--- Rust target triple for a `uname -sm` string, matching release assets.
function M.rust_target(uname_sm)
  local map = {
    ["Linux x86_64"] = "x86_64-unknown-linux-musl",
    ["Linux aarch64"] = "aarch64-unknown-linux-musl",
    ["Darwin arm64"] = "aarch64-apple-darwin",
    ["Darwin x86_64"] = "x86_64-apple-darwin",
  }
  return map[uname_sm]
end

--- The nvim invocation every driver boots a workspace instance with. The
--- target travels via the RNVIM_TARGET environment variable, never by
--- string-interpolating user input into Lua.
M.BOOT_CMD = "lua vim.g.rnvim = { target = vim.env.RNVIM_TARGET }"

--- argv for a new workspace instance (drivers that take a command list).
function M.instance_argv(target)
  return { "env", "RNVIM_TARGET=" .. target, "nvim", "--cmd", M.BOOT_CMD }
end

--- Human window/tab name for a target: host slug plus the path's basename.
function M.window_name(target)
  local t = M.parse_target(target)
  local name = M.host_slug(t.host)
  local base = t.path:gsub("/+$", ""):match("([^/]+)$")
  if base and base ~= "" and base ~= "~" then
    name = name .. ":" .. base
  end
  return name
end

function M.notify(msg, level)
  vim.schedule(function()
    vim.notify("[rnvim] " .. msg, level or vim.log.levels.INFO)
  end)
end

return M
