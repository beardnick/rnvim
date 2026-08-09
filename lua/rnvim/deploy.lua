-- Remote agent deployment over ssh (a Lua port of the old deploy.rs).
--
-- Preference order:
-- 1. version-stamped agent already on the remote → run it
-- 2. prebuilt agent for the remote target, fetched from this version's
--    GitHub release into ~/.rnvim/dist/ (authenticated locally via `gh`
--    when available; anonymous curl once the repo is public) → push it
--
-- Artifacts are version-stamped filenames under ~/.rnvim/bin on the
-- remote, so a pure existence check is enough — plugin and agent can
-- never skew.

local util = require("rnvim.util")

local M = {}

local REPO = "beardnick/rnvim"

local function bin_path()
  return "$HOME/.rnvim/bin/rnvim-agent-" .. util.agent_version()
end

local function ssh_run(host, script, stdin_data)
  local res = vim
    .system({ "ssh", "-o", "BatchMode=yes", host, script }, { stdin = stdin_data, text = true })
    :wait()
  if res.code ~= 0 then
    error(("[rnvim] ssh to %s failed: %s"):format(host, (res.stderr or ""):gsub("%s+$", "")))
  end
  return res.stdout or ""
end

--- Download the prebuilt agent for `target`, caching it under
--- ~/.rnvim/dist/<version>/. Returns the local path.
local function fetch_agent_dist(target)
  local version = util.agent_version()
  local dist_dir = util.home("dist", version)
  local asset = "rnvim-agent-" .. target
  local path = vim.fs.joinpath(dist_dir, asset)
  if vim.uv.fs_stat(path) then
    return path
  end
  util.ensure_dir(dist_dir)
  local tag = "v" .. version

  if vim.fn.executable("gh") == 1 then
    local res = vim
      .system({ "gh", "release", "download", tag, "--repo", REPO, "--pattern", asset, "--dir", dist_dir })
      :wait()
    if res.code == 0 and vim.uv.fs_stat(path) then
      return path
    end
  end
  local url = ("https://github.com/%s/releases/download/%s/%s"):format(REPO, tag, asset)
  local res = vim.system({ "curl", "-fsSL", "-o", path .. ".part", url }):wait()
  if res.code ~= 0 or not vim.uv.fs_stat(path .. ".part") then
    error(
      ("[rnvim] no prebuilt agent %s for release %s — the release must be published (gh and curl both failed)"):format(
        asset,
        tag
      )
    )
  end
  assert(vim.uv.fs_rename(path .. ".part", path))
  return path
end

--- Agent binary for `local:` loopback sessions: an explicit override
--- (RNVIM_AGENT_BIN / vim.g.rnvim_agent_bin), else the dist cache.
function M.local_agent_bin()
  local target = util.rust_target(util.local_uname_sm())
  if not target then
    error("[rnvim] unsupported local platform: " .. util.local_uname_sm())
  end
  local path = fetch_agent_dist(target)
  vim.uv.fs_chmod(path, 493) -- 0755
  return path
end

local function push_agent_binary(host, local_file)
  local f = assert(io.open(local_file, "rb"))
  local data = f:read("*a")
  f:close()
  local bin = bin_path()
  ssh_run(
    host,
    ("mkdir -p $HOME/.rnvim/bin && cat > %s.tmp && chmod +x %s.tmp && mv %s.tmp %s"):format(bin, bin, bin, bin),
    data
  )
end

--- Make sure a compatible agent exists on `host`; return the shell command
--- that starts it (passed to ssh by the transport).
function M.ensure_remote_agent(host)
  local probe = ssh_run(host, ("uname -sm; test -x %s && echo bin=yes || echo bin=no"):format(bin_path()))
  local lines = vim.split(probe, "\n", { trimempty = true })
  local uname = vim.trim(lines[1] or "")
  if uname == "" then
    error("[rnvim] could not probe " .. host .. ": empty uname output")
  end
  local bin_cmd = bin_path() .. " --stdio"
  if vim.list_contains(lines, "bin=yes") then
    return bin_cmd
  end

  local target = util.rust_target(uname)
  if not target then
    error(
      ("[rnvim] remote %s is %s — no agent build exists for this platform (supported: Linux/macOS on x86_64/aarch64)"):format(
        host,
        uname
      )
    )
  end
  util.notify(("deploying agent (%s) to %s..."):format(target, host))
  push_agent_binary(host, fetch_agent_dist(target))
  return bin_cmd
end

return M
