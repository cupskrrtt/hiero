-- nvim-dap → Hiero: the debugged program's output in a Hiero pane.
--
-- Most debug adapters (delve, netcoredbg, ...) send the program's output as
-- DAP `output` events instead of running it in a terminal. This writes those
-- events to a file, and a Hiero pane next to this nvim follows the file.
--
--   require("hiero.dap").setup({ dir = "down", size = 30 })
--
-- Outside Hiero (no $HIERO_PANE) it does nothing.

local M = {}

M.config = {
  cmd = "hiero", -- the hiero binary
  dir = "down", -- where the pane opens next to this nvim: "down" or "right"
  size = 30, -- its share of the split, in percent
  focus = false, -- move the focus to it
  close_on_end = false, -- close it when the session ends (otherwise it stays until the next run)
  categories = { stdout = true, stderr = true, console = false }, -- DAP output categories to show
}

local state = { file = nil, path = nil, pane = nil }

local function own_pane()
  local p = vim.env.HIERO_PANE
  return (p and p ~= "") and p or nil
end

-- One output pane per nvim pane, so two nvims in a workspace don't share one.
local function pane_name()
  return "dap output " .. own_pane():sub(-4)
end

local function log_path()
  return vim.fn.stdpath("run") .. "/hiero-dap-" .. own_pane() .. ".log"
end

local function hiero(args, on_ok)
  local argv = vim.list_extend({ M.config.cmd, "pane" }, args)
  vim.system(argv, { text = true }, function(r)
    if r.code ~= 0 then
      vim.schedule(function()
        vim.notify("hiero pane: " .. vim.trim(r.stderr or ""), vim.log.levels.WARN)
      end)
    elseif on_ok then
      vim.schedule(function()
        on_ok(vim.trim(r.stdout or ""))
      end)
    end
  end)
end

local function write(s)
  if state.file then
    state.file:write(s)
    state.file:flush()
  end
end

--- Start a fresh output file and open (or restart) the pane that follows it.
function M.open(title)
  if not own_pane() then
    return
  end
  if state.file then
    state.file:close()
  end
  state.path = log_path()
  state.file = io.open(state.path, "w")
  if not state.file then
    vim.notify("hiero.dap: can't write " .. state.path, vim.log.levels.WARN)
    return
  end
  write(("\27[2m── %s · %s ──\27[0m\n"):format(title or "debug", os.date("%H:%M:%S")))
  local args = { "split", "--reuse", "--name", pane_name(), "--dir", M.config.dir }
  if M.config.size then
    vim.list_extend(args, { "--size", tostring(M.config.size) })
  end
  if not M.config.focus then
    table.insert(args, "--no-focus")
  end
  vim.list_extend(args, { "--", "tail", "-n", "+1", "-F", state.path })
  hiero(args, function(id)
    state.pane = id
  end)
end

--- Close the output pane.
function M.close()
  if own_pane() then
    hiero({ "close", "--name", pane_name() })
  end
  state.pane = nil
end

local function on_output(_, body)
  local cat = body.category or "console"
  if not M.config.categories[cat] or not body.output then
    return
  end
  if cat == "stderr" then
    write("\27[31m" .. body.output .. "\27[0m")
  elseif cat == "console" then
    write("\27[2m" .. body.output .. "\27[0m")
  else
    write(body.output)
  end
end

local ended = false
local function on_end(_, body)
  if ended then
    return
  end
  ended = true
  local code = body and body.exitCode
  write(("\27[2m── %s ──\27[0m\n"):format(code and ("exited with " .. code) or "session ended"))
  if M.config.close_on_end then
    M.close()
  end
end

function M.setup(opts)
  M.config = vim.tbl_deep_extend("force", M.config, opts or {})
  if not own_pane() then
    return
  end
  local dap = require("dap")
  local function start(session)
    ended = false
    M.open(session and session.config and session.config.name)
  end
  dap.listeners.before.launch.hiero = start
  dap.listeners.before.attach.hiero = start
  dap.listeners.after.event_output.hiero = on_output
  dap.listeners.after.event_exited.hiero = on_end
  dap.listeners.after.event_terminated.hiero = on_end
  vim.api.nvim_create_user_command("HieroDapOutput", function()
    M.open("output")
  end, { desc = "Open the Hiero pane for debug output" })
  vim.api.nvim_create_user_command("HieroDapOutputClose", M.close, { desc = "Close the Hiero debug output pane" })
end

return M
