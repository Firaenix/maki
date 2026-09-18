-- `@` file completion for the chat input, in Lua.
--
-- Nothing here is special-cased in the host. The popup is an ordinary
-- unfocused float, the navigation keys are ordinary keymaps, and accepting is
-- one `maki.ui.input_edit` over the range the `@` opened. If this plugin can
-- do it, so can yours.
--
-- It triggers on `InputChanged` rather than on a key. Binding `"@"` would be
-- the obvious thing and it is a trap: a shifted character arrives with SHIFT
-- set, the override table compares modifiers exactly, and which terminals
-- report which is not something a plugin should have to know. Watching the
-- text is also what lets the list narrow while you keep typing, which no
-- keybinding could do without claiming every printable character.

local Trigger = require("trigger")
local Rank = require("rank")
local Place = require("place")

local opts = maki.api.register_options({
  max_items = { default = 10, min = 1, desc = "Rows the completion popup shows at once." },
})

-- Only unshifted keys, for the same reason the trigger is not a key.
local KEYS = {
  { "<Tab>", "next" },
  { "<C-n>", "next" },
  { "<C-p>", "prev" },
  { "<CR>", "accept" },
  { "<Esc>", "close" },
}
local FOOTER = { { "Tab", "next" }, { "Enter", "insert" }, { "Esc", "close" } }
local GLOB = "**/*"
local MIN_WIDTH = 24
-- Above the input box and the transcript, below anything modal.
local ZINDEX = 120
-- Cells the border costs, on both sides and on both ends.
local BORDER = Place.BORDER
-- One blank cell either side of a row, so text never touches the border.
local PAD = 2
-- Shown in place of the list rather than closing the popup, so a path that
-- does not exist yet says so instead of making the popup blink out and back.
local NO_MATCHES = "no matches"
-- Seconds a scanned file list stays usable. The walk is the expensive part of
-- a keystroke (`sort = "mtime"` stats every file), and a list a few seconds
-- out of date costs the user one file that will be there next time, so the
-- list outlives the popup that asked for it.
local CACHE_TTL = 30

local popup = nil
-- Survives `close`, unlike anything hung off `popup`: a query matching nothing
-- used to drop the list and re-walk the tree on the next keystroke.
local cache = { files = nil, at = 0 }
-- The keys this plugin has on right now. `maki.keymap.del` takes whatever is
-- bound to a key rather than only our own binding, so a key we never managed
-- to claim must never be deleted: it belongs to someone else.
local bound = {}
-- Declared up here because the key handlers below redraw with it.
local lines
-- Bumped by every close, so a round-trip that comes back after the popup it
-- was for has gone cannot repaint the one that replaced it.
local generation = 0
-- Numbers the refreshes. Two keystrokes in flight at once would otherwise race
-- to paint, and the one that lost would leave the previous query's files on
-- screen. Only the newest refresh is allowed to finish.
local latest = 0

