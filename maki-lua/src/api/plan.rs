//! `maki.plan`: the plan-mode surface. Plugins read the current plan state
//! without reaching into session internals, layer `ui.plan_form.actions` to
//! shape the form's menu, and layer `ui.plan_form` to take the form over.

use std::collections::HashMap;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::util::command::{
    PlanFormRow, PlanRequest, PlanRowAction, UiAction, ui_json_roundtrip,
};
use crate::api::util::pair::Pair;

const LABEL_KEY: &str = "label";
const DESC_KEY: &str = "desc";
const ACTION_KEY: &str = "action";
const HANDLER_KEY: &str = "handler";

/// The handlers behind the plugin rows of the plan form each session has
/// open. A session draws one form at a time, so a new menu replaces the last
/// one's keys rather than piling them up, and a row is found by its position
/// in the menu the UI is drawing.
#[derive(Default)]
pub(crate) struct PlanRowHandlers(HashMap<String, Vec<Option<RegistryKey>>>);

impl PlanRowHandlers {
    pub fn handler(&self, session: &str, row: usize) -> Option<&RegistryKey> {
        self.0.get(session)?.get(row)?.as_ref()
    }
}

/// Publish the menu a session is about to draw, dropping the handlers of the
/// menu it replaces so a long-lived session does not accumulate registry keys.
pub(crate) fn install_row_handlers(
    lua: &Lua,
    session: String,
    handlers: Vec<Option<RegistryKey>>,
) -> LuaResult<()> {
    let replaced = lua
        .app_data_mut::<PlanRowHandlers>()
        .ok_or_else(|| mlua::Error::runtime("plan row handlers not initialized"))?
        .0
        .insert(session, handlers);
    for key in replaced.into_iter().flatten().flatten() {
        lua.remove_registry_value(key)?;
    }
    Ok(())
}

/// The rows the host proposes, as the table the bottom of the
/// `ui.plan_form.actions` chain answers with.
pub(crate) fn rows_to_table(lua: &Lua, rows: &[PlanFormRow]) -> LuaResult<Table> {
    let out = lua.create_table()?;
    for row in rows {
        let t = lua.create_table()?;
        t.set(LABEL_KEY, row.label.as_str())?;
        t.set(DESC_KEY, row.desc.as_str())?;
        t.set(ACTION_KEY, row.action.tag())?;
        out.push(t)?;
    }
    Ok(out)
}

/// The menu the chain answered with. A row carrying a `handler` belongs to a
/// plugin and its function is stashed for the pick; anything else has to name
/// a built-in outcome, since the host is the only thing that can run one.
pub(crate) fn rows_from_table(
    lua: &Lua,
    table: Table,
) -> LuaResult<(Vec<PlanFormRow>, Vec<Option<RegistryKey>>)> {
    let mut rows = Vec::with_capacity(table.raw_len());
    let mut handlers = Vec::with_capacity(table.raw_len());
    for entry in table.sequence_values::<Table>() {
        let entry = entry?;
        let label: String = entry.get(LABEL_KEY)?;
        if label.is_empty() {
            return Err(mlua::Error::runtime("plan form row needs a 'label'"));
        }
        let desc: String = entry.get(DESC_KEY).unwrap_or_default();
        let (action, handler) = match entry.get::<Option<Function>>(HANDLER_KEY)? {
            Some(handler) => (
                PlanRowAction::Plugin,
                Some(lua.create_registry_value(handler)?),
            ),
            None => {
                let tag: String = entry.get(ACTION_KEY)?;
                let action = PlanRowAction::from_tag(&tag).ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "plan form row '{label}' has no 'handler' and an unknown action '{tag}'"
                    ))
                })?;
                (action, None)
            }
        };
        rows.push(PlanFormRow {
            label,
            desc,
            action,
        });
        handlers.push(handler);
    }
    Ok((rows, handlers))
}

/// The table a plugin row's handler is called with when the user picks it.
pub(crate) fn row_handler_opts(
    lua: &Lua,
    session: &str,
    path: &str,
    parallel: bool,
) -> LuaResult<Table> {
    let opts = lua.create_table()?;
    opts.set("session", session)?;
    opts.set("path", path)?;
    opts.set("parallel", parallel)?;
    Ok(opts)
}

/// Read the current plan state without reaching into session internals.
/// Returns `{ mode, path, content, ready }`:
/// - `mode` is `"plan"` or `"build"`.
/// - `path` is the absolute plan path when in plan mode, else `nil`.
/// - `content` is the file contents when `ready` is true, else `nil`
///   (`nil` distinguishes "not ready" and "read failed" from an empty
///   plan).
/// - `ready` is `true` once the agent has written the plan file.
///
/// @param opts table? `session` (string?) Session id; defaults to focused.
/// @return (table|nil, string|nil) Plan snapshot table, or nil and an error.
/// @example
/// local plan, err = maki.plan.read({ session = id })
/// if plan and plan.ready then
///   print("plan at " .. plan.path)
///   print(plan.content)
/// end
#[lua_fn]
async fn read(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let session = match opts {
        Some(opts) => opts.get("session")?,
        None => None,
    };
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::Read { session },
        reply_tx,
    })
    .await
}

