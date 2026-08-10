-- Boot through the REAL plugin path: a child nvim started exactly like
-- production (plugin/rnvim.lua auto-boots from its VimEnter autocmd, the
-- target arrives via --cmd before config load). Every other spec calls
-- require("rnvim").setup() directly from script context — which is NOT
-- an autocmd context, and that difference hid a real bug: a non-nested
-- VimEnter autocmd swallows the BufReadCmd of boot's final `:edit`,
-- leaving a named-but-empty buffer instead of the workspace listing.
--
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e/boot_spec.lua

local here = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
local T = dofile(here .. "/../helpers.lua")
local root = vim.fs.dirname(vim.fs.dirname(here))

T.scenario("plugin-path boot renders the workspace", {
  { "a child nvim booted via plugin/rnvim.lua shows the directory listing", function(ctx)
    assert(vim.env.RNVIM_AGENT_BIN, "RNVIM_AGENT_BIN must point at a built agent")
    local workdir = vim.fn.tempname()
    vim.fn.mkdir(workdir, "p")
    local f = assert(io.open(workdir .. "/marker.txt", "w"))
    f:write("x\n")
    f:close()

    local probe = [[lua vim.defer_fn(function()
      local deadline = vim.uv.now() + 15000
      local function report()
        local lines = vim.api.nvim_buf_get_lines(0, 0, 5, false)
        if (lines[1] or ""):find("^rnvim://") or vim.uv.now() > deadline then
          print("BOOTPROBE " .. vim.json.encode({
            name = vim.api.nvim_buf_get_name(0),
            lines = lines,
          }))
          vim.cmd.qa()
        else
          vim.defer_fn(report, 500)
        end
      end
      report()
    end, 1000)]]

    local res = vim
      .system({
        vim.v.progpath,
        "--headless",
        "--clean",
        "--cmd",
        "set rtp+=" .. root,
        "--cmd",
        "lua vim.g.rnvim = { target = vim.env.RNVIM_TARGET }",
        "+" .. probe,
      }, {
        env = { RNVIM_TARGET = "local:" .. workdir },
        text = true,
      })
      :wait(60000)

    local payload = (res.stdout or ""):match("BOOTPROBE (%b{})")
      or ((res.stderr or ""):match("BOOTPROBE (%b{})"))
    T.truthy(payload, "child printed its probe (stdout: " .. vim.inspect(res.stdout) .. ")")
    ctx.state = vim.json.decode(payload)
  end },

  { "the buffer is the workspace directory, populated", function(ctx)
    T.contains(ctx.state.name, "/.rnvim/ws/local", "buffer under the ws prefix")
    T.contains(ctx.state.lines[1] or "", "rnvim://local", "listing header rendered")
    T.truthy(vim.list_contains(ctx.state.lines, "marker.txt"), "listing shows entries: " .. vim.inspect(ctx.state.lines))
  end },
})
