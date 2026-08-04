-- tmux driver: one workspace per tmux window.

local M = {}

--- The exact nvim invocation a new workspace instance boots with. The
--- target reaches the new instance through an environment variable, not
--- string-interpolated Lua, so no quoting of user input is required.
function M.instance_cmd()
  return [[nvim --cmd 'lua vim.g.rnvim = { target = vim.env.RNVIM_TARGET }']]
end

function M.spawn(slug, target)
  local res = vim
    .system({
      "tmux",
      "new-window",
      "-n",
      slug,
      "-P",
      "-F",
      "#{window_id}",
      "-e",
      "RNVIM_TARGET=" .. target,
      M.instance_cmd(),
    })
    :wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "tmux new-window failed"):gsub("%s+$", "")
  end
  return vim.trim(res.stdout or "")
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no tmux window handle"
  end
  local res = vim.system({ "tmux", "select-window", "-t", handle }):wait()
  if res.code ~= 0 then
    return false, (res.stderr or "tmux select-window failed"):gsub("%s+$", "")
  end
  return true
end

--- Handle of the window this instance runs in (registered at connect so
--- other instances can focus us).
function M.self_handle()
  local res = vim.system({ "tmux", "display-message", "-p", "#{window_id}" }):wait()
  if res.code ~= 0 then
    return nil
  end
  return vim.trim(res.stdout or "")
end

return M
