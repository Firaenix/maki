use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthChar;

use maki_agent::PermissionPreview;
use maki_agent::permissions::{DEFAULT_DENY_GUIDANCE, PermissionAnswer, generalized_scopes};
use maki_config::{DEFAULT_PERMISSION_EXPAND_KEY, ToolKey};
use maki_lua::Key;

use crate::components::Overlay;
use crate::components::code_view::{render_file, render_snippet_diff};
use crate::components::form::render_form;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::components::shell_format::format_command;
use crate::highlight;
use crate::text_buffer::TextBuffer;
use crate::theme;

const HINT_ALLOW_ROW: &[(&str, &str)] = &[
    ("y", "Allow"),
    ("a", "Always (project)"),
    ("A", "Always (all projects)"),
    ("s", "Session"),
];
const HINT_DENY_ROW: &[(&str, &str)] = &[
    ("n", "Deny"),
    ("d", "Deny-always (project)"),
    ("D", "Deny-always (all)"),
];

// An untrusted folder keeps its project answers in memory, so the rows say
// what the answer really does instead of promising a saved rule.
const HINT_ALLOW_ROW_UNTRUSTED: &[(&str, &str)] = &[
    ("y", "Allow"),
    ("a", "Project (this session)"),
    ("A", "Always (all projects, saved)"),
    ("s", "Session"),
];
const HINT_DENY_ROW_UNTRUSTED: &[(&str, &str)] = &[
    ("n", "Deny"),
    ("d", "Deny project (this session)"),
    ("D", "Deny-always (all, saved)"),
];
const UNTRUSTED_NOTICE: &str = "folder not trusted, project answers last for this session only";
/// Nothing is written into a folder the user declined, so the answer that
/// survives a restart is the global one and the prompt has to name it.
const UNTRUSTED_DURABLE_ANSWER: &str = "use D to save a deny that outlives this session";

const CONFIRM_ALLOW_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_ALLOW_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_SESSION_HINTS: &[(&str, &str)] =
    &[("Enter / y", "Confirm allow (session)"), ("any", "Cancel")];
const CONFIRM_DENY_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_DENY_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_ALLOW_PROJECT_SESSION_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow (project, this session)"),
    ("any", "Cancel"),
];
const CONFIRM_DENY_PROJECT_SESSION_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny (project, this session)"),
    ("any", "Cancel"),
];

const DENY_GUIDANCE_HINTS: &[(&str, &str)] = &[("Enter", "Deny"), ("Esc", "Cancel")];
const QUEUED_NOTICE: &str = "more request(s) waiting";
const EXPAND_HINT: &str = "Expand";
const COLLAPSE_HINT: &str = "Collapse";
const EXPANDED_NAV_ROW: &[(&str, &str)] = &[
    ("j/k", "Scroll"),
    ("PgUp/PgDn", "Page"),
    ("g/G", "Top/bottom"),
    ("Esc/q", "Collapse"),
];
const PREVIEW_TRUNCATED: &str = "… cut short, the call carries more than this";
const TITLE: &str = " Permission Required ";
/// Answer keys win over the expand key, so one bound on top of them would
/// leave nothing to expand with.
const ANSWER_KEYS: &[char] = &['y', 'n', 'a', 'A', 'd', 'D', 's'];

fn is_shell(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(name) if &**name == "bash")
}

/// One row of spans per line of {text}, highlighted as bash when {shell}.
/// Highlighting runs over the lines in order so a heredoc or a quote that
/// spans lines keeps its colour.
fn highlighted_rows(text: &str, shell: bool, value_style: Style) -> Vec<Vec<Span<'static>>> {
    let mut hl =
        (shell && highlight::is_ready()).then(|| maki_highlight::Highlighter::for_token("bash"));
    text.split('\n')
        .map(|line| match hl.as_mut() {
            Some(hl) => highlight::highlight_line(hl, &format!("{line}\n"))
                .into_iter()
                .map(|span| {
                    let content = span.content.trim_end_matches('\n').to_owned();
                    Span::styled(content, span.style)
                })
                .filter(|span| !span.content.is_empty())
                .collect(),
            None => vec![Span::styled(line.to_owned(), value_style)],
        })
        .collect()
}

/// A bash scope is the command itself: each line of it gets its own row,
/// highlighted as bash, with the label on the first.
fn scope_lines(
    label: &'static str,
    scope: &str,
    shell: bool,
    label_style: Style,
    value_style: Style,
) -> Vec<Line<'static>> {
    let rows = if shell {
        highlighted_rows(scope, true, value_style)
    } else {
        vec![vec![Span::styled(scope.to_owned(), value_style)]]
    };
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(if i == 0 { label } else { "      " }, label_style),
            ];
            spans.extend(row);
            Line::from(spans)
        })
        .collect()
}

/// Cuts {spans} into rows at most {width} columns wide, keeping each piece's
/// style, so the expanded view can number source lines and still wrap them.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut rows = vec![Vec::new()];
    let mut used = 0;
    for span in spans {
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used > 0 && used + w > width {
                if !chunk.is_empty() {
                    let piece = Span::styled(std::mem::take(&mut chunk), span.style);
                    rows.last_mut().expect("never empty").push(piece);
                }
                rows.push(Vec::new());
                used = 0;
            }
            chunk.push(ch);
            used += w;
        }
        if !chunk.is_empty() {
            rows.last_mut()
                .expect("never empty")
                .push(Span::styled(chunk, span.style));
        }
    }
    rows
}

/// Wraps rendered code to {width}, treating each line's first span as its
/// gutter: continuation rows hang under the code, not under the number.
fn hang_wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut spans = line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.trim_end_matches('\n').to_owned(), span.style))
            .filter(|span| !span.content.is_empty());
        let Some(gutter) = spans.next() else {
            out.push(Line::raw(""));
            continue;
        };
        let gutter_width = gutter.width();
        let avail = (width as usize).saturating_sub(2 + gutter_width).max(1);
        for (k, piece) in wrap_spans(spans.collect(), avail).into_iter().enumerate() {
            let lead = if k == 0 {
                gutter.clone()
            } else {
                Span::raw(" ".repeat(gutter_width))
            };
            let mut row = vec![Span::raw("  "), lead];
            row.extend(piece);
            out.push(Line::from(row));
        }
    }
    out
}

