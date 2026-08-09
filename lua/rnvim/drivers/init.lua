-- Multiplexer / terminal drivers. A driver needs only two operations —
-- the session registry (sessions.lua) answers "what is open", so no query
-- API is ever required of the host program:
--
--   spawn(name, target) → handle?, err   open a new window/tab running a
--                                        workspace instance for `target`;
--                                        return a focus handle (nil when
--                                        the host can't focus by handle)
--   focus(handle) → ok, err              bring an existing window to the front
--   self_handle() → handle?              this instance's own focus handle
--
-- Detection is by environment; `vim.g.rnvim_driver` overrides. True
-- multiplexers (tmux/screen/zellij/herdr) are preferred over plain
-- terminals (kitty/ghostty/warp) when nested, because they keep sessions
-- alive across terminal restarts.

local M = {}

local function detect()
  local override = vim.g.rnvim_driver
  if type(override) == "string" and override ~= "" then
    return override
  end
  local env = vim.env
  if env.TMUX and env.TMUX ~= "" then
    return "tmux"
  end
  if env.STY and env.STY ~= "" then
    return "screen"
  end
  if env.ZELLIJ and env.ZELLIJ ~= "" then
    return "zellij"
  end
  if env.HERDR_SOCKET_PATH and env.HERDR_SOCKET_PATH ~= "" then
    return "herdr"
  end
  if env.KITTY_WINDOW_ID and env.KITTY_WINDOW_ID ~= "" then
    return "kitty"
  end
  if (env.TERM_PROGRAM or ""):lower() == "ghostty" or env.GHOSTTY_RESOURCES_DIR then
    return "ghostty"
  end
  if env.TERM_PROGRAM == "WarpTerminal" then
    return "warp"
  end
  return "none"
end

function M.name()
  return detect()
end

function M.get()
  return require("rnvim.drivers." .. detect())
end

-- exposed for tests
M._detect = detect

return M
