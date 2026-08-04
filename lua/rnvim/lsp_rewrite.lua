-- Path rewriting between the local workspace prefix and remote paths
-- (a Lua port of the old lsp-proxy binary's rules, applied to decoded
-- message tables instead of Content-Length byte frames):
--
--   nvim → server: `file://<ws_root>/x` → `file:///x`, `<ws_root>/x` → `/x`
--   server → nvim: `file:///x` → `file://<ws_root>/x`, and plain strings
--                  starting with the remote workspace root get prefixed
--
-- The remote workspace root needed for the reverse plain-path rule is
-- captured from the `initialize` request after rewriting.
--
-- Known limitation (inherited, documented): string values that merely
-- *contain* a path mid-string are not rewritten; only values that start at
-- a path boundary are. Percent-encoded URIs are not handled.

local M = {}

--- True if `rest` continues a path cleanly after a prefix match, so that
--- ws_root `/a/b` never matches `/a/bc`.
local function boundary(rest)
  return rest == "" or vim.startswith(rest, "/")
end

local function strip_prefix(s, prefix)
  if vim.startswith(s, prefix) then
    return s:sub(#prefix + 1)
  end
  return nil
end

--- Rewrite one string value in the nvim→server direction, or nil.
function M.to_remote(ws_root, s)
  local uri_rest = strip_prefix(s, "file://" .. ws_root)
  if uri_rest and boundary(uri_rest) then
    return uri_rest == "" and "file:///" or ("file://" .. uri_rest)
  end
  local rest = strip_prefix(s, ws_root)
  if rest and boundary(rest) then
    return rest == "" and "/" or rest
  end
  return nil
end

--- Rewrite one string value in the server→nvim direction, or nil.
function M.to_local(ws_root, remote_root, s)
  local uri_rest = strip_prefix(s, "file://")
  if uri_rest and vim.startswith(uri_rest, "/") then
    return "file://" .. ws_root .. uri_rest
  end
  if remote_root then
    local rest = strip_prefix(s, remote_root)
    if rest and boundary(rest) then
      return ws_root .. s
    end
  end
  return nil
end

--- Deep-rewrite every string value of `v` with `f`, returning a new value
--- (never mutates — nvim's LSP client may hold references to the tables).
function M.rewrite(v, f)
  local t = type(v)
  if t == "string" then
    return f(v) or v
  end
  if t ~= "table" then
    return v
  end
  local out = {}
  for k, item in pairs(v) do
    out[k] = M.rewrite(item, f)
  end
  return setmetatable(out, getmetatable(v))
end

--- Extract the remote workspace root from an already-rewritten
--- `initialize` params table (rootUri preferred, first workspaceFolder as
--- fallback), or nil.
function M.capture_remote_root(params)
  if type(params) ~= "table" then
    return nil
  end
  local function from_uri(u)
    return type(u) == "string" and strip_prefix(u, "file://") or nil
  end
  local root = from_uri(params.rootUri)
  if root then
    return root
  end
  local folders = params.workspaceFolders
  if type(folders) == "table" and type(folders[1]) == "table" then
    return from_uri(folders[1].uri)
  end
  return nil
end

return M