/// A write as the file it leaves, an edit as a diff per replacement.
fn preview_lines(preview: &PermissionPreview, highlight: bool) -> Vec<Line<'static>> {
    let t = theme::current();
    let heading = |label: &'static str, path: &str| {
        Line::from(vec![
            Span::styled(label, t.tool_dim),
            Span::styled(path.to_owned(), Style::new().fg(t.foreground)),
        ])
    };
    let mut lines = Vec::new();
    match preview {
        PermissionPreview::Write {
            path,
            content,
            append,
            ..
        } => {
            lines.push(heading(if *append { "append " } else { "write " }, path));
            lines.extend(render_file(path, content, highlight));
        }
        PermissionPreview::Edit { path, edits, .. } => {
            lines.push(heading("edit ", path));
            for (i, edit) in edits.iter().enumerate() {
                if edits.len() > 1 {
                    lines.push(Line::raw(""));
                    lines.push(Line::from(Span::styled(
                        format!("edit {}/{}", i + 1, edits.len()),
                        t.tool_dim,
                    )));
                }
                lines.extend(render_snippet_diff(path, &edit.old, &edit.new, highlight));
            }
        }
    }
    if preview.truncated() {
        lines.push(Line::from(Span::styled(PREVIEW_TRUNCATED, t.tool_dim)));
    }
    lines
}

fn wrapped_height(lines: &[Line<'static>], width: u16) -> u16 {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width) as u16
}

fn aligned_hint_rows(rows: &[&[(&str, &str)]]) -> Vec<Line<'static>> {
    let t = theme::current();
    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; max_cols];
    for row in rows {
        for (i, (key, desc)) in row.iter().enumerate() {
            let cell_len = key.len() + 1 + desc.len();
            col_widths[i] = col_widths[i].max(cell_len);
        }
    }
    rows.iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(row.len() * 2);
            for (i, (key, desc)) in row.iter().enumerate() {
                spans.push(Span::styled(format!("  {key}"), t.keybind_key));
                let cell_len = key.len() + 1 + desc.len();
                let pad = if i + 1 < row.len() {
                    col_widths[i].saturating_sub(cell_len)
                } else {
                    0
                };
                spans.push(Span::styled(
                    format!(" {desc}{:width$}", "", width = pad),
                    t.tool_dim,
                ));
            }
            Line::from(spans)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysProject,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysProject,
    ConfirmDenyAlwaysGlobal,
    DenyEditing,
}

struct Request {
    id: String,
    tool: ToolKey,
    scopes: Vec<String>,
    subagent_id: Option<String>,
    allow_scopes: Vec<String>,
    project_trusted: bool,
    /// Set when a plugin escalated the call, so the user sees why they are
    /// asked about a call their rules might have let through.
    reason: Option<String>,
    /// What a file write would put on disk. The expanded view shows it in
    /// place of the scopes, which only name the file.
    preview: Option<PermissionPreview>,
    /// The preview highlighted, kept since every frame draws it and a big
    /// write is slow to highlight. Tagged with whether highlighting was ready,
    /// so a plain copy made during warmup is redone once it is.
    preview_lines: RefCell<Option<(bool, Vec<Line<'static>>)>>,
}

/// An answer carries the ask it settles: the agent only accepts one naming the
/// request it is parked on, and the ask on screen is not always the last one in.
#[derive(Debug)]
pub struct AnsweredRequest {
    pub id: String,
    pub subagent_id: Option<String>,
    pub answer: PermissionAnswer,
}

/// Every request parks a tool call until it is answered, so asks that arrive
/// while one is on screen queue up behind it instead of replacing it.
pub struct PermissionPrompt {
    /// The front is the one on screen; `state` and `buffer` belong to it and
    /// reset whenever it leaves.
    queue: VecDeque<Request>,
    state: PromptState,
    buffer: TextBuffer,
    expand_key: Key,
    expanded: bool,
    /// The expanded view's first body row. Cells because only drawing knows
    /// how many rows the body wraps to and how many fit, and the keys that
    /// page and clamp need both.
    scroll: Cell<u16>,
    page: Cell<u16>,
    total: Cell<u16>,
}

impl Overlay for PermissionPrompt {
    fn is_open(&self) -> bool {
        !self.queue.is_empty()
    }

    fn is_modal(&self) -> bool {
        false
    }

