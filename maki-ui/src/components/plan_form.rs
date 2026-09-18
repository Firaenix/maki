use crate::components::form::{render_form, selected_prefix};
use crate::components::hint_line;
use crate::components::keybindings::key;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_lua::{PlanFormRow, PlanRowAction};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

const FORM_LABEL: &str = " Plan complete ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("Space", "toggle parallel"),
    ("Enter", "confirm"),
    (key::OPEN_EDITOR.label, "edit plan"),
    (DISMISS_KEYS, "dismiss"),
];

/// The rows the host proposes, i.e. the menu on a stock install. They are
/// what the `ui.plan_form.actions` chain starts from and what the form falls
/// back to when no Lua host answers, so the two can never drift apart.
const BUILTIN_ROWS: &[(&str, &str, PlanRowAction)] = &[
    (
        "Refine plan",
        "  Dismiss and keep editing the plan",
        PlanRowAction::Refine,
    ),
    (
        "Clear context and implement",
        "  Start fresh session, then implement the plan",
        PlanRowAction::ClearAndImplement,
    ),
    (
        "Implement plan",
        "  Keep current context, implement the plan",
        PlanRowAction::Implement,
    ),
];

pub fn builtin_rows() -> Vec<PlanFormRow> {
    BUILTIN_ROWS
        .iter()
        .map(|(label, desc, action)| PlanFormRow {
            label: (*label).to_owned(),
            desc: (*desc).to_owned(),
            action: *action,
        })
        .collect()
}

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanFormAction {
    Consumed,
    Passthrough,
    ClearAndImplement,
    Implement,
    OpenEditor,
    Hide,
    /// Plugin row picked, named by its position in the menu the
    /// `ui.plan_form.actions` chain built; App dispatches it to Lua.
    /// The form hides after emitting this, same as the built-in outcomes.
    Plugin {
        row: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

pub struct PlanForm {
    visibility: Visibility,
    selected: usize,
    parallel: bool,
    /// The menu the `ui.plan_form.actions` chain answered with for the
    /// current draft, or the host's own rows when nothing layered it.
    menu: Vec<PlanFormRow>,
}

impl PlanForm {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
            menu: builtin_rows(),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn on_plan_ready(&mut self) {
        if self.visibility != Visibility::UserDismissed {
            self.visibility = Visibility::Shown;
            self.selected = 0;
        }
    }

    pub fn on_plan_drafting(&mut self) {
        self.visibility = Visibility::Hidden;
    }

    pub fn toggle(&mut self) {
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            self.selected = 0;
            Visibility::Shown
        };
    }

    pub fn hide(&mut self) {
        if self.is_visible() {
            self.visibility = Visibility::UserDismissed;
        }
    }

    pub fn parallel(&self) -> bool {
        self.parallel
    }

    /// Show the form with the menu the `ui.plan_form.actions` chain built
    /// for this draft. Empty answers fall back to the host's own rows: a
    /// plugin that wants no form keeps `ui.plan_form` closed instead.
    pub fn open_with(&mut self, rows: Vec<PlanFormRow>) {
        self.menu = if rows.is_empty() {
            builtin_rows()
        } else {
            rows
        };
        self.on_plan_ready();
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.selected = 0;
        self.menu = builtin_rows();
    }

    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" Plan ", Style::new().fg(t.foreground)),
            Span::styled(key::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    pub fn height(&self) -> u16 {
        if self.is_visible() {
            self.menu.len() as u16 + CHROME_LINES
        } else {
            0
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> PlanFormAction {
        if key::QUIT.matches(key_event)
            || key_event.code == KeyCode::Esc
            || key::PLAN_TOGGLE.matches(key_event)
        {
            return PlanFormAction::Hide;
        }
        if key::OPEN_EDITOR.matches(key_event) {
            return PlanFormAction::OpenEditor;
        }
        match key_event.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PlanFormAction::Consumed
            }
            KeyCode::Down => {
                let max = self.menu.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(max);
                PlanFormAction::Consumed
            }
            KeyCode::Char(' ') => {
                self.parallel = !self.parallel;
                PlanFormAction::Consumed
            }
            KeyCode::Enter => self.row_action(self.selected),
            KeyCode::Tab => PlanFormAction::Passthrough,
            _ => PlanFormAction::Consumed,
        }
    }

    fn row_action(&self, row: usize) -> PlanFormAction {
        match self.menu.get(row).map(|r| r.action) {
            Some(PlanRowAction::ClearAndImplement) => PlanFormAction::ClearAndImplement,
            Some(PlanRowAction::Implement) => PlanFormAction::Implement,
            Some(PlanRowAction::Plugin) => PlanFormAction::Plugin { row },
            // Refine dismisses, and so does a pick off the end of the menu.
            Some(PlanRowAction::Refine) | None => PlanFormAction::Hide,
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(self.menu.len() + 2);

        for (i, row) in self.menu.iter().enumerate() {
            let (prefix, style) = selected_prefix(&t, i == self.selected);
            let mut spans = vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(row.label.clone(), style),
                Span::styled(row.desc.clone(), t.tool_dim),
            ];
            if self.parallel {
                spans.push(Span::styled(" (parallel)", t.tool_dim.bold()));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        render_form(&t, FORM_LABEL, frame, area, lines, (0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use test_case::test_case;

    const PLUGIN_LABEL: &str = "Commit and implement";

    fn plugin_row(label: &str) -> PlanFormRow {
        PlanFormRow {
            label: label.to_owned(),
            desc: String::new(),
            action: PlanRowAction::Plugin,
        }
    }

    fn last(form: &PlanForm) -> usize {
        form.menu.len() - 1
    }

    #[test]
    fn on_plan_ready_shows_and_resets_selected() {
        let mut form = PlanForm::new();
        form.selected = 1;
        form.on_plan_ready();
        assert!(form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn on_plan_ready_respects_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_ready();
        assert!(!form.is_visible());
    }

    #[test]
    fn on_plan_drafting_clears_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_drafting();
        form.on_plan_ready();
        assert!(
            form.is_visible(),
            "drafting should clear dismiss so next ready shows"
        );
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert!(form.is_visible());
        form.toggle();
        assert!(!form.is_visible());
        form.toggle();
        assert!(form.is_visible());
    }

    #[test]
    fn reset_clears_state() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 1;
        form.reset();
        assert!(!form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn hint_line_only_when_dismissed() {
        let mut form = PlanForm::new();
        assert!(form.hint_line().is_none());
        form.on_plan_ready();
        assert!(form.hint_line().is_none());
        form.hide();
        assert!(form.hint_line().is_some());
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = PlanForm::new();
        assert_eq!(form.height(), 0);
        form.on_plan_ready();
        assert_eq!(form.height(), BUILTIN_ROWS.len() as u16 + CHROME_LINES);
        form.hide();
        assert_eq!(form.height(), 0);
    }

    #[test_case(0, KeyCode::Up,   0    ; "up_at_zero_stays")]
    #[test_case(0, KeyCode::Down, 1    ; "down_from_zero")]
    fn navigation(start: usize, code: KeyCode, expected: usize) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = start;
        assert_eq!(form.handle_key(key(code)), PlanFormAction::Consumed);
        assert_eq!(form.selected, expected);
    }

    #[test]
    fn down_at_max_stays_at_last_row() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = last(&form);
        let target = last(&form);
        assert_eq!(
            form.handle_key(key(KeyCode::Down)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, target);
    }

    #[test]
    fn up_from_max_moves_one_up() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = last(&form);
        let target = last(&form) - 1;
        assert_eq!(form.handle_key(key(KeyCode::Up)), PlanFormAction::Consumed);
        assert_eq!(form.selected, target);
    }

    #[test_case(0, PlanFormAction::Hide              ; "enter_at_0_refine")]
    #[test_case(1, PlanFormAction::ClearAndImplement ; "enter_at_1")]
    #[test_case(2, PlanFormAction::Implement          ; "enter_at_2")]
    fn enter_dispatches(selected: usize, expected: PlanFormAction) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = selected;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), expected);
    }

    #[test]
    fn space_toggles_parallel() {
        let mut form = PlanForm::new();
        let initial = form.parallel();
        form.on_plan_ready();
        assert_eq!(form.parallel(), initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), !initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), initial);
    }

    #[test_case(key(KeyCode::Esc)              ; "esc")]
    #[test_case(key::QUIT.to_key_event()      ; "ctrl_c")]
    #[test_case(key::PLAN_TOGGLE.to_key_event(); "ctrl_t")]
    fn dismiss(k: KeyEvent) {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(form.handle_key(k), PlanFormAction::Hide);
    }

    #[test]
    fn ctrl_o_opens_editor() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key::OPEN_EDITOR.to_key_event()),
            PlanFormAction::OpenEditor
        );
    }

    #[test]
    fn unknown_key_consumed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Char('x'))),
            PlanFormAction::Consumed
        );
    }

    #[test]
    fn tab_passes_through() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Tab)),
            PlanFormAction::Passthrough
        );
    }

    // Chain-driven menu tests below cover the additions this PR brings to
    // the form. Existing tests above are unchanged so any regression in
    // built-in behavior surfaces at the same assertion.

    /// The chain owns the order, so the form draws the list it was handed
    /// rather than sorting it again behind the plugin's back.
    #[test]
    fn the_menu_is_drawn_in_the_order_the_chain_answered() {
        let mut form = PlanForm::new();
        let mut rows = builtin_rows();
        rows.insert(0, plugin_row(PLUGIN_LABEL));
        form.open_with(rows);
        let labels: Vec<_> = form.menu.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels[0], PLUGIN_LABEL);
        assert_eq!(labels[1], BUILTIN_ROWS[0].0);
    }

    /// A pick names the row's position, which is where the chain stashed its
    /// handler; nothing about the row has to survive the trip back to Lua.
    #[test]
    fn a_plugin_row_enter_names_its_position() {
        let mut form = PlanForm::new();
        let mut rows = builtin_rows();
        rows.push(plugin_row(PLUGIN_LABEL));
        let row = rows.len() - 1;
        form.open_with(rows);
        form.selected = row;
        assert_eq!(
            form.handle_key(key(KeyCode::Enter)),
            PlanFormAction::Plugin { row }
        );
    }

    /// A layer is free to drop a built-in row, and the outcome goes with it.
    #[test]
    fn a_chain_can_drop_a_builtin_row() {
        let mut form = PlanForm::new();
        let rows = vec![builtin_rows().remove(0)];
        form.open_with(rows);
        assert_eq!(form.menu.len(), 1);
        form.selected = 0;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), PlanFormAction::Hide);
    }

    /// Emptying the menu would leave the user a form with nothing in it; a
    /// plugin that wants no form keeps `ui.plan_form` closed instead.
    #[test]
    fn an_empty_menu_falls_back_to_the_builtin_rows() {
        let mut form = PlanForm::new();
        form.open_with(vec![]);
        assert_eq!(form.menu.len(), BUILTIN_ROWS.len());
    }

    /// The menu belongs to one draft, so the next session starts from the
    /// rows the host knows rather than the last plugin's.
    #[test]
    fn reset_restores_the_builtin_menu() {
        let mut form = PlanForm::new();
        form.open_with(vec![plugin_row(PLUGIN_LABEL)]);
        form.reset();
        assert_eq!(form.menu, builtin_rows());
    }

    #[test]
    fn menu_length_drives_form_height() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        let base = form.height();
        let mut rows = builtin_rows();
        rows.push(plugin_row(PLUGIN_LABEL));
        rows.push(plugin_row("Another"));
        form.open_with(rows);
        assert_eq!(form.height(), base + 2);
    }
}