local function relative(path, root)
  if root and path:sub(1, #root + 1) == root .. "/" then
    return path:sub(#root + 2)
  end
  return path
end

-- Unbinding is the one thing that must never be skipped. An error with `<CR>`
-- still claimed leaves the user unable to send a message at all, so every key
-- comes off on its own, and a failure to remove one cannot strand the rest.
local function unbind()
  local keys = bound
  bound = {}
  for _, key in ipairs(keys) do
    pcall(maki.keymap.del, "n", key)
  end
end

-- The only teardown. Keys come off before anything that could fail.
local function close()
  local closing = popup
  popup = nil
  generation = generation + 1
  unbind()
  if closing then
    pcall(function()
      closing.win:close()
    end)
  end
end

local function move(delta)
  if not popup then
    -- A binding outlived its popup. Take the keys off so the next press
    -- reaches the input box, and swallow this one: the host counts a key as
    -- claimed the moment the callback is dispatched, so there is nothing to
    -- hand back.
    unbind()
    return
  end
  local n = #popup.items
  if n > 0 then
    popup.sel = (popup.sel - 1 + delta) % n + 1
    popup.buf:set_lines(lines())
  end
end

-- Re-reads the input instead of trusting what was on screen when the popup was
-- drawn: the handler runs a frame or more after the key, and the edit has to
-- land on the mention that is there now.
local function accept()
  local choice = popup and popup.items[popup.sel]
  -- Nothing to insert, which is the "no matches" popup. The press is already
  -- claimed and cannot be handed back, so the most it can do is get the popup
  -- out of the way for the next one.
  if not choice then
    close()
    return
  end
  local st = maki.ui.input()
  local start = st and Trigger.find(st.text, st.cursor)
  if not start then
    close()
    return
  end
  -- The version refuses the edit outright if the user typed while this handler
  -- was running, rather than writing over a range that has since moved.
  local _, err = maki.ui.input_edit({
    start = start,
    stop = st.cursor,
    text = choice .. " ",
    version = st.version,
  })
  if err then
    maki.ui.flash(err)
  end
  close()
end

local HANDLERS = {
  next = function()
    move(1)
  end,
  prev = function()
    move(-1)
  end,
  accept = accept,
  close = close,
}

-- These replace any binding the user already had on the same key, and putting
-- one back is not something the keymap API can do, so the popup only ever
-- claims keys for as long as it is on screen.
local function bind()
  for _, entry in ipairs(KEYS) do
    local ok = pcall(maki.keymap.set, "n", entry[1], HANDLERS[entry[2]], { desc = "completion: " .. entry[2] })
    if ok then
      bound[#bound + 1] = entry[1]
    end
  end
end

-- What the popup shows: the ranked files, or the one placeholder row that says
-- there are none.
local function rows()
  if #popup.items == 0 then
    return { NO_MATCHES }, true
  end
  return popup.items, false
end

function lines()
  local shown, empty = rows()
  local out = {}
  for i, item in ipairs(shown) do
    local style = empty and "dim" or (i == popup.sel and "selected" or "item")
    out[i] = { { " " .. item, style } }
  end
  return out
end

local function render(caret, size)
  local shown = rows()
  local width = MIN_WIDTH
  for _, item in ipairs(shown) do
    width = math.max(width, maki.ui.display_width(item) + PAD + BORDER)
  end
  width = math.min(width, size.cols)
  local row, col, height = Place.fit(caret, #shown, width, size)
  -- Nowhere on screen to put it. Closing beats handing the host a rect it has
  -- to clamp into a sliver.
  if not row then
    return close()
  end
  popup.buf:set_lines(lines())
  popup.win:set_config({ width = width, height = height, row = row, col = col })
  popup.win:show()
end

-- Opened hidden, so the first frame never paints it at the placeholder
-- position it was created with.
local function ensure_popup()
  if popup then
    return
  end
  local buf = maki.ui.buf()
  local win = maki.ui.open_win(buf, {
    width = MIN_WIDTH,
    height = BORDER + 1,
    row = 0,
    col = 0,
    anchor = "NW",
    border = "rounded",
    footer = FOOTER,
    zindex = ZINDEX,
    focus = false,
    visible = false,
  })
  popup = { win = win, buf = buf, sel = 1, items = {} }
  bind()
end

-- One host round-trip per file list, then pure Lua ranking on every keystroke
-- after it. Re-globbing per keystroke would walk the tree for an answer that
-- has not changed, and typing a path that does not exist yet is exactly when
-- that would happen most.
local function scan()
  -- Checked before the session round-trip, so a keystroke on a warm cache
  -- costs nothing at all. A session switch clears the cache from the
  -- `SessionFocusChanged` handler, so a hit cannot be another tree's files.
  if cache.files and os.time() - cache.at < CACHE_TTL then
    return cache.files
  end
  -- The session's directory, not the process's. They differ the moment a
  -- second session is opened elsewhere, and offering files from the wrong
  -- tree is worse than offering none.
  local session = maki.session.read()
  local root = session and session.cwd
  if not root then
    return nil
  end
  -- No result cap. With `sort = "mtime"` the host stats the whole tree before
  -- it can rank, so a limit trims the answer without saving any of the work.
  local found, err = maki.fs.glob(GLOB, { path = root, sort = "mtime" })
  if err then
    maki.ui.flash(err)
    return nil
  end
  local files = {}
  for i, path in ipairs(found) do
    files[i] = relative(path, root)
  end
  cache.files, cache.at = files, os.time()
  return files
end

-- The one place that decides whether there should be a popup at all, and what
-- is in it. Everything it needs it reads for itself, because between the
-- keystroke that woke it and here the user may have typed on.
local function refresh()
  local token = generation
  latest = latest + 1
  local mine = latest
  -- True once this refresh has been overtaken, either by a newer keystroke or
  -- by a close.
  local function stale()
    return generation ~= token or latest ~= mine
  end

  local st = maki.ui.input()
  if stale() then
    return
  end
  -- `caret` is absent whenever the input box is not the thing the terminal
  -- cursor sits in, the first frame and a modal alike, and there is nothing to
  -- anchor a popup to then.
  if not st or not st.caret then
    return close()
  end
  local start, query = Trigger.find(st.text, st.cursor)
  -- A slash command owns the input while the command palette is up, and its
  -- own Tab and Enter with it.
  if not start or st.text:sub(1, 1) == "/" then
    return close()
  end
  local size = maki.ui.terminal_size()
  -- Asking for more rows than the screen has is what leaves the host clamping
  -- the float, so the limit is what fits rather than what was configured.
  local room = math.min(opts.max_items, Place.room(st.caret, size))
  if room < 1 then
    return close()
  end

  local files = scan()
  if stale() then
    return
  end
  if not files then
    return close()
  end
  local items = Rank.rank(files, query, room)

  ensure_popup()
  -- The highlight follows the file it was on while that file is still listed,
  -- so a keystroke that only drops candidates does not move the selection out
  -- from under the user.
  local was = popup.items[popup.sel]
  popup.items, popup.sel = items, 1
  for i, item in ipairs(items) do
    if item == was then
      popup.sel = i
    end
  end
  render(st.caret, size)
end

-- Autocmd dispatch waits on its handlers, so the round-trips run off to the
-- side rather than holding up every other plugin's events behind them.
maki.api.create_autocmd("InputChanged", {
  callback = function(ev)
    -- An accept is an `input_edit`, which is itself an InputChanged. Acting on
    -- it would reopen the popup on the path just inserted.
    if ev.data.source == "plugin" then
      return
    end
    if not popup and not Trigger.find(ev.data.text, ev.data.cursor) then
      return
    end
    maki.async.run(refresh)
  end,
})

-- The input of another session is another line of text entirely, and the
-- popup was placed against this one. The file list goes with it: a focus
-- change is the cheap signal that enough has happened for a fresh walk.
maki.api.create_autocmd({ "SessionFocusChanged", "SessionReset" }, {
  callback = function()
    cache.files = nil
    close()
  end,
})

-- A turn starting takes the input box away, and `<Esc>` goes to cancelling it
-- rather than to this plugin, so without this the popup would sit there with
-- `<CR>` still claimed and no key left that closes it. Closing twice is
-- harmless: the keys are already off and the window is already gone.
maki.api.create_autocmd("SessionStatusChanged", {
  callback = close,
})
