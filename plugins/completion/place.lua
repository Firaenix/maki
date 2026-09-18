-- Where the popup goes, given the cell the input cursor is drawn in.
--
-- Separate from the drawing so the arithmetic can be tested without a
-- terminal, and because getting it wrong is silent: the host clamps a float
-- that runs past the edge of the screen down to whatever is left, so a popup
-- asking for ten rows two rows from the bottom becomes a two-row sliver rather
-- than an error.

local Place = {}

-- Rows the border costs, one at each end.
local BORDER = 2

Place.BORDER = BORDER

-- Rows of content that fit on the roomier side of {caret}. Whichever side has
-- more space wins, rather than trying above first: a caret near the top of the
-- screen has the whole transcript below it and two rows above.
function Place.room(caret, size)
  local above = caret.row
  local below = size.rows - caret.row - 1
  return math.max(above, below) - BORDER
end

-- Row, col and total height for a popup showing {rows} of content, or nil when
-- neither side of the caret has room for a single row.
--
-- The popup sits directly against the caret cell, above it when that is the
-- roomier side and below it otherwise, and the column is pulled left far
-- enough that the whole width lands on screen.
function Place.fit(caret, rows, width, size)
  local room = Place.room(caret, size)
  if rows < 1 or room < 1 then
    return nil
  end
  local height = math.min(rows, room) + BORDER
  local above = caret.row
  local below = size.rows - caret.row - 1
  local row = above >= below and caret.row - height or caret.row + 1
  local col = math.max(0, math.min(caret.col, size.cols - width))
  return row, col, height
end

return Place
