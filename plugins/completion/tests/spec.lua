local Trigger = require("trigger")
local Rank = require("rank")
local Place = require("place")
local th = require("maki.test_helpers")

local case = th.case
local eq = th.eq

local function find(text, cursor)
  local start, query = Trigger.find(text, cursor or #text)
  return start, query
end

case("trigger_finds_a_mention_at_the_cursor", function()
  local start, query = find("explain @src/ma")
  eq(start, 8, "the `@` is the ninth byte, so offset 8")
  eq(query, "src/ma", "everything after the `@` is the query")
end)

case("trigger_fires_on_a_bare_at_sign", function()
  local start, query = find("@")
  eq(start, 0, "an `@` in an empty input opens a mention at offset 0")
  eq(query, "", "with nothing typed into it yet")
end)

case("trigger_ignores_an_at_sign_after_a_multi_byte_letter", function()
  eq(find("wörld@src"), nil, "the byte before the `@` is a letter, whatever its encoding")
end)

case("trigger_ignores_an_at_sign_mid_word", function()
  eq(find("mail me at nick@example.com"), nil, "an email address is not a mention")
end)

case("trigger_ends_at_a_space", function()
  eq(find("@src/main.rs and then"), nil, "the mention ended when the user typed past it")
end)

case("trigger_takes_the_last_mention_on_the_line", function()
  local start, query = find("@one and @tw")
  eq(start, 9, "the mention being typed is the one at the cursor")
  eq(query, "tw")
end)

case("trigger_stops_at_the_cursor_not_the_end", function()
  local start, query = find("@src/ma and more", 6)
  eq(start, 0)
  eq(query, "src/m", "text past the cursor is not part of the query")
end)

-- Offsets are what the accept writes over, and the host counts them in bytes,
-- so a multi-byte character earlier in the line moves them.
case("trigger_counts_bytes_not_characters", function()
  local start, query = find("héllo wörld @src")
  eq(start, 14, "two two-byte characters put the `@` two bytes further along")
  eq(query, "src")
end)

case("trigger_survives_a_multi_byte_query", function()
  local start, query = find("@日本")
  eq(start, 0)
  eq(query, "日本")
end)

case("trigger_opens_after_a_newline", function()
  local start, query = find("first line\n@sec")
  eq(start, 11, "a newline counts as one byte, like everywhere else")
  eq(query, "sec")
end)

local FILES = {
  "maki-ui/src/main.rs",
  "README.md",
  "maki-lua/src/api/ui/mod.rs",
}

case("rank_matches_a_scattered_subsequence", function()
  local got = Rank.rank(FILES, "mrs", 10)
  eq(got[1], "maki-ui/src/main.rs", "a subsequence match beats having to type the path")
end)

case("rank_drops_what_does_not_match", function()
  eq(#Rank.rank(FILES, "zzz", 10), 0, "a query that is not a subsequence of anything matches nothing")
end)

case("rank_prefers_a_segment_start", function()
  local got = Rank.rank({ "src/formatter.rs", "s/x/r/c.rs" }, "src", 10)
  eq(got[1], "src/formatter.rs", "a run at a path boundary outranks scattered letters")
end)

case("rank_keeps_the_input_order_for_an_empty_query", function()
  local got = Rank.rank(FILES, "", 10)
  eq(got[1], FILES[1], "an empty query keeps the caller's mtime sort")
  eq(got[3], FILES[3])
end)

case("rank_honours_the_limit", function()
  eq(#Rank.rank(FILES, "", 2), 2)
end)

case("rank_is_case_insensitive", function()
  eq(Rank.rank(FILES, "README", 10)[1], "README.md")
  eq(Rank.rank(FILES, "readme", 10)[1], "README.md")
end)

-- Two paths that score the same must not swap places between calls; the sort
-- is not stable, so the input position is carried along explicitly.
case("rank_breaks_ties_by_input_order", function()
  local same = { "a/x.rs", "b/x.rs" }
  for _ = 1, 5 do
    eq(Rank.rank(same, "x.rs", 10)[1], "a/x.rs")
  end
end)

-- A 24-row terminal, so "above" and "below" are both nameable numbers.
local SIZE = { rows = 24, cols = 80 }

case("place_sits_above_a_caret_near_the_bottom", function()
  local row, col, height = Place.fit({ row = 20, col = 5 }, 5, 30, SIZE)
  eq(height, 7, "five rows of content plus the border")
  eq(row, 13, "the bottom border lands on the row over the caret")
  eq(col, 5, "anchored to the caret column when the width fits")
end)

-- The old placement tried above first and let the host clamp what did not fit,
-- which is how a full popup became a two-row sliver.
case("place_flips_below_when_there_is_more_room_there", function()
  local row, _, height = Place.fit({ row = 2, col = 0 }, 5, 30, SIZE)
  eq(row, 3, "directly under the caret")
  eq(height, 7, "the side with room takes the whole popup")
end)

case("place_clamps_the_height_to_the_room_it_has", function()
  local row, _, height = Place.fit({ row = 8, col = 0 }, 10, 30, { rows = 10, cols = 80 })
  eq(height, 8, "six rows of content is all that fits over the caret")
  eq(row, 0, "and it stops exactly at the top of the screen")
end)

case("place_gives_up_when_neither_side_fits_a_row", function()
  eq(Place.fit({ row = 2, col = 0 }, 5, 30, { rows = 4, cols = 80 }), nil, "nowhere to put it")
end)

case("place_pulls_the_column_left_so_the_width_fits", function()
  local _, col = Place.fit({ row = 20, col = 70 }, 3, 30, SIZE)
  eq(col, 50, "a popup anchored at column 70 would run off an 80-column screen")
end)

case("place_room_counts_the_roomier_side_less_the_border", function()
  eq(Place.room({ row = 20, col = 0 }, SIZE), 18, "twenty rows above, minus the border")
  eq(Place.room({ row = 2, col = 0 }, SIZE), 19, "twenty-one below beats two above")
end)

th.report()