    /// Only reached when the run those asks belonged to is gone (cancel,
    /// session switch), which is what unparks their tool calls anyway.
    fn close(&mut self) {
        self.queue.clear();
        self.reset_entry();
    }
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            state: PromptState::Normal,
            buffer: TextBuffer::new(String::new()),
            expand_key: default_expand_key(),
            expanded: false,
            scroll: Cell::new(0),
            page: Cell::new(0),
            total: Cell::new(0),
        }
    }

    /// Takes `ui.permission_expand_key`. A key that does not parse, that the
    /// host reserves, or that is already an answer keeps the default, since
    /// the prompt must stay answerable either way.
    pub fn with_expand_key(mut self, notation: &str) -> Self {
        match parse_expand_key(notation) {
            Ok(key) => self.expand_key = key,
            Err(error) => tracing::warn!(
                key = notation,
                error,
                "ignoring ui.permission_expand_key, using \"{DEFAULT_PERMISSION_EXPAND_KEY}\""
            ),
        }
        self
    }

    fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.scroll.set(0);
    }

    fn max_scroll(&self) -> u16 {
        self.total.get().saturating_sub(self.page.get())
    }

    /// Expand, collapse and scroll. Only in the plain prompt: a half-typed
    /// deny reason takes every letter, and a confirm takes any key to cancel.
    fn view_key(&mut self, key: KeyEvent) -> bool {
        if self.state != PromptState::Normal {
            return false;
        }
        if Key::from_event(key) == Some(self.expand_key) {
            self.set_expanded(!self.expanded);
            return true;
        }
        if !self.expanded
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let (scroll, page) = (self.scroll.get(), self.page.get().max(1));
        let to = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.set_expanded(false);
                return true;
            }
            KeyCode::Char('j') | KeyCode::Down => scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => scroll.saturating_sub(1),
            KeyCode::PageDown => scroll.saturating_add(page),
            KeyCode::PageUp => scroll.saturating_sub(page),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => u16::MAX,
            _ => return false,
        };
        // Before the first draw nothing is known to clamp against; the draw
        // clamps then.
        let to = if self.total.get() > 0 {
            to.min(self.max_scroll())
        } else {
            to
        };
        self.scroll.set(to);
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        subagent_id: Option<String>,
        project_trusted: bool,
        reason: Option<String>,
        preview: Option<PermissionPreview>,
    ) {
        let allow_scopes = generalized_scopes(&tool, &scopes);
        let allow_scopes = if allow_scopes == scopes {
            vec![]
        } else {
            allow_scopes
        };
        self.queue.push_back(Request {
            id,
            tool,
            scopes,
            subagent_id,
            allow_scopes,
            project_trusted,
            reason,
            preview,
            preview_lines: RefCell::default(),
        });
    }

    fn reset_entry(&mut self) {
        self.state = PromptState::Normal;
        self.buffer = TextBuffer::new(String::new());
        self.set_expanded(false);
    }

    /// A cancelled subagent has nothing left to answer with, so its asks leave
    /// with it instead of parking the panel on a dead tool call.
    pub fn drop_subagent(&mut self, subagent_id: &str) {
        let owns = |request: &Request| request.subagent_id.as_deref() == Some(subagent_id);
        if self.queue.front().is_some_and(owns) {
            self.reset_entry();
        }
        self.queue.retain(|request| !owns(request));
    }

    pub(crate) fn tool(&self) -> Option<&ToolKey> {
        self.queue.front().map(|request| &request.tool)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<AnsweredRequest> {
        let answer = self.key_answer(key)?;
        let request = self.queue.pop_front()?;
        self.reset_entry();
        Some(AnsweredRequest {
            id: request.id,
            subagent_id: request.subagent_id,
            answer,
        })
    }

    fn key_answer(&mut self, key: KeyEvent) -> Option<PermissionAnswer> {
        if !self.is_open() {
            return None;
        }
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            return Some(PermissionAnswer::Deny);
        }
        if self.view_key(key) {
            return None;
        }
        let (state, buffer) = (&mut self.state, &mut self.buffer);
        if *state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = buffer.value().trim().to_string();
                    if text.is_empty() {
                        Some(PermissionAnswer::Deny)
                    } else {
                        Some(PermissionAnswer::DenyWithGuidance(text))
                    }
                }
                KeyCode::Esc => {
                    *buffer = TextBuffer::new(String::new());
                    *state = PromptState::Normal;
                    None
                }
                _ => {
                    buffer.handle_key(key);
                    None
                }
            };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let confirm_answer = match *state {
            PromptState::ConfirmAllowAlwaysProject => Some(PermissionAnswer::AllowAlwaysProject),
            PromptState::ConfirmAllowAlwaysGlobal => Some(PermissionAnswer::AllowAlwaysGlobal),
            PromptState::ConfirmAllowSession => Some(PermissionAnswer::AllowSession),
            PromptState::ConfirmDenyAlwaysProject => Some(PermissionAnswer::DenyAlwaysProject),
            PromptState::ConfirmDenyAlwaysGlobal => Some(PermissionAnswer::DenyAlwaysGlobal),
            _ => None,
        };
        if let Some(answer) = confirm_answer {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(answer),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            };
        }
        match key.code {
            KeyCode::Char('y') => Some(PermissionAnswer::AllowOnce),
            KeyCode::Char('n') => {
                *state = PromptState::DenyEditing;
                None
            }
            KeyCode::Char('a') => {
                *state = PromptState::ConfirmAllowAlwaysProject;
                None
            }
            KeyCode::Char('A') => {
                *state = PromptState::ConfirmAllowAlwaysGlobal;
                None
            }
            KeyCode::Char('d') => {
                *state = PromptState::ConfirmDenyAlwaysProject;
                None
            }
            KeyCode::Char('D') => {
                *state = PromptState::ConfirmDenyAlwaysGlobal;
                None
            }
            KeyCode::Char('s') => {
                *state = PromptState::ConfirmAllowSession;
                None
            }
            _ => None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.is_open() || self.state != PromptState::DenyEditing {
            return false;
        }
        self.buffer.insert_text(text);
        true
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let mut lines = self.header_lines();
        lines.extend(self.scope_rows());
        lines.extend(self.footer_lines());
        lines
    }

    fn header_lines(&self) -> Vec<Line<'static>> {
        let Some(Request {
            tool,
            subagent_id,
            reason,
            ..
        }) = self.queue.front()
        else {
            return vec![];
        };
        let t = theme::current();
        let label_style = t.tool_dim;
        let value_style = Style::new().fg(t.foreground);

        let mut tool_spans = vec![Span::raw("  "), Span::styled("tool  ", label_style)];
        if subagent_id.is_some() {
            tool_spans.push(Span::styled("[subtask] ", t.item_desc));
        }
        tool_spans.push(Span::styled(tool.to_string(), value_style));

        let mut lines = vec![Line::raw(""), Line::from(tool_spans)];
        if let Some(reason) = reason {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("why   ", label_style),
                Span::styled(reason.clone(), value_style),
            ]));
        }
        let waiting = self.queue.len() - 1;
        if waiting > 0 {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("queue ", label_style),
                Span::styled(format!("{waiting} {QUEUED_NOTICE}"), t.item_desc),
            ]));
        }
        lines
    }

    fn scope_rows(&self) -> Vec<Line<'static>> {
        let Some(Request { tool, scopes, .. }) = self.queue.front() else {
            return vec![];
        };
        let t = theme::current();
        let shell = is_shell(tool);
        let mut lines = Vec::new();
        for (i, s) in scopes.iter().enumerate() {
            let label = if i == 0 { "scope " } else { "    + " };
            lines.extend(scope_lines(
                label,
                s,
                shell,
                t.tool_dim,
                Style::new().fg(t.foreground),
            ));
        }
        lines
    }

    /// The expanded body: a file write's preview when it has one, else every
    /// scope, a bash one laid out one step per line, numbered by source line
    /// and wrapped to {width}.
    fn expanded_rows(&self, width: u16) -> Vec<Line<'static>> {
        let Some(Request {
            tool,
            scopes,
            preview,
            preview_lines: cached,
            ..
        }) = self.queue.front()
        else {
            return vec![];
        };
        if let Some(preview) = preview {
            let ready = highlight::is_ready();
            let mut cached = cached.borrow_mut();
            if cached
                .as_ref()
                .is_none_or(|(was_ready, _)| *was_ready != ready)
            {
                *cached = Some((ready, preview_lines(preview, ready)));
            }
            let lines = cached.as_ref().map(|(_, lines)| lines.clone());
            return hang_wrap(lines.unwrap_or_default(), width);
        }
        let t = theme::current();
        let shell = is_shell(tool);
        let text = scopes
            .iter()
            .map(|s| if shell { format_command(s) } else { s.clone() })
            .collect::<Vec<_>>()
            .join("\n");
        let rows = highlighted_rows(&text, shell, Style::new().fg(t.foreground));
        let digits = rows.len().to_string().len();
        let gutter = digits + 3;
        let avail = (width as usize).saturating_sub(gutter).max(1);
        let mut lines = Vec::with_capacity(rows.len());
        for (n, row) in rows.into_iter().enumerate() {
            for (k, piece) in wrap_spans(row, avail).into_iter().enumerate() {
                let number = if k == 0 {
                    format!("  {:>digits$} ", n + 1)
                } else {
                    " ".repeat(gutter)
                };
                let mut spans = vec![Span::styled(number, t.tool_dim)];
                spans.extend(piece);
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn footer_lines(&self) -> Vec<Line<'static>> {
        let Some(Request {
            allow_scopes,
            project_trusted,
            ..
        }) = self.queue.front()
        else {
            return vec![];
        };
        let (state, buffer) = (&self.state, &self.buffer);
        let t = theme::current();
        let label_style = t.tool_dim;
        let value_style = Style::new().fg(t.foreground);
        let mut lines = Vec::new();

        if !allow_scopes.is_empty() {
            for (i, g) in allow_scopes.iter().enumerate() {
                let label = if i == 0 { "allow " } else { "    + " };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label, label_style),
                    Span::styled(g.clone(), value_style),
                ]));
            }
        }

        if *state == PromptState::DenyEditing {
            let text = buffer.value();
            let (display_text, cursor_pos) = if text.is_empty() {
                (DEFAULT_DENY_GUIDANCE, 0)
            } else {
                (text.as_str(), TextBuffer::char_to_byte(&text, buffer.x()))
            };
            let (before, after) = display_text.split_at(cursor_pos);
            let mut chars = after.chars();
            let cursor_ch = chars.next().unwrap_or(' ');
            let rest: String = chars.collect();

            let mut spans = vec![Span::raw("  "), Span::styled("guide ", label_style)];
            if text.is_empty() {
                spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
                spans.push(Span::styled(rest, t.tool_dim));
            } else {
                spans.push(Span::raw(before.to_string()));
                spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
                if !rest.is_empty() {
                    spans.push(Span::raw(rest));
                }
            }
            lines.push(Line::from(spans));
        }

        if !*project_trusted {
            for notice in [UNTRUSTED_NOTICE, UNTRUSTED_DURABLE_ANSWER] {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(notice, t.tool_dim),
                ]));
            }
        }

        lines.push(Line::raw(""));
        match *state {
            PromptState::ConfirmAllowAlwaysProject => {
                lines.push(hint_line(if *project_trusted {
                    CONFIRM_ALLOW_PROJECT_HINTS
                } else {
                    CONFIRM_ALLOW_PROJECT_SESSION_HINTS
                }));
            }
            PromptState::ConfirmAllowAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_ALLOW_ALL_HINTS));
            }
            PromptState::ConfirmAllowSession => {
                lines.push(hint_line(CONFIRM_SESSION_HINTS));
            }
            PromptState::ConfirmDenyAlwaysProject => {
                lines.push(hint_line(if *project_trusted {
                    CONFIRM_DENY_PROJECT_HINTS
                } else {
                    CONFIRM_DENY_PROJECT_SESSION_HINTS
                }));
            }
            PromptState::ConfirmDenyAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_DENY_ALL_HINTS));
            }
            PromptState::DenyEditing => {
                lines.push(hint_line(DENY_GUIDANCE_HINTS));
            }
            PromptState::Normal => {
                let (allow, deny) = if *project_trusted {
                    (HINT_ALLOW_ROW, HINT_DENY_ROW)
                } else {
                    (HINT_ALLOW_ROW_UNTRUSTED, HINT_DENY_ROW_UNTRUSTED)
                };
                // The deny row has a free column, so the toggle costs no row.
                let toggle = self.expand_key.notation();
                let toggle_hint = if self.expanded {
                    COLLAPSE_HINT
                } else {
                    EXPAND_HINT
                };
                let mut deny = deny.to_vec();
                deny.push((toggle.as_str(), toggle_hint));
                let mut rows: Vec<&[(&str, &str)]> = vec![allow, &deny];
                if self.expanded {
                    rows.push(EXPANDED_NAV_ROW);
                }
                lines.extend(aligned_hint_rows(&rows));
            }
        }
        lines.push(Line::raw(""));
        lines
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_open() {
            return;
        }
        let t = theme::current();
        if !self.expanded {
            render_form(&t, TITLE, frame, area, self.build_lines(), (0, 0));
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(t.panel_border)
            .title_top(Line::from(TITLE).left_aligned())
            .title_style(t.panel_title);
        let inner = block.inner(area);

        let header = self.header_lines();
        let footer = self.footer_lines();
        let [header_area, body_area, footer_area] = Layout::vertical([
            Constraint::Length(wrapped_height(&header, inner.width)),
            Constraint::Min(1),
            Constraint::Length(wrapped_height(&footer, inner.width)),
        ])
        .areas(inner);

        let body = self.expanded_rows(body_area.width);
        let total = body.len().min(u16::MAX as usize) as u16;
        self.total.set(total);
        self.page.set(body_area.height);
        let scroll = self.scroll.get().min(self.max_scroll());
        self.scroll.set(scroll);

        let block = if total > body_area.height {
            let last = scroll.saturating_add(body_area.height).min(total);
            block.title_top(Line::from(format!(" {}-{last}/{total} ", scroll + 1)).right_aligned())
        } else {
            block
        };
        frame.render_widget(block, area);
        let style = Style::new().fg(t.foreground);
        let wrapped = |lines| {
            Paragraph::new(lines)
                .style(style)
                .wrap(Wrap { trim: false })
        };
        frame.render_widget(wrapped(header), header_area);
        frame.render_widget(
            Paragraph::new(body).style(style).scroll((scroll, 0)),
            body_area,
        );
        frame.render_widget(wrapped(footer), footer_area);
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2);
        let rows = if self.expanded {
            let mut lines = self.header_lines();
            lines.extend(self.footer_lines());
            wrapped_height(&lines, inner_width)
                .saturating_add(self.expanded_rows(inner_width).len() as u16)
        } else {
            wrapped_height(&self.build_lines(), inner_width)
        };
        rows.saturating_add(2)
    }
}

