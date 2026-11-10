-- A panel pane: a small nvim that talks to the main nvim (which holds the
-- nvim-dap session) over RPC. The main side is hiero/dap.lua.
--
-- Panel → main: require('hiero.dap')._subscribe / _eval, by rpcnotify.
-- Main → panel: require('hiero.panel').on_hello / on_eval / on_event, by
-- rpcnotify over a channel the main nvim opens to this panel's server.
-- Nothing blocks on the other side, so a busy nvim never freezes the other.

local M = {}

local S = { kind = nil, server = nil, chan = nil, buf = nil, history = {}, hist_i = nil, said_down = false }
local ns = vim.api.nvim_create_namespace("hiero_panel")

local function present(v)
  return v ~= nil and v ~= vim.NIL
end

--- Call the main nvim. False when it can't be reached.
local function call(fn, ...)
  if not S.chan then
    return false
  end
  local ok = pcall(vim.rpcnotify, S.chan, "nvim_exec_lua", ("require('hiero.dap').%s(...)"):format(fn), { ... })
  if not ok then
    S.chan = nil
  end
  return ok
end

--- Add lines above the prompt, highlighted with `hl`, the last one with a dim `note` after it.
local function append(text, hl, note)
  local buf = S.buf
  local lines = vim.split(text, "\n", { plain = true })
  local at = vim.api.nvim_buf_line_count(buf) - 1
  vim.api.nvim_buf_set_lines(buf, at, at, false, lines)
  for i = 0, #lines - 1 do
    if hl then
      vim.api.nvim_buf_set_extmark(buf, ns, at + i, 0, { line_hl_group = hl })
    end
  end
  if note then
    vim.api.nvim_buf_set_extmark(buf, ns, at + #lines - 1, 0, { virt_text = { { "  " .. note, "Comment" } } })
  end
  -- Keep the prompt line in view in every window showing it.
  for _, w in ipairs(vim.fn.win_findbuf(buf)) do
    vim.api.nvim_win_call(w, function()
      vim.cmd("normal! zb")
    end)
  end
end

local function connect()
  local ok, chan = pcall(vim.fn.sockconnect, "pipe", S.server, { rpc = true })
  if ok and chan > 0 then
    S.chan = chan
    S.said_down = false
    call("_subscribe", S.kind, vim.v.servername)
  elseif not S.said_down then
    S.said_down = true
    append("waiting for nvim at " .. S.server, "Comment")
  end
end

local function watch()
  local timer = vim.uv.new_timer()
  timer:start(0, 1000, vim.schedule_wrap(function()
    -- A closed channel has no info left.
    if S.chan and next(vim.api.nvim_get_chan_info(S.chan)) == nil then
      S.chan = nil
      append("lost the connection to nvim", "Comment")
    end
    if not S.chan then
      connect()
    end
  end))
end

-- From the main nvim ---------------------------------------------------------

function M.on_hello(session)
  append(present(session) and ("connected · " .. session) or "connected · no debug session", "Comment")
end

function M.on_eval(_, err, result, typ)
  if present(err) then
    append(err, "ErrorMsg")
  else
    append(present(result) and result or "", nil, present(typ) and typ ~= "" and typ or nil)
  end
end

function M.on_event(text)
  append("● " .. text, "Comment")
end

-- The REPL -------------------------------------------------------------------

local function set_prompt_text(text)
  local n = vim.api.nvim_buf_line_count(S.buf)
  local line = vim.fn.prompt_getprompt(S.buf) .. text
  vim.api.nvim_buf_set_lines(S.buf, n - 1, n, false, { line })
  vim.api.nvim_win_set_cursor(0, { n, #line })
end

local function history(step)
  local h = S.history
  if #h == 0 then
    return
  end
  local i = (S.hist_i or #h + 1) + step
  if i < 1 then
    i = 1
  end
  if i > #h then
    S.hist_i = nil
    set_prompt_text("")
    return
  end
  S.hist_i = i
  set_prompt_text(h[i])
end

local function repl()
  local buf = vim.api.nvim_create_buf(false, true)
  S.buf = buf
  vim.bo[buf].buftype = "prompt"
  vim.fn.prompt_setprompt(buf, "> ")
  vim.fn.prompt_setcallback(buf, function(text)
    text = vim.trim(text)
    if text == "" then
      return
    end
    if S.history[#S.history] ~= text then
      table.insert(S.history, text)
    end
    S.hist_i = nil
    if not call("_eval", vim.v.servername, text) then
      append("not connected to nvim", "ErrorMsg")
    end
  end)
  vim.fn.prompt_setinterrupt(buf, function() end)
  vim.keymap.set("i", "<Up>", function()
    history(-1)
  end, { buffer = buf })
  vim.keymap.set("i", "<Down>", function()
    history(1)
  end, { buffer = buf })
  vim.api.nvim_set_current_buf(buf)
  vim.cmd.startinsert()
end

-- Start ----------------------------------------------------------------------

function M.start(kind, server)
  S.kind, S.server = kind or "repl", server
  vim.o.shadafile = "NONE"
  vim.o.swapfile = false
  vim.o.number = false
  vim.o.signcolumn = "no"
  vim.o.laststatus = 0
  vim.o.ruler = false
  vim.o.showmode = false
  vim.o.termguicolors = true
  vim.opt.shortmess:append("I")
  if S.kind == "repl" then
    repl()
  else
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { "hiero panel: unknown kind " .. tostring(kind) })
    return
  end
  if not server or server == "" then
    append("no nvim server given", "ErrorMsg")
    return
  end
  watch()
end

return M
