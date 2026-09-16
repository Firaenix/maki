//! `maki.plan`: the plan-mode surface. Plugins add rows to the plan form,
//! take the form over by layering the `ui.plan_form` slot, and read the
//! current plan state without reaching into session internals.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::util::command::{PlanRequest, UiAction, ui_json_roundtrip};
use crate::api::util::pair::Pair;

pub(crate) const PLAN_ACTION_DEFAULT_ORDER: i64 = 500;

/// Per-plugin plan actions the UI merges into the form menu.
pub(crate) type PlanActionHandlerMap = HashMap<Arc<str>, HashMap<Arc<str>, PlanActionEntry>>;

pub(crate) struct PlanActionEntry {
    pub handler: RegistryKey,
    pub id: Arc<str>,
    pub label: Arc<str>,
    pub desc: Arc<str>,
    pub order: i64,
}

/// The identity a handler matches on. Derived from `(plugin, name)` rather
/// than handed out by a counter, so it survives a reload and says the same
/// thing in two processes.
pub(crate) fn action_id(plugin: &str, name: &str) -> Arc<str> {
    Arc::from(format!("{plugin}/{name}").as_str())
}

#[derive(Clone)]
pub struct PlanActionInfo {
    pub plugin: Arc<str>,
    pub name: Arc<str>,
    pub id: Arc<str>,
    pub label: Arc<str>,
    pub desc: Arc<str>,
    pub order: i64,
}

#[derive(Clone, Default)]
pub struct PlanActionSnapshot {
    /// Sorted by `(order, plugin, name)`, so the menu the UI builds from this
    /// is the same menu in every process.
    pub actions: Vec<PlanActionInfo>,
    /// The plugin layering `ui.plan_form`, if any. Lives here because the
    /// plan form is the one reader, and it already polls this snapshot.
    pub form_owner: Option<Arc<str>>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct PlanActionReader(Arc<ArcSwap<PlanActionSnapshot>>);

impl PlanActionReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(
            PlanActionSnapshot::default(),
        )))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<PlanActionSnapshot>> {
        self.0.load()
    }
}

pub(crate) struct PlanActionWriter {
    store: Arc<ArcSwap<PlanActionSnapshot>>,
    generation: AtomicU64,
}

impl PlanActionWriter {
    pub fn new() -> (Self, PlanActionReader) {
        let inner = Arc::new(ArcSwap::from_pointee(PlanActionSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            PlanActionReader(inner),
        )
    }

    pub fn publish(&self, actions: Vec<PlanActionInfo>, form_owner: Option<Arc<str>>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(PlanActionSnapshot {
            actions,
            form_owner,
            generation,
        }));
    }
}

fn collect_actions(map: &PlanActionHandlerMap) -> Vec<PlanActionInfo> {
    let mut actions: Vec<PlanActionInfo> = map
        .iter()
        .flat_map(|(plugin, actions)| {
            actions.iter().map(move |(name, entry)| PlanActionInfo {
                plugin: Arc::clone(plugin),
                name: Arc::clone(name),
                id: Arc::clone(&entry.id),
                label: Arc::clone(&entry.label),
                desc: Arc::clone(&entry.desc),
                order: entry.order,
            })
        })
        .collect();
    // `HashMap` hands rows back in `RandomState` order, which reseeds per
    // process, and every plugin row defaults to the same `order`. Without a
    // tiebreak two plugins would swap places between runs of the same config.
    actions.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then_with(|| a.plugin.cmp(&b.plugin))
            .then_with(|| a.name.cmp(&b.name))
    });
    actions
}

/// Rebuild the snapshot the plan form polls. Called from every mutation of
/// the action map and from `set_slot` / plugin teardown, since the form's
/// owner lives in the slot store and rides along here.
pub(crate) fn republish_snapshot(lua: &Lua) -> LuaResult<()> {
    let form_owner = crate::api::slot::slot_layer_owner(lua, crate::api::slot::PLAN_FORM_SLOT);
    let map = lua
        .app_data_ref::<PlanActionHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
    let writer = lua
        .app_data_ref::<PlanActionWriter>()
        .ok_or_else(|| mlua::Error::runtime("plan action writer not initialized"))?;
    writer.publish(collect_actions(&map), form_owner);
    Ok(())
}