fn default_expand_key() -> Key {
    Key::parse(DEFAULT_PERMISSION_EXPAND_KEY).expect("default expand key parses")
}

fn parse_expand_key(notation: &str) -> Result<Key, String> {
    let key = Key::parse(notation)?;
    if key.is_reserved() {
        return Err(format!("{notation} is reserved by the host"));
    }
    let event = KeyEvent::from(key);
    if !event
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(event.code, KeyCode::Char(c) if ANSWER_KEYS.contains(&c))
    {
        return Err(format!("{notation} already answers the prompt"));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::permissions::PermissionAnswer;
    use maki_config::ToolKey;
    use test_case::test_case;

    use maki_agent::PermissionPreview;
    use ratatui::text::Line;

    use super::{
        COLLAPSE_HINT, CONFIRM_ALLOW_PROJECT_HINTS, CONFIRM_ALLOW_PROJECT_SESSION_HINTS,
        CONFIRM_DENY_PROJECT_HINTS, CONFIRM_DENY_PROJECT_SESSION_HINTS, EXPAND_HINT, Overlay,
        PREVIEW_TRUNCATED, PermissionPrompt, PromptState, QUEUED_NOTICE, UNTRUSTED_DURABLE_ANSWER,
        UNTRUSTED_NOTICE, theme,
    };

    const MAIN_ID: &str = "id";
    const SUB_ID: &str = "id-2";
    const SUB_AGENT: &str = "sub-2";

    fn open_trusted(prompt: &mut PermissionPrompt, project_trusted: bool) {
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native("bash"),
            vec!["execute".into()],
            None,
            project_trusted,
            None,
            None,
        );
    }

    fn push_subagent_ask(prompt: &mut PermissionPrompt) {
        prompt.push(
            SUB_ID.into(),
            ToolKey::native("read"),
            vec!["/tmp/x".into()],
            Some(SUB_AGENT.into()),
            true,
            None,
            None,
        );
    }

    /// Most tests only care about the answer, not the ask it came back with.
    fn answer(prompt: &mut PermissionPrompt, key: KeyEvent) -> Option<PermissionAnswer> {
        prompt.handle_key(key).map(|answered| answered.answer)
    }

    fn open_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, true);
        prompt
    }

    fn rendered(prompt: &PermissionPrompt) -> String {
        prompt
            .build_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn row_texts(prompt: &PermissionPrompt) -> Vec<String> {
        prompt
            .build_lines()
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn bash_scope_gets_a_row_per_line_under_one_label() {
        crate::highlight::warmup();
        let mut prompt = PermissionPrompt::new();
        let script = "for f in *.rs; do\n  echo \"$f\"\ndone";
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native("bash"),
            vec![script.into()],
            None,
            true,
            None,
            None,
        );
        let rows = row_texts(&prompt);
        let at = rows.iter().position(|r| r.contains("scope ")).unwrap();
        assert_eq!(rows[at], "  scope for f in *.rs; do");
        assert_eq!(rows[at + 1], "          echo \"$f\"");
        assert_eq!(rows[at + 2], "        done");
    }

    #[test]
    fn non_bash_scope_stays_one_row() {
        let mut prompt = PermissionPrompt::new();
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native("read"),
            vec!["a\nb".into()],
            None,
            true,
            None,
            None,
        );
        assert!(row_texts(&prompt).contains(&"  scope a\nb".to_owned()));
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_c_denies() {
        let mut prompt = open_prompt();
        assert_eq!(answer(&mut prompt, ctrl_c()), Some(PermissionAnswer::Deny));
        // Also test from editing state
        let mut prompt2 = open_prompt();
        prompt2.handle_key(key(KeyCode::Char('n')));
        prompt2.handle_key(key(KeyCode::Char('t')));
        assert_eq!(answer(&mut prompt2, ctrl_c()), Some(PermissionAnswer::Deny));
    }

    #[test]
    fn n_goes_to_deny_editing() {
        let mut prompt = open_prompt();
        assert_eq!(answer(&mut prompt, key(KeyCode::Char('n'))), None);
        assert_eq!(prompt.state, PromptState::DenyEditing);
    }

    #[test]
    fn deny_editing_esc_returns_to_normal() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('t')));
        assert_eq!(answer(&mut prompt, key(KeyCode::Esc)), None);
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.buffer.value().is_empty());
    }

    #[test]
    fn deny_editing_enter_empty_sends_deny() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            answer(&mut prompt, key(KeyCode::Enter)),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn deny_editing_with_text_sends_guidance() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste("Use cat");
        assert_eq!(
            answer(&mut prompt, key(KeyCode::Enter)),
            Some(PermissionAnswer::DenyWithGuidance("Use cat".into()))
        );
    }

    #[test]
    fn handle_paste_requires_editing_mode() {
        let mut prompt = open_prompt();
        assert!(!prompt.handle_paste("ignored"));
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste("accepted"));
        assert_eq!(prompt.buffer.value(), "accepted");
    }

    #[test]
    fn wildcard_tool_key_opens() {
        let mut prompt = PermissionPrompt::new();
        prompt.push(
            MAIN_ID.into(),
            ToolKey::Wildcard,
            vec![],
            None,
            true,
            None,
            None,
        );
        assert!(prompt.is_open());
    }

    /// Parallel subagents each park a tool call on their own ask. An ask that
    /// lands while another is on screen has to wait its turn, not replace it:
    /// the replaced one would never be answered and its tool call would hang.
    #[test]
    fn a_second_request_waits_behind_the_one_on_screen() {
        let mut prompt = open_prompt();
        push_subagent_ask(&mut prompt);

        assert!(rendered(&prompt).contains(QUEUED_NOTICE));

        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste("main only");
        let first = prompt.handle_key(key(KeyCode::Enter)).expect("an answer");
        assert_eq!(first.id, MAIN_ID);
        assert_eq!(first.subagent_id, None);
        assert_eq!(
            first.answer,
            PermissionAnswer::DenyWithGuidance("main only".into())
        );

        assert!(prompt.is_open(), "the queued ask takes the panel");
        assert!(!rendered(&prompt).contains(QUEUED_NOTICE));

        // 'y' only answers if the half-typed deny left with the ask it was typed
        // for, otherwise it is still landing in the buffer.
        let second = prompt
            .handle_key(key(KeyCode::Char('y')))
            .expect("an answer");
        assert_eq!(second.id, SUB_ID);
        assert_eq!(second.subagent_id.as_deref(), Some(SUB_AGENT));
        assert_eq!(second.answer, PermissionAnswer::AllowOnce);
        assert!(!prompt.is_open());
    }

    #[test]
    fn a_cancelled_subagent_takes_its_request_with_it() {
        let mut prompt = PermissionPrompt::new();
        push_subagent_ask(&mut prompt);
        open_trusted(&mut prompt, true);
        prompt.handle_key(key(KeyCode::Char('n')));

        prompt.drop_subagent(SUB_AGENT);

        let answered = prompt
            .handle_key(key(KeyCode::Char('y')))
            .expect("the ask behind it moves up with a clean entry state");
        assert_eq!(answered.id, MAIN_ID);
        assert!(!prompt.is_open());
    }

    /// The always-answers are scoped to the project, and the hint has to say so
    /// before the user commits to one. In an untrusted folder the answer only
    /// holds for the session, and the hint says that instead.
    #[test_case(KeyCode::Char('a'), true, PromptState::ConfirmAllowAlwaysProject, CONFIRM_ALLOW_PROJECT_HINTS ; "allow_always")]
    #[test_case(KeyCode::Char('d'), true, PromptState::ConfirmDenyAlwaysProject, CONFIRM_DENY_PROJECT_HINTS ; "deny_always")]
    #[test_case(KeyCode::Char('a'), false, PromptState::ConfirmAllowAlwaysProject, CONFIRM_ALLOW_PROJECT_SESSION_HINTS ; "allow_always_untrusted")]
    #[test_case(KeyCode::Char('d'), false, PromptState::ConfirmDenyAlwaysProject, CONFIRM_DENY_PROJECT_SESSION_HINTS ; "deny_always_untrusted")]
    fn project_answers_confirm_before_they_apply(
        code: KeyCode,
        project_trusted: bool,
        expected: PromptState,
        hints: &[(&str, &str)],
    ) {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, project_trusted);

        assert_eq!(answer(&mut prompt, key(code)), None);

        assert_eq!(prompt.state, expected);
        let text = rendered(&prompt);
        assert!(text.contains(hints[0].1), "hint missing from: {text}");
    }

    /// The project answers stay on the screen in an untrusted folder, so the
    /// prompt has to explain how long they last and where a lasting deny goes
    /// instead.
    #[test_case(true, false ; "trusted_folder_says_nothing")]
    #[test_case(false, true ; "untrusted_folder_warns")]
    fn untrusted_folder_notice_follows_trust(project_trusted: bool, warned: bool) {
        let mut prompt = PermissionPrompt::new();
        open_trusted(&mut prompt, project_trusted);

        let text = rendered(&prompt);
        assert_eq!(text.contains(UNTRUSTED_NOTICE), warned, "rendered: {text}");
        assert_eq!(
            text.contains(UNTRUSTED_DURABLE_ANSWER),
            warned,
            "rendered: {text}"
        );
        assert_eq!(
            answer(&mut prompt, key(KeyCode::Char('a'))),
            None,
            "the project answer stays available either way"
        );
    }

    const ASK_REASON: &str = "plugin wants a human to look";
    const WHY_LABEL: &str = "why";

    #[test_case(Some(ASK_REASON) ; "with_reason")]
    #[test_case(None ; "without_reason")]
    fn why_row_follows_reason(reason: Option<&str>) {
        let mut prompt = PermissionPrompt::new();
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native("bash"),
            vec!["execute".into()],
            None,
            true,
            reason.map(str::to_owned),
            None,
        );

        let why_row = prompt.build_lines().iter().find_map(|line| {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            text.trim_start()
                .starts_with(WHY_LABEL)
                .then(|| text.trim_start()[WHY_LABEL.len()..].trim().to_owned())
        });
        assert_eq!(why_row.as_deref(), reason);
    }

    fn push_bash(prompt: &mut PermissionPrompt, command: &str) {
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native("bash"),
            vec![command.into()],
            None,
            true,
            None,
            None,
        );
    }

    /// The drawn screen, one string per row with trailing blanks trimmed.
    fn screen(prompt: &PermissionPrompt, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| prompt.view(f, f.area())).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn has_row(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|r| r.contains(needle))
    }

    const LONG_LINES: usize = 40;

    fn long_script() -> String {
        (1..=LONG_LINES)
            .map(|n| format!("echo line{n}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collapsed_footer_offers_the_expand_key() {
        let prompt = open_prompt();
        let text = rendered(&prompt);
        assert!(text.contains(&format!("  v {EXPAND_HINT}")), "{text}");
    }

    #[test]
    fn expanded_view_numbers_the_formatted_command() {
        let mut prompt = PermissionPrompt::new();
        push_bash(&mut prompt, "cd a && make 'x | y' | tee log");
        assert_eq!(answer(&mut prompt, key(KeyCode::Char('v'))), None);
        assert!(prompt.expanded);

        let rows = screen(&prompt, 60, 20);
        assert!(has_row(&rows, "│  1 cd a &&"), "{rows:#?}");
        assert!(has_row(&rows, "│  2   make 'x | y' |"), "{rows:#?}");
        assert!(has_row(&rows, "│  3   tee log"), "{rows:#?}");
        assert!(has_row(&rows, COLLAPSE_HINT), "{rows:#?}");
    }

    #[test]
    fn expanded_view_wraps_long_lines_under_one_number() {
        let mut prompt = PermissionPrompt::new();
        push_bash(&mut prompt, &format!("echo {}", "x".repeat(50)));
        prompt.handle_key(key(KeyCode::Char('v')));
        let rows = prompt.expanded_rows(30);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(texts.len() > 1, "{texts:#?}");
        assert!(texts[0].starts_with("  1 echo "));
        assert!(texts[1].starts_with("    x"), "{texts:#?}");
        assert!(texts.iter().all(|t| t.chars().count() <= 30), "{texts:#?}");
    }

    #[test_case(KeyCode::Char('v') ; "toggle_key")]
    #[test_case(KeyCode::Esc ; "esc")]
    #[test_case(KeyCode::Char('q') ; "q")]
    fn collapse_keys_close_the_expanded_view(code: KeyCode) {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('v')));
        assert_eq!(answer(&mut prompt, key(code)), None);
        assert!(!prompt.expanded);
        assert!(prompt.is_open());
    }

    #[test]
    fn expanded_view_scrolls() {
        let mut prompt = PermissionPrompt::new();
        push_bash(&mut prompt, &long_script());
        prompt.handle_key(key(KeyCode::Char('v')));
        let (w, h) = (60, 16);

        let rows = screen(&prompt, w, h);
        assert!(has_row(&rows, " 1 echo line1"), "{rows:#?}");
        assert!(!has_row(&rows, &format!("echo line{LONG_LINES}")));
        let page = prompt.page.get();
        assert!(page > 0 && (page as usize) < LONG_LINES);

        prompt.handle_key(key(KeyCode::Char('j')));
        prompt.handle_key(key(KeyCode::Down));
        assert_eq!(prompt.scroll.get(), 2);
        prompt.handle_key(key(KeyCode::Char('k')));
        assert_eq!(prompt.scroll.get(), 1);
        prompt.handle_key(key(KeyCode::PageDown));
        assert_eq!(prompt.scroll.get(), 1 + page);
        prompt.handle_key(key(KeyCode::PageUp));
        assert_eq!(prompt.scroll.get(), 1);

        prompt.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(prompt.scroll.get(), LONG_LINES as u16 - page);
        let rows = screen(&prompt, w, h);
        assert!(
            has_row(&rows, &format!("echo line{LONG_LINES}")),
            "{rows:#?}"
        );
        assert!(has_row(&rows, &format!("/{LONG_LINES} ")), "{rows:#?}");

        prompt.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            prompt.scroll.get(),
            LONG_LINES as u16 - page,
            "clamped at the bottom"
        );
        prompt.handle_key(key(KeyCode::Char('g')));
        assert_eq!(prompt.scroll.get(), 0);
    }

    #[test]
    fn answers_still_work_while_expanded() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('v')));
        assert_eq!(
            answer(&mut prompt, key(KeyCode::Char('y'))),
            Some(PermissionAnswer::AllowOnce)
        );
    }

    /// A deny reason takes every letter, scroll and toggle keys included.
    #[test]
    fn deny_reason_typed_while_expanded_keeps_its_letters() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('v')));
        prompt.handle_key(key(KeyCode::Char('n')));
        for c in "vjqg".chars() {
            prompt.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(prompt.buffer.value(), "vjqg");
        assert!(prompt.expanded);
        prompt.handle_key(key(KeyCode::Esc));
        assert_eq!(prompt.state, PromptState::Normal);
        assert!(prompt.expanded, "Esc leaves the reason, not the view");
    }

    #[test]
    fn the_next_request_starts_collapsed() {
        let mut prompt = open_prompt();
        push_subagent_ask(&mut prompt);
        prompt.handle_key(key(KeyCode::Char('v')));
        prompt.handle_key(key(KeyCode::Char('y')));
        assert!(prompt.is_open());
        assert!(!prompt.expanded);
    }

    fn push_preview(prompt: &mut PermissionPrompt, tool: &str, preview: PermissionPreview) {
        prompt.push(
            MAIN_ID.into(),
            ToolKey::native(tool),
            vec![preview.path().to_owned()],
            None,
            true,
            None,
            Some(preview),
        );
        prompt.handle_key(key(KeyCode::Char('v')));
    }

    fn row_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn write_preview(content: &str, truncated: bool) -> PermissionPreview {
        PermissionPreview::Write {
            path: "/p/main.rs".into(),
            content: content.into(),
            append: false,
            truncated,
        }
    }

    #[test]
    fn expanded_write_shows_the_numbered_highlighted_content() {
        crate::highlight::warmup();
        let mut prompt = PermissionPrompt::new();
        push_preview(
            &mut prompt,
            "write",
            write_preview("fn main() {\n    run();\n}\n", false),
        );
        let lines = prompt.expanded_rows(60);
        let rows = row_strings(&lines);
        assert_eq!(rows[0], "  write /p/main.rs", "{rows:#?}");
        assert_eq!(rows[1], "  1 fn main() {", "{rows:#?}");
        assert_eq!(rows[2], "  2     run();", "{rows:#?}");
        assert_eq!(rows[3], "  3 }", "{rows:#?}");
        let styles: std::collections::HashSet<_> =
            lines[1].spans.iter().skip(2).map(|s| s.style).collect();
        assert!(styles.len() > 1, "highlighted as rust: {:?}", lines[1]);
        assert!(!rows.iter().any(|r| r.contains(PREVIEW_TRUNCATED)));
    }

    #[test]
    fn expanded_edit_shows_each_replacement_as_a_coloured_diff() {
        crate::highlight::warmup();
        let mut prompt = PermissionPrompt::new();
        push_preview(
            &mut prompt,
            "multiedit",
            PermissionPreview::Edit {
                path: "/p/main.rs".into(),
                edits: vec![
                    maki_agent::PreviewEdit {
                        old: "let a = 1;".into(),
                        new: "let a = 2;".into(),
                    },
                    maki_agent::PreviewEdit {
                        old: "old_call();".into(),
                        new: "new_call();".into(),
                    },
                ],
                truncated: true,
            },
        );
        let lines = prompt.expanded_rows(60);
        let rows = row_strings(&lines);
        assert_eq!(rows[0], "  edit /p/main.rs", "{rows:#?}");
        assert!(rows.contains(&"  edit 1/2".to_owned()), "{rows:#?}");
        assert!(rows.contains(&"  edit 2/2".to_owned()), "{rows:#?}");
        let t = theme::current();
        let sign_of = |needle: &str| {
            let at = rows.iter().position(|r| r.contains(needle)).unwrap();
            lines[at]
                .spans
                .iter()
                .find(|s| s.content.trim() == "-" || s.content.trim() == "+")
                .map(|s| (s.content.trim().to_owned(), s.style))
                .unwrap()
        };
        let (removed, removed_style) = sign_of("let a = 1;");
        let (added, added_style) = sign_of("let a = 2;");
        assert_eq!(removed, "-");
        assert_eq!(added, "+");
        assert_eq!(removed_style.fg, t.code_block.patch(t.diff_old_sign).fg);
        assert_eq!(added_style.fg, t.code_block.patch(t.diff_new_sign).fg);
        assert!(has_row(&rows, "new_call();"), "{rows:#?}");
        assert_eq!(rows.last().unwrap(), &format!("  {PREVIEW_TRUNCATED}"));
    }

    #[test]
    fn a_preview_line_wraps_under_its_code_not_its_number() {
        let mut prompt = PermissionPrompt::new();
        push_preview(&mut prompt, "write", write_preview(&"x".repeat(50), false));
        let rows = row_strings(&prompt.expanded_rows(30));
        assert!(rows[1].starts_with("  1 x"), "{rows:#?}");
        assert!(rows[2].starts_with("    x"), "{rows:#?}");
        assert!(rows.iter().all(|r| r.chars().count() <= 30), "{rows:#?}");
    }

    #[test]
    fn the_collapsed_view_still_names_only_the_file() {
        let mut prompt = PermissionPrompt::new();
        push_preview(&mut prompt, "write", write_preview("secret body", false));
        prompt.handle_key(key(KeyCode::Char('v')));
        assert!(!prompt.expanded);
        let text = rendered(&prompt);
        assert!(text.contains("/p/main.rs"), "{text}");
        assert!(!text.contains("secret body"), "{text}");
    }

    #[test]
    fn expanding_asks_for_more_height() {
        let mut prompt = PermissionPrompt::new();
        push_bash(&mut prompt, "a && b && c && d");
        let collapsed = prompt.height(80);
        prompt.handle_key(key(KeyCode::Char('v')));
        assert!(prompt.height(80) > collapsed);
    }

    #[test_case("<C-e>", KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL), true ; "ctrl_key")]
    #[test_case("x", key(KeyCode::Char('x')), true ; "letter")]
    #[test_case("y", key(KeyCode::Char('v')), true ; "answer_key_keeps_default")]
    #[test_case("<C-c>", key(KeyCode::Char('v')), true ; "reserved_key_keeps_default")]
    #[test_case("<nope>", key(KeyCode::Char('v')), true ; "bad_notation_keeps_default")]
    #[test_case("x", key(KeyCode::Char('v')), false ; "rebound_default_is_free")]
    fn expand_key_follows_config(notation: &str, press: KeyEvent, expands: bool) {
        let mut prompt = PermissionPrompt::new().with_expand_key(notation);
        open_trusted(&mut prompt, true);
        prompt.handle_key(press);
        assert_eq!(prompt.expanded, expands);
    }
}
