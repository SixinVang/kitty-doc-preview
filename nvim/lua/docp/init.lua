-- docp.nvim: live docp Markdown preview in a Rex split, with bidirectional
-- scroll sync. The preview is the IPC server; this plugin is the client.
--
-- Preview -> editor: {"v":1,"type":"ready"} / {"v":1,"type":"top","line":N}
-- Editor -> preview: {"v":1,"type":"sync","line":N} / {"v":1,"type":"close"}

local uv = vim.uv or vim.loop

local M = {}

local defaults = {
  -- docp binary (name on PATH or absolute path)
  docp_bin = "docp",
  -- extra arguments passed to docp before --ipc
  docp_args = {},
  -- percentage of the Rex split given to the preview
  ratio = 50,
  -- debounce for editor scroll events, in ms
  debounce = 40,
  -- how long editor scrolls are ignored after applying a preview scroll, in ms
  takeover_ms = 120,
}

local config = vim.deepcopy(defaults)

--- bufnr -> session table
local sessions = {}

local function now_ms()
  return uv.hrtime() / 1e6
end

local function run_dir()
  local dir = vim.fn.stdpath("run")
  vim.fn.mkdir(dir, "p")
  return dir
end

local function window_for(buf)
  local win = vim.fn.bufwinid(buf)
  if win == -1 then
    return nil
  end
  return win
end

local function send(s, msg)
  if not s.pipe or s.pipe:is_closing() then
    return
  end
  local ok = pcall(s.pipe.write, s.pipe, vim.json.encode(msg) .. "\n")
  if not ok then
    s.ready = false
  end
end

local function send_sync(buf)
  local s = sessions[buf]
  if not s or not s.ready then
    return
  end
  if now_ms() < s.applying_until then
    return
  end
  local win = window_for(buf)
  if not win then
    return
  end
  local line = vim.fn.line("w0", win)
  if line == s.last_sent then
    return
  end
  s.last_sent = line
  send(s, { v = 1, type = "sync", line = line })
end

local function queue_sync(buf, delay)
  local s = sessions[buf]
  if not s or not s.ready then
    return
  end
  if now_ms() < s.applying_until then
    return
  end
  s.timer = s.timer or uv.new_timer()
  s.timer:stop()
  s.timer:start(delay or config.debounce, 0, vim.schedule_wrap(function()
    send_sync(buf)
  end))
end

local function apply_top(buf, line)
  local s = sessions[buf]
  local win = window_for(buf)
  if not s or not win or type(line) ~= "number" or line < 1 then
    return
  end
  line = math.min(line, vim.api.nvim_buf_line_count(buf))
  -- Do not echo this scroll back.
  s.last_sent = line
  s.applying_until = now_ms() + config.takeover_ms
  local ok = pcall(vim.api.nvim_win_set_cursor, win, { line, 0 })
  if not ok then
    return
  end
  pcall(vim.api.nvim_win_call, win, function()
    vim.cmd("normal! zt")
  end)
end

local function on_message(s, msg)
  local kind = msg and msg.type
  if kind == "ready" then
    s.ready = true
    s.last_sent = nil
    send_sync(s.buf)
  elseif kind == "top" then
    apply_top(s.buf, msg.line)
  end
end

local function on_data(s, data)
  if sessions[s.buf] ~= s then
    return
  end
  s.out = s.out .. data
  while true do
    local nl = s.out:find("\n", 1, true)
    if not nl then
      break
    end
    local line = s.out:sub(1, nl - 1)
    s.out = s.out:sub(nl + 1)
    local ok, msg = pcall(vim.json.decode, line)
    if ok and type(msg) == "table" then
      on_message(s, msg)
    end
  end
end

local function connect(buf)
  local s = sessions[buf]
  if not s then
    return
  end
  if s.connecting then
    return
  end
  s.connecting = true
  local pipe = uv.new_pipe(false)
  s.pipe = pipe
  local ok, err = pcall(pipe.connect, pipe, s.sock, function(cerr)
    s.connecting = false
    if sessions[buf] ~= s then
      if not pipe:is_closing() then
        pipe:close()
      end
      return
    end
    if cerr then
      s.pipe = nil
      if not pipe:is_closing() then
        pipe:close()
      end
      s.attempts = (s.attempts or 0) + 1
      if s.attempts > 120 then
        vim.notify("docp: could not connect to " .. s.sock, vim.log.levels.ERROR)
        return
      end
      s.retry = s.retry or uv.new_timer()
      s.retry:stop()
      s.retry:start(50, 0, vim.schedule_wrap(function()
        connect(buf)
      end))
      return
    end
    s.attempts = 0
    pipe:read_start(vim.schedule_wrap(function(rerr, data)
      if sessions[buf] ~= s then
        return
      end
      if rerr then
        s.ready = false
        return
      end
      if data then
        on_data(s, data)
      else
        s.ready = false
      end
    end))
  end)
  if not ok then
    s.connecting = false
    s.pipe = nil
    vim.notify("docp: IPC connect failed: " .. tostring(err), vim.log.levels.ERROR)
  end
