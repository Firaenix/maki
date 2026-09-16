-- Ranking the candidate paths against what has been typed after the `@`.
--
-- A subsequence match, so "mrs" finds "maki-ui/src/main.rs" without the user
-- having to know which directory it is in. The bonuses are what turn a
-- subsequence into a useful order: a run of adjacent characters and a
-- character right after a separator are what a person actually types when they
-- mean a particular file.
--
-- This lives in Lua on purpose. It is the one piece a host-side fuzzy matcher
-- would replace wholesale, and keeping it here means the plugin needs nothing
-- from the core that is not already there.

local Rank = {}

-- Adjacent characters are worth more the longer the run gets, so a literal
-- substring beats the same letters scattered across the path.
-- Worth more than a boundary on its own: every character of a deep path sits
-- right after a `/` or a `.`, so boundaries alone would rank
-- "s/x/r/c.rs" over "src/formatter.rs" for "src".
local RUN_BONUS = 8
-- Landing right after a separator means the user is typing the start of a path
-- or word segment rather than letters from the middle of one.
local BOUNDARY_BONUS = 6
local BOUNDARIES = { ["/"] = true, ["_"] = true, ["-"] = true, ["."] = true }
-- Only a tiebreak. Two paths that matched the same way are better ordered by
-- the shorter one than by nothing at all.
local LENGTH_PENALTY = 8

-- Scores {path} against an already-lowercased {query}, or nil when {query} is
-- not a subsequence of it.
--
-- Multi-byte characters are compared byte by byte. A subsequence search stays
-- correct that way, since the bytes of one character are adjacent and in
-- order; only the bonuses land slightly off, which costs nothing a user can
-- see.
local function score(path, query)
  local lower = path:lower()
  local pos, total, run = 1, 0, 0
  for i = 1, #query do
    local found = lower:find(query:sub(i, i), pos, true)
    if not found then
      return nil
    end
    local bonus = 0
    if found == pos and i > 1 then
      run = run + 1
      bonus = RUN_BONUS + run
    else
      run = 0
    end
    local prev = found > 1 and lower:sub(found - 1, found - 1) or "/"
    if BOUNDARIES[prev] then
      bonus = bonus + BOUNDARY_BONUS
    end
    total = total + bonus
    pos = found + 1
  end
  return total - #path / LENGTH_PENALTY
end

-- The best {limit} of {paths} for {query}, best first. An empty query keeps
-- the input order, which is the caller's mtime sort: the files you touched
-- last are the ones you are most likely reaching for.
function Rank.rank(paths, query, limit)
  local q = query:lower()
  local scored = {}
  for i, path in ipairs(paths) do
    local s = q == "" and 0 or score(path, q)
    if s then
      -- The sort is not stable, so the input position has to be carried along
      -- as the tiebreak rather than relied on.
      scored[#scored + 1] = { path = path, score = s, order = i }
    end
  end
  table.sort(scored, function(a, b)
    if a.score ~= b.score then
      return a.score > b.score
    end
    return a.order < b.order
  end)
  local out = {}
  for i = 1, math.min(#scored, limit) do
    out[i] = scored[i].path
  end
  return out
end

Rank._score = score

return Rank
