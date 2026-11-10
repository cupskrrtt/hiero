-- nvim-dap → Hiero panes. nvim-dap does the debugging; Hiero panes show it.
--
-- * output: most debug adapters (delve, netcoredbg, ...) send the program's
--   output as DAP `output` events. They go to a file that a pane next to this
--   nvim follows with `hiero pane follow` (Linux and Windows).
-- * panels (repl, ...): each is a small `nvim --clean` in its own pane that
--   talks to this nvim over RPC (see hiero/panel.lua). This nvim keeps the
--   session; a panel asks it to evaluate, and gets told when the program stops.
--
--   require("hiero.dap").setup({ dir = "down", size = 30, panels = { "repl" } })
--
-- Outside Hiero (no $HIERO_PANE) it does nothing.

local M = {}

M.config = {
  cmd = "hiero", -- the hiero binary
  dir = "down", -- where the output pane opens next to this nvim: "down" or "right"
  size = 30, -- its share of the split, in percent
  focus = false, -- move the focus to it
  close_on_end = false, -- close the output pane when the session ends (otherwise it stays until the next run)
  categories = { stdout = true, stderr = true, console = false }, -- DAP output categories to show
  panels = { "repl" }, -- panel panes opened with a session; {} for none
  close_panels_on_end = true, -- close the panels when the session ends
}

local state = { file = nil, path = nil, pane = nil }

local function own_pane()
  local p = vim.env.HIERO_PANE
  return (p and p ~= "") and p or nil
end

-- One set of panes per nvim pane, so two nvims in a workspace don't share them.
local function pane_name(kind)
  return "dap " .. kind .. " " .. own_pane():sub(-4)
end

local function log_path()
  return vim.fn.stdpath("run") .. "/hiero-dap-" .. own_pane() .. ".log"
end

-- This file is <root>/lua/hiero/dap.lua; the panels start from <root>/panel.lua.
local root = vim.fn.fnamemodify(debug.getinfo(1, "S").source:sub(2), ":p:h:h:h")

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

-- Panels ---------------------------------------------------------------------

-- Connected panels by their server address: { chan, kind }.
local subs = {}

local function notify(addr, fn, ...)
  local sub = subs[addr]
  if not sub then
    return
  end
  local ok = pcall(vim.rpcnotify, sub.chan, "nvim_exec_lua", ("require('hiero.panel').%s(...)"):format(fn), { ... })
  if not ok then
    subs[addr] = nil
  end
end

local function broadcast(fn, ...)
  for addr in pairs(subs) do
    notify(addr, fn, ...)
  end
end

local function nilable(v)
  if v == nil then
    return vim.NIL
  end
  return v
end

local function err_text(err)
  if type(err) ~= "table" then
    return tostring(err)
  end
  local fmt = vim.tbl_get(err, "body", "error", "format")
  return fmt or err.message or vim.inspect(err)
end

--- From a panel: it listens at `addr`. Connect back so we can tell it things.
function M._subscribe(kind, addr)
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", addr, { rpc = true })
  if not ok or chan == 0 then
    return
  end
  subs[addr] = { chan = chan, kind = kind }
  local s = require("dap").session()
  notify(addr, "on_hello", s and (s.config.name or "debug") or vim.NIL)
end

--- From a panel: evaluate in the current frame and answer it.
function M._eval(addr, expr)
  local s = require("dap").session()
  if not s then
    notify(addr, "on_eval", expr, "no debug session", vim.NIL, vim.NIL)
    return
  end
  s:evaluate(expr, function(err, resp)
    notify(
      addr,
      "on_eval",
      expr,
      err and err_text(err) or vim.NIL,
      nilable(resp and resp.result),
      nilable(resp and resp.type)
    )
  end)
end

local function open_panels(beside)
  for i, kind in ipairs(M.config.panels) do
    local args = { "split", "--reuse", "--name", pane_name(kind), "--no-focus" }
    if beside then
      -- Side by side with the output pane, sharing its row.
      vim.list_extend(args, { "--pane", beside, "--dir", "right", "--size", tostring(math.floor(100 * i / (i + 1))) })
    else
      vim.list_extend(args, { "--dir", M.config.dir, "--size", tostring(M.config.size or 30) })
    end
    vim.list_extend(args, { "--", vim.v.progpath, "--clean", "-u", root .. "/panel.lua", "--", kind, vim.v.servername })
    hiero(args)
  end
end

local function close_panels()
  for _, kind in ipairs(M.config.panels) do
    hiero({ "close", "--name", pane_name(kind) })
  end
end

-- Output ---------------------------------------------------------------------

--- Start a fresh output file and open (or restart) the pane that follows it.
--- `then_` gets the pane's id.
function M.open(title, then_)
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
  local args = { "split", "--reuse", "--name", pane_name("output"), "--dir", M.config.dir }
  if M.config.size then
    vim.list_extend(args, { "--size", tostring(M.config.size) })
  end
  if not M.config.focus then
    table.insert(args, "--no-focus")
  end
  vim.list_extend(args, { "--", M.config.cmd, "pane", "follow", state.path })
  hiero(args, function(id)
    state.pane = id
    if then_ then
      then_(id)
    end
  end)
end

--- Close the output pane.
function M.close()
  if own_pane() then
    hiero({ "close", "--name", pane_name("output") })
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

-- Session --------------------------------------------------------------------

local ended = false
local stop_pending = nil -- the reason of a stop whose location isn't known yet

local function on_end(_, body)
  if ended then
    return
  end
  ended = true
  local code = body and body.exitCode
  local text = code and ("exited with " .. code) or "session ended"
  write(("\27[2m── %s ──\27[0m\n"):format(text))
  broadcast("on_event", text)
  if M.config.close_on_end then
    M.close()
  end
  if M.config.close_panels_on_end then
    close_panels()
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
    M.open(session and session.config and session.config.name, function(out)
      open_panels(out)
    end)
  end
  dap.listeners.before.launch.hiero = start
  dap.listeners.before.attach.hiero = start
  dap.listeners.after.event_output.hiero = on_output
  dap.listeners.after.event_exited.hiero = on_end
  dap.listeners.after.event_terminated.hiero = on_end
  dap.listeners.after.event_stopped.hiero = function(_, body)
    stop_pending = body.reason or "stopped"
  end
  -- The stopped thread's frames arrive after the stop: that's where it is.
  dap.listeners.after.stackTrace.hiero = function(session, err, body, request)
    if not stop_pending or err or not body or request.threadId ~= session.stopped_thread_id then
      return
    end
    local f = (body.stackFrames or {})[1]
    local where = f and ("%s:%d"):format(f.source and (f.source.name or f.source.path) or f.name, f.line or 0) or ""
    broadcast("on_event", ("%s at %s"):format(stop_pending, where))
    stop_pending = nil
  end
  vim.api.nvim_create_user_command("HieroDapOutput", function()
    M.open("output")
  end, { desc = "Open the Hiero pane for debug output" })
  vim.api.nvim_create_user_command("HieroDapOutputClose", M.close, { desc = "Close the Hiero debug output pane" })
  vim.api.nvim_create_user_command("HieroDapPanels", function()
    open_panels(state.pane)
  end, { desc = "Open the Hiero debug panels" })
  vim.api.nvim_create_user_command("HieroDapPanelsClose", close_panels, { desc = "Close the Hiero debug panels" })
end

return M
