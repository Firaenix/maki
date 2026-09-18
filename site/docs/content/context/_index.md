+++
title = "Context"
weight = 31
[extra]
group = "Concepts"
+++

# Context

Everything the model knows about your project passes through one context window, and every token in it costs money and attention. This page covers what Maki puts there, when, and where you should put things so they land well.

## What loads when

```
session start (paid every request)   on demand (paid when used)
──────────────────────────────────   ─────────────────────────────────
system prompt                        file contents   read / index / grep
tool definitions                     skill bodies    skill tool
instruction files (AGENTS.md, ...)   memory notes    memory tool
memory tag names                     subdir rules    first read there
skill names + descriptions           MCP tool defs   tool_search
```

The left column is the fixed overhead of every single request, so Maki keeps it small on purpose: a skill contributes one description line, memories one list of tags, a big MCP server one search tool. The bodies stay on disk until the agent asks.

## Instruction files

At session start Maki walks from the project git root down to the working directory (no `.git` root, only the cwd). In each directory it loads **one** project instruction file, first match wins:

| Order | File |
|------|------|
| 1 | `AGENTS.md` |
| 2 | `CLAUDE.md` |
| 3 | `.github/copilot-instructions.md` |
| 4 | `COPILOT.md` |
| 5 | `.cursorrules` |
| 6 | `.windsurfrules` |
| 7 | `.clinerules` |
| 8 | `CONVENTIONS.md` |
| 9 | `GEMINI.md` |
| 10 | `CODING_AGENT.md` |

After the match it always loads `AGENTS.local.md` from the same directory if present: that one is yours, keep it gitignored. Closer directories win on conflicts. Finally one global `~/.config/maki/AGENTS.md` for preferences that follow you across projects.

```
~/repo/AGENTS.md           loaded (root)
~/repo/AGENTS.local.md     loaded (yours, gitignored)
~/repo/api/CLAUDE.md       loaded when cwd is ~/repo/api, wins over root
~/repo/web/AGENTS.md       not loaded yet...
~/.config/maki/AGENTS.md   loaded (global)
```

That `web/AGENTS.md` is not dead weight. The first time the agent `read`s a file under a subdirectory whose instruction file was never loaded, Maki pulls it in. Monorepo rules live next to the code they govern and cost nothing until someone works there.

Put coding conventions, repo quirks, and off-limits directories in these files. Keep them short; the next section explains why.

## Four places to put knowledge

All four end up in context, but at different times and prices:

| | Loaded | Costs | Good for |
|---|--------|-------|----------|
| `AGENTS.md` | every session | every request | short rules: conventions, build commands, no-go areas |
| [Skills](/docs/skills/) | when the agent picks one | a description line until then | long playbooks: release process, plugin authoring |
| Memory | when the agent recalls a tag | tag names until then | gotchas the agent learns while working |
| [Commands](/docs/commands/) | when you type `/name` | nothing until invoked | prompts you keep retyping |

Rule of thumb: when `AGENTS.md` grows past a screen, the new material probably wants to be a skill. `AGENTS.md` is a tax on every request; a skill is a tax only on the sessions that need it.

## Pointing at a file with `@`

Naming the file you mean saves the agent a search, and a search costs a tool call and a few hundred tokens before it has read anything. Typing `@` in the chat input opens a completion popup over your message, ranked with the same matcher as the `Ctrl+S` file picker:

```
> explain @maki-ui/src/app/mo
                ╭────────────────────────────────╮
                │ maki-ui/src/app/mod.rs         │
                │ maki-ui/src/app/model.rs       │
                ╰─ Tab next · Enter insert · Esc ╯
```

| Key | What it does |
|-----|--------------|
| `Tab`, `Ctrl+N` | next row |
| `Ctrl+P` | previous row |
| `Enter` | insert the highlighted path |
| `Esc` | close the popup |

The popup owns those keys only while it is on screen, so `Ctrl+P` still opens `/sessions` the rest of the time and every other key still types into your message. Keep typing to narrow the list. A space ends the mention, so `@` in an email address opens nothing. Moving the caret out of the mention with an arrow key closes the popup and gives the keys back, without changing a character of what you typed. With no match to insert, Enter closes the popup and sends nothing, and the next Enter sends your message as usual.

Esc closes the popup while the agent is working too, and the press after that stops the turn, the way it does with nothing on screen.

The rows are ranked for the text as it stood when you asked for them. Press Enter on a row faster than the list can catch up with your typing and maki refuses the insert and says so, rather than writing a path over the wrong part of your line.

Inserting a path writes text into your message. The file is not attached and nothing is read yet: the agent reads it when it decides to, with the same `read` tool it would have used anyway.

The plugin ships switched off while its file index proves itself on large repositories. Turn it on in `init.lua`:

```lua
maki.setup({
  plugins = {
    completion = { enabled = true, max_items = 10 },
  },
})
```

`max_items` is how many rows the popup shows at once. The first walk of a large tree takes a moment. Until it lands the popup says `scanning…`, then fills in on its own when the walk finishes, so you do not have to press a key to wake it. Enter while it is still scanning waits for the rows instead of closing the popup a moment before they arrive. That wait is bounded: when a walk never reports back the popup gives up after a few seconds, the rows read `no matches`, and Enter closes the popup from there.

## When the window fills

Long sessions eventually approach the model's context limit. Maki reserves a slice of the window (`agent.compaction_buffer`, default 20%) and before running out it summarizes the older turns and continues from the summary. `/compact` triggers it early, `/compact keep the repro steps` steers that one summary, `/usage` shows where the tokens went, and `agent.compaction_instructions` steers every summary.

Compaction replaces the older turns in the session's on-disk log with the summary. The dropped turns are not lost: before the rewrite, Maki parks the previous log at `sessions/archive/<session-id>/<n>.jsonl` in the [state directory](/docs/configuration/#directory-layout). It keeps the newest three per session, and at most 32 MB of them. The names count up, so the highest number is the newest.

An archive is a complete session file, so `jq` or an editor reads it as it is. To open one in Maki you have to put it back in place of the live log, which drops the session's current state, so move that out of the way first:

```sh
cd ~/.local/state/maki/sessions
mv <session-id>.jsonl <session-id>.jsonl.bak
cp archive/<session-id>/<n>.jsonl <session-id>.jsonl
maki -s <session-id>
```

`MAKI_DISABLE_AUTOCOMPACT=1` turns off the automatic compaction. A manual `/compact` still compacts.

Related: [Token Economy](/docs/token-economy/) for why all this frugality exists, [Configuration](/docs/configuration/) for the knobs.