/// Add a row to the plan-mode form menu. The form appears when the agent
/// finishes writing a plan; plugin rows sit alongside the built-in
/// "Refine plan", "Clear context and implement", and "Implement plan"
/// entries, sorted by `order`.
///
/// Same `name` registered twice by the same plugin replaces in place, so
/// a reload never stacks duplicates. Two different plugins can each
/// register the same name because rows are keyed by `(plugin, name)`.
///
/// Rows are sorted by `order`, then by plugin and action name, so the menu
/// reads the same on every run.
///
/// The handler runs on the Lua thread when the user picks the row. It
/// receives `{ id, name, session, path, parallel }`: `id` and `name`
/// identify the row that fired, so one handler can serve several rows, and
/// `session` is the session the plan belongs to, to pass to
/// `maki.plan.*`. Fire the built-in outcomes with `maki.plan.implement` or
/// `maki.plan.open_editor`; returning without calling one just hides the
/// form.
///
/// @param spec table Action specification:
///   name    (string)   Required. Unique per plugin.
///   label   (string)   Required. Menu row title.
///   desc    (string)   Optional. Second row shown under the title.
///   order   (integer)  Optional. Position among rows (default 500). Built-ins are 0, 1000, 2000.
///   handler (function) Required. Called with `{ id, name, session, path, parallel }`.
/// @return (string) Stable id of the row, `"<plugin>/<name>"`.
/// @example
/// local id = maki.api.register_plan_action({
///   name = "commit-and-implement",
///   label = "Commit and implement",
///   desc  = "Commit the plan file first, then implement",
///   handler = function(opts)
///     if opts.id ~= id then return end
///     -- write the plan file to git etc.
///     maki.plan.implement({ session = opts.session })
///   end,
/// })
#[lua_fn]
fn register_plan_action(lua: &Lua, #[ctx] plugin: Arc<str>, spec: Table) -> LuaResult<String> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'name'"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_plan_action: 'name' must be non-empty",
        ));
    }
    let label: String = spec
        .get("label")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'label'"))?;
    if label.is_empty() {
        return Err(mlua::Error::runtime(
            "register_plan_action: 'label' must be non-empty",
        ));
    }
    let desc: String = spec.get("desc").unwrap_or_default();
    let order: i64 = spec
        .get::<Option<i64>>("order")
        .map_err(|_| mlua::Error::runtime("register_plan_action: 'order' must be an integer"))?
        .unwrap_or(PLAN_ACTION_DEFAULT_ORDER);
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_plan_action: missing 'handler'"))?;

    let handler_key = lua.create_registry_value(handler)?;
    let id = action_id(&plugin, &name);
    let name: Arc<str> = Arc::from(name.as_str());
    let label: Arc<str> = Arc::from(label.as_str());
    let desc: Arc<str> = Arc::from(desc.as_str());

    {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        let by_plugin = map.entry(Arc::clone(&plugin)).or_default();
        // Silent replace within (plugin, name), matching register_reviewer /
        // register_command; reloads must not stack duplicates.
        if let Some(prev) = by_plugin.insert(
            Arc::clone(&name),
            PlanActionEntry {
                handler: handler_key,
                id: Arc::clone(&id),
                label,
                desc,
                order,
            },
        ) {
            let _ = lua.remove_registry_value(prev.handler);
        }
    }
    republish_snapshot(lua)?;
    Ok(id.to_string())
}

/// Remove one of this plugin's plan actions by name. Unknown names are a
/// no-op so a toggle can call it unconditionally.
///
/// @param name string The name the action was registered under.
/// @return
/// @example
/// maki.api.unregister_plan_action("commit-and-implement")
#[lua_fn]
fn unregister_plan_action(lua: &Lua, #[ctx] plugin: Arc<str>, name: String) -> LuaResult<()> {
    let removed = {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        let Some(by_plugin) = map.get_mut(&plugin) else {
            return Ok(());
        };
        let removed = by_plugin.remove(name.as_str());
        if by_plugin.is_empty() {
            map.remove(&plugin);
        }
        removed
    };
    if let Some(entry) = removed {
        let _ = lua.remove_registry_value(entry.handler);
        republish_snapshot(lua)?;
    }
    Ok(())
}

/// Drop every plan action this plugin registered. Companion to
/// `unregister_plan_action` for disable toggles that don't want to name
/// each action.
///
/// @return
/// @example
/// maki.api.clear_plan_actions()
#[lua_fn]
fn clear_plan_actions(lua: &Lua, #[ctx] plugin: Arc<str>) -> LuaResult<()> {
    let removed = {
        let mut map = lua
            .app_data_mut::<PlanActionHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("plan action map not initialized"))?;
        map.remove(&plugin).unwrap_or_default()
    };
    if removed.is_empty() {
        return Ok(());
    }
    for (_, entry) in removed {
        let _ = lua.remove_registry_value(entry.handler);
    }
    republish_snapshot(lua)
}