end

local function attach_autocmds(s)
  local events = { "WinScrolled", "CursorMoved", "CursorMovedI" }
  local group = vim.api.nvim_create_augroup("docp_buf_" .. s.buf, { clear = true })
  s.aug = group
  vim.api.nvim_create_autocmd(events, {
    group = group,
    buffer = s.buf,
    callback = function()
      queue_sync(s.buf)
    end,
  })
  vim.api.nvim_create_autocmd("BufWritePost", {
    group = group,
    buffer = s.buf,
    callback = function()
      -- docp reloads by itself; resending re-anchors the preview.
      s.last_sent = nil
      queue_sync(s.buf)
    end,
  })
end

local function close_session(s, close_pane)
  if close_pane and s.block and s.session then
    vim.system({ "rex", "block", "close", s.block, "-s", s.session }, { text = true })
  end
  if s.pipe then
    if s.ready then
      pcall(s.pipe.write, s.pipe, '{"v":1,"type":"close"}\n')
    end
    if not s.pipe:is_closing() then
      s.pipe:close()
    end
    s.pipe = nil
  end
  if s.timer then
    s.timer:stop()
    s.timer:close()
    s.timer = nil
  end
  if s.retry then
    s.retry:stop()
    s.retry:close()
    s.retry = nil
  end
  if s.aug then
    pcall(vim.api.nvim_del_augroup_by_id, s.aug)
    s.aug = nil
  end
  os.remove(s.sock)
end

function M.setup(opts)
  config = vim.tbl_deep_extend("force", defaults, opts or {})
end

function M.open(buf)
  buf = buf or vim.api.nvim_get_current_buf()
  local file = vim.api.nvim_buf_get_name(buf)
  if file == "" then
    vim.notify("docp: save the buffer first", vim.log.levels.ERROR)
    return
  end
  M.close(buf)

  local session = vim.env.REX_SESSION
  local block = vim.env.REX_BLOCK
  if not session or not block then
    vim.notify(
      "docp: this plugin currently requires Rex (REX_SESSION/REX_BLOCK are unset)",
      vim.log.levels.ERROR
    )
    return
  end

  local bin = vim.fn.exepath(config.docp_bin)
  if bin == "" then
    vim.notify("docp: executable not found: " .. config.docp_bin, vim.log.levels.ERROR)
    return
  end

  local sock = run_dir() .. "/docp-" .. buf .. ".sock"
  os.remove(sock)

  local cmd = {
    "rex",
    "split",
    "-s",
    session,
    "-b",
    block,
    "--split=right",
    "--ratio",
    tostring(config.ratio),
    "--label",
    "docp: " .. vim.fn.fnamemodify(file, ":t"),
    "--focus=false",
    "--json",
    "--",
    bin,
  }
  vim.list_extend(cmd, config.docp_args)
  vim.list_extend(cmd, { "--ipc", sock, file })

  local res = vim.system(cmd, { text = true, timeout = 10000 }):wait()
  if res.code ~= 0 then
    vim.notify("docp: rex split failed: " .. (res.stderr or ""), vim.log.levels.ERROR)
    return
  end
  local ok, data = pcall(vim.json.decode, res.stdout or "")
  local pane = ok and data and data.block_ids and data.block_ids[1]
  if not pane then
    vim.notify("docp: could not parse rex split output", vim.log.levels.ERROR)
    return
  end

  local s = {
    buf = buf,
    session = session,
    block = pane,
    sock = sock,
    out = "",
    attempts = 0,
    applying_until = 0,
    last_sent = nil,
    ready = false,
  }
  sessions[buf] = s
  attach_autocmds(s)
  connect(buf)
end

function M.close(buf)
  buf = buf or vim.api.nvim_get_current_buf()
  local s = sessions[buf]
  if not s then
    return
  end
  sessions[buf] = nil
  close_session(s, true)
end

function M.toggle(buf)
  buf = buf or vim.api.nvim_get_current_buf()
  if sessions[buf] then
    M.close(buf)
  else
    M.open(buf)
  end
end

function M.close_all()
  for buf in pairs(sessions) do
    local s = sessions[buf]
    sessions[buf] = nil
    close_session(s, true)
  end
end

return M
