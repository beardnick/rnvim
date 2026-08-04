-- Fallback driver: no multiplexer. Everything except spawn/focus works;
-- new workspaces are started from a shell instead.

local M = {}

function M.spawn(_, target)
  return nil,
    ("no multiplexer detected — open a terminal and run:  rnvim %s  (or start nvim inside tmux)"):format(target)
end

function M.focus(_)
  return false, "no multiplexer detected — switch to the instance's terminal yourself"
end

function M.self_handle()
  return nil
end

return M
