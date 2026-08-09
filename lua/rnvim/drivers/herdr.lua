-- herdr driver: one workspace per herdr tab, driven over herdr's socket
-- API (herdr exports HERDR_SOCKET_PATH / HERDR_TAB_ID to its panes).
--
-- `herdr tab create` starts the tab's default shell (it cannot exec a
-- command directly), so the nvim invocation is injected with
-- pane send-text + send-keys enter; the target still travels via --env,
-- never through shell-quoted interpolation.

local util = require("rnvim.util")

local M = {}

local function api(args)
  local res = vim.system(vim.list_extend({ "herdr" }, args)):wait()
  if res.code ~= 0 then
    return nil, (res.stderr or ("herdr " .. args[1] .. " failed")):gsub("%s+$", "")
  end
  local out = vim.trim(res.stdout or "")
  if out == "" then
    return {} -- some commands (send-text, send-keys) are silent on success
  end
  local ok, msg = pcall(vim.json.decode, out)
  if not ok then
    return nil, "unexpected herdr output: " .. out
  end
  return msg
end

function M.spawn(name, target)
  local created, err =
    api({ "tab", "create", "--label", name, "--env", "RNVIM_TARGET=" .. target, "--focus" })
  if not created then
    return nil, err
  end
  local result = created.result or {}
  local tab_id = result.tab and result.tab.tab_id
  local pane_id = result.root_pane and result.root_pane.pane_id
  if not tab_id or not pane_id then
    return nil, "herdr tab create returned no tab/pane id"
  end

  local _, terr = api({ "pane", "send-text", pane_id, "exec nvim --cmd '" .. util.BOOT_CMD .. "'" })
  if terr then
    return nil, terr
  end
  local _, kerr = api({ "pane", "send-keys", pane_id, "enter" })
  if kerr then
    return nil, kerr
  end
  return tab_id
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no herdr tab handle"
  end
  local msg, err = api({ "tab", "focus", handle })
  if not msg then
    return false, err
  end
  return true
end

function M.self_handle()
  local id = vim.env.HERDR_TAB_ID
  return (id and id ~= "") and id or nil
end

return M