lua_table! {
    /// Plan-mode surface for plugins.
    ///
    /// Read the plan without touching session internals, and shape what the
    /// plan form offers by layering the two slots maki fires around it.
    ///
    /// `ui.plan_form.actions` is the menu. The default answers with the
    /// built-in rows, each `{ label, desc, action }`, so a layer can append
    /// its own, reorder them, or drop one it does not want. A row carrying a
    /// `handler` is yours: the function runs on the Lua thread when the user
    /// picks it, with `{ session, path, parallel }`.
    ///
    /// `ui.plan_form` is the form itself. A layer that answers without
    /// calling `prev` keeps it closed and renders the plan however it likes.
    ///
    /// Both go away with your plugin, so an unload hands the form back.
    ///
    /// Every call takes an optional `session` and defaults to the focused
    /// tab. Plan state is per session, so a handler reacting to `PlanReady`
    /// on a background tab has to pass `ev.data.session_id` through, or it
    /// reads whichever plan the user happens to be looking at.
    ///
    /// ```lua
    /// -- One more row, next to the built-in ones:
    /// maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
    ///   local rows = prev(ev)
    ///   table.insert(rows, {
    ///     label = "Commit and implement",
    ///     desc = "Commit the plan file first, then implement it",
    ///     handler = function(opts)
    ///       maki.fn.system({ "git", "commit", "-am", "plan" })
    ///       local prompt = "Implement the plan at `" .. opts.path .. "`."
    ///       -- Fresh context, the way "Clear context and implement" does it:
    ///       maki.session.new({ prompt = prompt, focus = true })
    ///       -- ...or keep the context the plan was written in:
    ///       -- maki.session.prompt(prompt, { session = opts.session })
    ///     end,
    ///   })
    ///   return rows
    /// end)
    ///
    /// -- Own the plan UI for as long as this plugin is loaded:
    /// maki.api.set_slot("ui.plan_form", function(prev, ev)
    ///   local plan = maki.plan.read({ session = ev.session })
    ///   -- render plan.content in your own window
    ///   return false
    /// end)
    /// ```
    "maki.plan" => pub(crate) fn create_plan_table(tx: Option<flume::Sender<UiAction>>), DOCS [
        read(tx),
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::api::util::command::NO_UI_ERR;

    const BUILTIN_LABEL: &str = "Implement plan";
    const PLUGIN_LABEL: &str = "Commit and implement";

    fn builtin_rows() -> Vec<PlanFormRow> {
        vec![PlanFormRow {
            label: BUILTIN_LABEL.to_owned(),
            desc: "  Keep current context".to_owned(),
            action: PlanRowAction::Implement,
        }]
    }

    fn roundtrip(lua: &Lua, script: &str) -> (Vec<PlanFormRow>, Vec<Option<RegistryKey>>) {
        let proposed = rows_to_table(lua, &builtin_rows()).unwrap();
        lua.globals().set("rows", proposed).unwrap();
        let answered: Table = lua.load(script).eval().unwrap();
        rows_from_table(lua, answered).unwrap()
    }

    #[test]
    fn the_proposed_rows_survive_a_layer_that_just_hands_them_back() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(&lua, "return rows");
        assert_eq!(rows, builtin_rows());
        assert!(handlers.iter().all(Option::is_none));
    }

    #[test]
    fn a_row_with_a_handler_is_a_plugin_row() {
        let lua = Lua::new();
        let (rows, handlers) = roundtrip(
            &lua,
            &format!(
                r#"table.insert(rows, {{ label = "{PLUGIN_LABEL}", handler = function() end }})
                   return rows"#
            ),
        );
        assert_eq!(rows[1].label, PLUGIN_LABEL);
        assert_eq!(rows[1].action, PlanRowAction::Plugin);
        assert!(handlers[0].is_none() && handlers[1].is_some());
    }

    #[test]
    fn a_layer_can_drop_a_builtin_row() {
        let lua = Lua::new();
        let (rows, _) = roundtrip(&lua, "table.remove(rows, 1) return rows");
        assert!(rows.is_empty());
    }

    #[test_case(r#"{ label = "x", action = "nope" }"# ; "unknown_action")]
    #[test_case(r#"{ action = "implement" }"# ; "missing_label")]
    #[test_case(r#"{ label = "", action = "implement" }"# ; "empty_label")]
    fn a_row_the_host_cannot_run_is_rejected(row: &str) {
        let lua = Lua::new();
        let table: Table = lua.load(format!("return {{ {row} }}")).eval().unwrap();
        assert!(rows_from_table(&lua, table).is_err());
    }

    #[test]
    fn plan_read_without_ui_returns_error_pair() {
        let lua = Lua::new();
        let table = create_plan_table(&lua, None).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return plan.read()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Plan state is per session, so the call has to be able to name one
    /// rather than assume whichever tab the user happens to be looking at.
    #[test_case(r#"plan.read({ session = "s1" })"#, Some("s1") ; "explicit_session")]
    #[test_case("plan.read()", None ; "focused_session")]
    fn the_session_option_travels_to_the_ui(call: &str, expected: Option<&str>) {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let served = std::thread::spawn(move || {
            let Ok(UiAction::Plan { req, reply_tx }) = rx.recv() else {
                panic!("expected a Plan UiAction");
            };
            reply_tx.send(Ok(serde_json::json!(true))).unwrap();
            req
        });
        smol::block_on(lua.load(format!("return {call}")).eval_async::<Value>()).unwrap();
        assert_eq!(served.join().unwrap().session(), expected);
    }
}