/// The session a plan call acts on. Plan state is per session, so a
/// handler woken by a background tab has to say which one it means; the
/// focused tab is the default because that is what an interactive plugin
/// wants.
fn session_of(opts: Option<&Table>) -> LuaResult<Option<String>> {
    match opts {
        Some(t) => t.get("session"),
        None => Ok(None),
    }
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
    let session = session_of(opts.as_ref())?;
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::Read { session },
        reply_tx,
    })
    .await
}

/// Fire the same "implement the plan" code path a built-in row would.
/// Call from a plan-action handler when it decides the plan is ready to
/// execute. `clear_context = true` starts a fresh session first
/// (equivalent to picking "Clear context and implement" from the
/// built-in menu).
///
/// @param opts table? Options:
///   clear_context (boolean) Default false. Start a fresh session before implementing.
///   session (string) Session id; defaults to focused.
/// @return (boolean|nil, string|nil) true once dispatched, or nil and an error.
/// @example
/// maki.plan.implement({ clear_context = true })
#[lua_fn]
async fn implement(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let clear_context = opts
        .as_ref()
        .and_then(|t| t.get::<Option<bool>>("clear_context").ok().flatten())
        .unwrap_or(false);
    let session = session_of(opts.as_ref())?;
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::Implement {
            clear_context,
            session,
        },
        reply_tx,
    })
    .await
}

/// Open the current plan file in `$EDITOR`, same as the "edit plan"
/// keybinding on the built-in form.
///
/// @param opts table? `session` (string?) Session id; defaults to focused.
/// @return (boolean|nil, string|nil) true once dispatched, or nil and an error.
/// @example
/// maki.plan.open_editor()
#[lua_fn]
async fn open_editor(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let session = session_of(opts.as_ref())?;
    ui_json_roundtrip(&lua, tx.as_ref(), |reply_tx| UiAction::Plan {
        req: PlanRequest::OpenEditor { session },
        reply_tx,
    })
    .await
}

lua_table! {
    /// Plan-mode surface for plugins.
    ///
    /// Read the plan without touching session internals, fire the built-in
    /// implement and edit outcomes, and put your own rows on the plan form
    /// with `maki.api.register_plan_action`. To render the plan yourself
    /// instead, layer the host's `ui.plan_form` slot: the default opens the
    /// built-in form, so a layer that answers without calling `prev` keeps
    /// it closed for as long as your plugin is loaded.
    ///
    /// Every call takes an optional `session` and defaults to the focused
    /// tab. Plan state is per session, so a handler reacting to `PlanReady`
    /// on a background tab has to pass `ev.data.session_id` through, or it
    /// reads whichever plan the user happens to be looking at.
    ///
    /// ```lua
    /// -- Own the plan UI for as long as this plugin is loaded:
    /// maki.api.set_slot("ui.plan_form", function(prev, ev)
    ///   local plan = maki.plan.read({ session = ev.session })
    ///   -- render plan.content in your own window
    ///   return false
    /// end)
    /// ```
    "maki.plan" => pub(crate) fn create_plan_table(tx: Option<flume::Sender<UiAction>>), DOCS [
        read(tx), implement(tx), open_editor(tx),
    ]
}

