-- rust-analyzer frontend for the generic remote debugging engine
-- (rnvim.dap): translate a debugSingle code-lens runnable into a cargo
-- build script + codelldb launch.

local util = require("rnvim.util")
local workspace = require("rnvim.workspace")

local M = {}

local function is_null(v)
  return v == nil or v == vim.NIL
end

--- `cd <root> && cargo <args> --no-run --message-format=json`, quoted.
local function build_script(args, remote_root)
  local cmd = { "cargo" }
  vim.list_extend(cmd, args.cargoArgs or {})
  vim.list_extend(cmd, args.cargoExtraArgs or {})
  if cmd[2] == "run" then
    cmd[2] = "build"
  else
    cmd[#cmd + 1] = "--no-run"
  end
  cmd[#cmd + 1] = "--message-format=json"

  local quoted = {}
  for _, a in ipairs(cmd) do
    quoted[#quoted + 1] = util.shell_quote(a)
  end
  return ("cd %s && %s"):format(util.shell_quote(remote_root), table.concat(quoted, " "))
end

--- The produced binary: last artifact message with a non-null executable.
local function parse_executable(stdout)
  local exe
  for line in stdout:gmatch("[^\n]+") do
    local ok, msg = pcall(vim.json.decode, line)
    if ok and type(msg) == "table" and not is_null(msg.executable) then
      exe = msg.executable
    end
  end
  return exe
end

--- Debug a rust-analyzer runnable (the debugSingle code-lens payload)
--- inside the instance's remote workspace.
function M.debug_runnable(runnable)
  local ws = workspace.current()
  if not ws then
    util.notify("no remote workspace attached", vim.log.levels.WARN)
    return
  end
  local args = runnable and runnable.args
  if not args or runnable.kind ~= "cargo" then
    util.notify("unsupported rust-analyzer runnable", vim.log.levels.WARN)
    return
  end

  -- the lens payload crossed the LSP rewrite boundary, so workspaceRoot
  -- arrives as a local ws-prefixed path: translate it back
  local remote_root = args.workspaceRoot or ws.entry
  if vim.startswith(remote_root, ws.ws_root) then
    remote_root = workspace.remote_path(remote_root)
  end

  require("rnvim.dap").debug({
    name = runnable.label or "rust debug (remote)",
    root = remote_root,
    adapter = {
      command = "codelldb",
      args = function(port)
        return { "--port", tostring(port) }
      end,
    },
    build = {
      label = runnable.label,
      script = build_script(args, remote_root),
      program = parse_executable,
    },
    config = {
      args = args.executableArgs or {},
      cwd = remote_root,
      stopOnEntry = false,
      -- let codelldb spawn the debuggee itself: its runInTerminal path
      -- would ask nvim-dap to run the (remote) launcher stub locally
      terminal = "console",
      sourceLanguages = { "rust" },
    },
  })
end

return M
