-- Where an `@` mention starts in the chat input, and what has been typed into
-- it so far.
--
-- Everything here counts bytes, because that is the unit `maki.ui.input` and
-- `maki.ui.input_edit` speak, and the one the Lua string library speaks too.
-- Nothing has to be converted: the offsets a match reports are the offsets an
-- edit writes over.

local Trigger = {}

-- An `@` only opens a mention at a word boundary, so an email address and a
-- `user@host` argument never do.
--
-- Looking at the single byte before the `@` is enough for text of any
-- encoding: every whitespace byte is ASCII, and no byte of a multi-byte
-- character is, so a `@` glued to the tail of "wörld" reads as mid-word the
-- same way one glued to "world" does.
local function opens_mention(before, at)
  return at == 1 or before:sub(at - 1, at - 1):match("^%s") ~= nil
end

-- Returns the byte offset of the `@` and the query typed after it, or nil when
-- the cursor is not sitting inside a mention.
function Trigger.find(text, cursor)
  local before = text:sub(1, cursor)
  local at = before:match("^.*()@")
  if not at or not opens_mention(before, at) then
    return nil
  end
  local query = before:sub(at + 1)
  -- A space ends the mention. Without this every later keystroke on the line
  -- would still count as typing into an `@` the user has long moved past.
  if query:match("%s") then
    return nil
  end
  return at - 1, query
end

return Trigger