lua_table! {
    extend "maki.api" => pub(crate) fn add_plan_action_methods(plugin: Arc<str>), PLAN_ACTION_DOCS [
        register_plan_action(plugin), unregister_plan_action(plugin), clear_plan_actions(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::api::util::command::NO_UI_ERR;

    fn plugin() -> Arc<str> {
        Arc::from("test-plugin")
    }

    fn install_registry(lua: &Lua) -> PlanActionReader {
        lua.set_app_data(PlanActionHandlerMap::new());
        let (writer, reader) = PlanActionWriter::new();
        lua.set_app_data(writer);
        reader
    }

    fn load_actions(reader: &PlanActionReader) -> Vec<PlanActionInfo> {
        reader.load().actions.clone()
    }

    fn setup(lua: &Lua) -> PlanActionReader {
        let reader = install_registry(lua);
        let api = lua.create_table().unwrap();
        add_plan_action_methods(&api, lua, plugin()).unwrap();
        lua.globals().set("api", api).unwrap();
        reader
    }

    #[test]
    fn register_and_unregister_roundtrip() {
        let lua = Lua::new();
        let reader = setup(&lua);
        lua.load(
            r#"api.register_plan_action({
                name = "act", label = "Act", handler = function() end,
            })"#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name.as_ref(), "act");
        assert_eq!(actions[0].label.as_ref(), "Act");
        assert_eq!(actions[0].order, PLAN_ACTION_DEFAULT_ORDER);

        lua.load(r#"api.unregister_plan_action("act")"#)
            .exec()
            .unwrap();
        assert!(load_actions(&reader).is_empty());
    }

    #[test]
    fn register_replaces_same_name_within_plugin() {
        let lua = Lua::new();
        let reader = setup(&lua);
        lua.load(
            r#"
            api.register_plan_action({ name = "act", label = "first", handler = function() end })
            api.register_plan_action({ name = "act", label = "second", desc = "d", order = 42, handler = function() end })
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1, "duplicate name must replace, not stack");
        assert_eq!(actions[0].label.as_ref(), "second");
        assert_eq!(actions[0].desc.as_ref(), "d");
        assert_eq!(actions[0].order, 42);
    }

    #[test]
    fn different_plugins_can_share_a_name() {
        let lua = Lua::new();
        let reader = install_registry(&lua);
        let api_a = lua.create_table().unwrap();
        add_plan_action_methods(&api_a, &lua, Arc::from("plug-a")).unwrap();
        let api_b = lua.create_table().unwrap();
        add_plan_action_methods(&api_b, &lua, Arc::from("plug-b")).unwrap();
        lua.globals().set("a", api_a).unwrap();
        lua.globals().set("b", api_b).unwrap();

        lua.load(
            r#"
            a.register_plan_action({ name = "act", label = "A", handler = function() end })
            b.register_plan_action({ name = "act", label = "B", handler = function() end })
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 2);
        let plugins: Vec<_> = actions.iter().map(|a| a.plugin.as_ref()).collect();
        assert!(plugins.contains(&"plug-a"));
        assert!(plugins.contains(&"plug-b"));
    }

    #[test]
    fn clear_plan_actions_drops_all_for_plugin_only() {
        let lua = Lua::new();
        let reader = install_registry(&lua);
        let api_a = lua.create_table().unwrap();
        add_plan_action_methods(&api_a, &lua, Arc::from("plug-a")).unwrap();
        let api_b = lua.create_table().unwrap();
        add_plan_action_methods(&api_b, &lua, Arc::from("plug-b")).unwrap();
        lua.globals().set("a", api_a).unwrap();
        lua.globals().set("b", api_b).unwrap();

        lua.load(
            r#"
            a.register_plan_action({ name = "x", label = "X", handler = function() end })
            a.register_plan_action({ name = "y", label = "Y", handler = function() end })
            b.register_plan_action({ name = "z", label = "Z", handler = function() end })
            a.clear_plan_actions()
        "#,
        )
        .exec()
        .unwrap();

        let actions = load_actions(&reader);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].plugin.as_ref(), "plug-b");
        assert_eq!(actions[0].name.as_ref(), "z");
    }

    #[test]
    fn register_rejects_missing_name_label_handler() {
        let lua = Lua::new();
        setup(&lua);
        assert!(
            lua.load(r#"api.register_plan_action({ label = "L", handler = function() end })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(r#"api.register_plan_action({ name = "n", handler = function() end })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(r#"api.register_plan_action({ name = "n", label = "L" })"#)
                .exec()
                .is_err()
        );
        assert!(
            lua.load(
                r#"api.register_plan_action({ name = "", label = "L", handler = function() end })"#
            )
            .exec()
            .is_err()
        );
        assert!(
            lua.load(
                r#"api.register_plan_action({ name = "n", label = "", handler = function() end })"#
            )
            .exec()
            .is_err()
        );
    }

    #[test]
    fn register_returns_the_stable_id() {
        let lua = Lua::new();
        let reader = setup(&lua);
        let id: String = lua
            .load(
                r#"return api.register_plan_action({
                    name = "act", label = "Act", handler = function() end,
                })"#,
            )
            .eval()
            .unwrap();
        assert_eq!(id, "test-plugin/act");
        assert_eq!(load_actions(&reader)[0].id.as_ref(), id);
    }

    /// The map is a `HashMap` keyed by `Arc<str>`, and `RandomState` reseeds
    /// per process, so without a tiebreak the same two rows come out in a
    /// different order on the next run. Building the same set from two
    /// insertion orders stands in for that.
    #[test]
    fn snapshot_order_is_total_and_insertion_independent() {
        let register = |order: &[(&str, &str)]| {
            let lua = Lua::new();
            let reader = install_registry(&lua);
            for (plugin, name) in order {
                let api = lua.create_table().unwrap();
                add_plan_action_methods(&api, &lua, Arc::from(*plugin)).unwrap();
                lua.globals().set("api", api).unwrap();
                lua.load(format!(
                    r#"api.register_plan_action({{
                        name = "{name}", label = "{name}", handler = function() end,
                    }})"#
                ))
                .exec()
                .unwrap();
            }
            load_actions(&reader)
                .iter()
                .map(|a| a.id.to_string())
                .collect::<Vec<_>>()
        };

        let forward = register(&[("alpha", "a"), ("alpha", "b"), ("zeta", "a")]);
        let reversed = register(&[("zeta", "a"), ("alpha", "b"), ("alpha", "a")]);
        assert_eq!(forward, ["alpha/a", "alpha/b", "zeta/a"]);
        assert_eq!(forward, reversed);
    }

    /// `order` still wins; the names only break its ties.
    #[test]
    fn explicit_order_outranks_the_name_tiebreak() {
        let lua = Lua::new();
        let reader = install_registry(&lua);
        let api = lua.create_table().unwrap();
        add_plan_action_methods(&api, &lua, Arc::from("zeta")).unwrap();
        lua.globals().set("api", api).unwrap();
        lua.load(
            r#"
            api.register_plan_action({ name = "late", label = "L", order = 900, handler = function() end })
            api.register_plan_action({ name = "early", label = "E", order = 100, handler = function() end })
        "#,
        )
        .exec()
        .unwrap();
        let names: Vec<_> = load_actions(&reader)
            .iter()
            .map(|a| a.name.to_string())
            .collect();
        assert_eq!(names, ["early", "late"]);
    }

    #[test]
    fn plan_table_read_without_ui_returns_error_pair() {
        let lua = Lua::new();
        let table = create_plan_table(&lua, None).unwrap();
        lua.globals().set("plan", table).unwrap();
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return plan.read()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Answers one request from a background thread so the async Lua call can
    /// complete, handing the request back for assertions.
    fn serve_one(rx: flume::Receiver<UiAction>) -> std::thread::JoinHandle<PlanRequest> {
        std::thread::spawn(move || {
            let Ok(UiAction::Plan { req, reply_tx }) = rx.recv() else {
                panic!("expected a Plan UiAction");
            };
            reply_tx.send(Ok(serde_json::json!(true))).unwrap();
            req
        })
    }

    #[test]
    fn implement_forwards_clear_context_flag() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let served = serve_one(rx);
        let (val, err): (bool, Option<String>) = smol::block_on(
            lua.load("return plan.implement({ clear_context = true })")
                .eval_async(),
        )
        .unwrap();
        assert!(val);
        assert_eq!(err, None);
        match served.join().unwrap() {
            PlanRequest::Implement {
                clear_context,
                session,
            } => {
                assert!(clear_context);
                assert_eq!(session, None, "no session means the focused one");
            }
            _ => panic!("expected Plan/Implement"),
        }
    }

    #[test]
    fn open_editor_dispatches() {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let served = serve_one(rx);
        let (val, err): (bool, Option<String>) =
            smol::block_on(lua.load("return plan.open_editor()").eval_async()).unwrap();
        assert!(val);
        assert_eq!(err, None);
        assert!(matches!(
            served.join().unwrap(),
            PlanRequest::OpenEditor { session: None }
        ));
    }

    /// Plan state is per session, so every call has to be able to name one
    /// rather than assume whichever tab the user happens to be looking at.
    #[test_case("plan.read({ session = \"s1\" })" ; "read")]
    #[test_case("plan.implement({ session = \"s1\" })" ; "implement")]
    #[test_case("plan.open_editor({ session = \"s1\" })" ; "open_editor")]
    fn session_option_travels_to_the_ui(call: &str) {
        let lua = Lua::new();
        let (tx, rx) = flume::unbounded::<UiAction>();
        let table = create_plan_table(&lua, Some(tx)).unwrap();
        lua.globals().set("plan", table).unwrap();
        let served = serve_one(rx);
        smol::block_on(lua.load(format!("return {call}")).eval_async::<Value>()).unwrap();
        assert_eq!(served.join().unwrap().session(), Some("s1"));
    }
}
