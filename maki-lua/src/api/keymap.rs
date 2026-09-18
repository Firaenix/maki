use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_lua_macro::{lua_fn, lua_table};
use mlua::{AppDataRefMut, Lua, RegistryKey, Result as LuaResult, Table};

static NEXT_KEYMAP_ID: AtomicU64 = AtomicU64::new(1);

const NO_STORE_ERR: &str = "keymap store not initialized";
const RESERVED_KEY_ERR: &str = "is reserved by the host and would never reach this binding";

/// Keystrokes one plugin may have in flight before its bindings stop taking
/// keys. Each plugin is counted on its own, so one that parks in a callback
/// costs itself its keys and nobody else theirs.
const MAX_IN_FLIGHT: usize = 8;

/// The keys the host resolves before it looks at a binding at all: quitting
/// and suspending have to work whatever a handler is doing. Binding one would
/// publish a mapping that can never fire, so it is refused instead.
///
/// One list for all three sides of that promise: the host weighs a key
/// against [`is_reserved`] before it dispatches, `set` refuses the same
/// entries, and so does the `keys` an unfocused window claims, so no side can
/// grow a key the others do not know about.
pub const RESERVED_KEYS: [(KeyCode, KeyModifiers); 2] = [
    (KeyCode::Char('c'), KeyModifiers::CONTROL),
    (KeyCode::Char('z'), KeyModifiers::CONTROL),
];

/// Whether the host answers {key} itself, whatever any plugin bound.
pub fn is_reserved(key: KeyEvent) -> bool {
    RESERVED_KEYS.contains(&(key.code, key.modifiers))
}

/// What a key resolves to, handed to the Lua thread whole. Resolving by id
/// over there instead can find nothing, which leaves the UI having consumed a
/// key with nothing to act on it.
#[derive(Clone, Debug)]
struct Keybind {
    callback: Arc<RegistryKey>,
    /// Shared by every binding of one plugin.
    in_flight: Arc<AtomicUsize>,
    /// Shared by every binding of one plugin, and cleared when its load is
    /// torn down.
    live: Arc<AtomicBool>,
}

/// A keystroke on its way to the Lua thread, holding one slot of its plugin's
/// budget until the callback finishes.
pub struct KeybindTicket {
    callback: Arc<RegistryKey>,
    in_flight: Arc<AtomicUsize>,
    live: Arc<AtomicBool>,
    plugin: Arc<str>,
    key: KeyEvent,
}

impl KeybindTicket {
    /// Refuses the key when the plugin's load is gone or its budget is full.
    /// Both answers are the host's to act on in the same key turn: it runs the
    /// built-in binding instead, rather than handing the key to a callback
    /// that cannot answer it.
    fn claim(entry: &KeymapEntry, key: KeyEvent) -> Option<Self> {
        let bind = &entry.bind;
        if !bind.live.load(Ordering::Acquire) {
            return None;
        }
        bind.in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_IN_FLIGHT).then_some(n + 1)
            })
            .ok()?;
        Some(Self {
            callback: Arc::clone(&bind.callback),
            in_flight: Arc::clone(&bind.in_flight),
            live: Arc::clone(&bind.live),
            plugin: Arc::clone(&entry.plugin),
            key,
        })
    }

    pub fn callback(&self) -> &RegistryKey {
        &self.callback
    }

    /// Whether the load that registered this binding is still the one in
    /// place. Read again on the Lua thread: a `/reload` can land between the
    /// claim and the call, and the plugin it tore down must run nothing.
    pub fn plugin_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// The load that registered the binding. An [`Arc`] rather than a borrow
    /// because the logs that name it run after the ticket has been handed on.
    pub fn plugin(&self) -> &Arc<str> {
        &self.plugin
    }

    /// The keystroke the host consumed to get here, for the log on the one
    /// path that loses it: a callback that cannot be reached at all.
    pub fn key(&self) -> KeyEvent {
        self.key
    }
}

impl Drop for KeybindTicket {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug)]
pub struct KeymapEntry {
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub desc: String,
    pub plugin: Arc<str>,
    pub id: u64,
    bind: Keybind,
}

#[derive(Clone, Default)]
pub struct KeymapSnapshot {
    pub entries: Vec<KeymapEntry>,
    pub generation: u64,
}

#[derive(Clone)]
pub struct KeymapReader(Arc<ArcSwap<KeymapSnapshot>>);

impl KeymapReader {
    pub fn empty() -> Self {
        Self(Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default())))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<KeymapSnapshot>> {
        self.0.load()
    }

    /// Hands {key} to the binding on it and reports whether it was taken.
    ///
    /// A key nothing took is reported untaken, and the host runs its own
    /// binding for it in the same key turn. That is the whole of fall-through:
    /// no key is ever handed back afterwards, because a keystroke replayed
    /// into a UI that has moved on lands somewhere the user never aimed it. A
    /// plugin with too many callbacks in flight, or one whose load is gone, is
    /// a plugin that took nothing, settled here before any Lua runs.
    ///
    /// These are global bindings, live until the plugin drops them. A key a
    /// popup should own only while it is on screen is not one of them: it is
    /// declared in the `keys` of `maki.ui.open_win`, and the host routes it to
    /// that window before it ever looks here.
    pub fn dispatch(&self, key: KeyEvent, run: impl FnOnce(KeybindTicket) -> bool) -> bool {
        let snapshot = self.0.load();
        let ticket = snapshot
            .entries
            .iter()
            .find(|e| e.key == key.code && e.modifiers == key.modifiers)
            .and_then(|entry| KeybindTicket::claim(entry, key));
        ticket.is_some_and(run)
    }
}

pub(crate) struct KeymapWriter {
    store: Arc<ArcSwap<KeymapSnapshot>>,
    generation: AtomicU64,
}

impl KeymapWriter {
    pub fn new() -> (Self, KeymapReader) {
        let inner = Arc::new(ArcSwap::from_pointee(KeymapSnapshot::default()));
        (
            Self {
                store: Arc::clone(&inner),
                generation: AtomicU64::new(0),
            },
            KeymapReader(inner),
        )
    }

    pub fn publish(&self, entries: Vec<KeymapEntry>) {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.store.store(Arc::new(KeymapSnapshot {
            entries,
            generation,
        }));
    }
}

pub(crate) struct StoredKeymap {
    id: u64,
    key: KeyCode,
    modifiers: KeyModifiers,
    /// Dropping the last reference hands the registry slot back to mlua, which
    /// the next binding reuses, so a callback an in-flight keystroke still
    /// holds frees itself once that keystroke is done.
    callback: Arc<RegistryKey>,
    plugin: Arc<str>,
    desc: String,
    state: PluginDispatch,
}

/// What every binding of one plugin shares: the keystroke budget they are
/// counted against, and whether the load that registered them is still the one
/// in place.
#[derive(Clone)]
struct PluginDispatch {
    in_flight: Arc<AtomicUsize>,
    live: Arc<AtomicBool>,
}

impl Default for PluginDispatch {
    fn default() -> Self {
        Self {
            in_flight: Arc::default(),
            live: Arc::new(AtomicBool::new(true)),
        }
    }
}

pub(crate) struct KeymapStore {
    globals: Vec<StoredKeymap>,
    plugins: HashMap<Arc<str>, PluginDispatch>,
}

impl KeymapStore {
    pub fn new() -> Self {
        Self {
            globals: Vec::new(),
            plugins: HashMap::new(),
        }
    }

    /// The budget and the liveness every binding of {plugin} shares. A name
    /// [`Self::clear_plugin`] tombstoned gets that dead state back rather than
    /// a fresh live one: between a `/reload` and the load that replaces it, a
    /// handler of the load that is gone can still be running and still call
    /// `set`, and the `clear_plugin` that would have taken those bindings away
    /// has already run.
    fn plugin_state(&mut self, plugin: &Arc<str>) -> PluginDispatch {
        self.plugins.entry(Arc::clone(plugin)).or_default().clone()
    }

    /// Whether a global binding was replaced.
    pub fn set(
        &mut self,
        key: KeyCode,
        modifiers: KeyModifiers,
        callback: RegistryKey,
        plugin: Arc<str>,
        desc: String,
    ) -> bool {
        let state = self.plugin_state(&plugin);
        let replaced = self
            .globals
            .iter()
            .any(|b| b.key == key && b.modifiers == modifiers);
        self.globals
            .retain(|b| b.key != key || b.modifiers != modifiers);
        self.globals.push(StoredKeymap {
            id: NEXT_KEYMAP_ID.fetch_add(1, Ordering::Relaxed),
            key,
            modifiers,
            callback: Arc::new(callback),
            plugin,
            desc,
            state,
        });
        replaced
    }

    pub fn del(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        self.globals
            .retain(|b| b.key != key || b.modifiers != modifiers);
    }

    /// The load is marked dead as well as emptied of keys. The snapshot loses
    /// its bindings either way, but a keystroke already on its way to the Lua
    /// thread carries its callback with it, and that callback belongs to the
    /// chunk this tore down.
    ///
    /// The name is tombstoned rather than forgotten. A keybind handler holds
    /// its budget slot for a bounded time and the drain barrier waits no
    /// longer, so a `/reload` can land while one is still running; that
    /// handler can call `set`, and a forgotten name would hand it a fresh live
    /// state and publish bindings for a load that is gone, which the
    /// `clear_plugin` that already ran will never clear. [`Self::revive`] is
    /// the one way back.
    pub fn clear_plugin(&mut self, plugin: &str) {
        self.globals.retain(|b| b.plugin.as_ref() != plugin);
        if let Some(state) = self.plugins.get(plugin) {
            state.live.store(false, Ordering::Release);
        }
    }

    /// Takes the tombstone off {plugin}, for the load that is about to
    /// register its bindings. Called before that load's chunks run and
    /// nowhere else, which is what tells a new load apart from a handler of
    /// the old one. Anything the old load's stragglers published in between
    /// goes with it: those bindings are dead and belong to nothing.
    pub fn revive(&mut self, plugin: &str) {
        self.globals.retain(|b| b.plugin.as_ref() != plugin);
        self.plugins.remove(plugin);
    }

    /// Every global binding, in dispatch order, which is also the order the
    /// keymap listing and the help modal read them in.
    pub fn snapshot_entries(&self) -> Vec<KeymapEntry> {
        self.globals
            .iter()
            .map(|b| KeymapEntry {
                key: b.key,
                modifiers: b.modifiers,
                desc: b.desc.clone(),
                plugin: Arc::clone(&b.plugin),
                id: b.id,
                bind: Keybind {
                    callback: Arc::clone(&b.callback),
                    in_flight: Arc::clone(&b.state.in_flight),
                    live: Arc::clone(&b.state.live),
                },
            })
            .collect()
    }
}

pub fn parse_key_notation(input: &str) -> Result<(KeyCode, KeyModifiers), String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty key notation".into());
    }

    if s.starts_with('<') && s.ends_with('>') {
        let inner = &s[1..s.len() - 1];
        return parse_bracketed(inner);
    }

    if s.len() == 1 {
        let c = s.chars().next().unwrap();
        return Ok((KeyCode::Char(c), KeyModifiers::NONE));
    }

    Err(format!("invalid key notation: {s}"))
}

fn parse_bracketed(inner: &str) -> Result<(KeyCode, KeyModifiers), String> {
    if inner.is_empty() {
        return Err("empty angle-bracket key notation".into());
    }

    let mut modifiers = KeyModifiers::NONE;
    let mut rest = inner;

    loop {
        let lower = rest.to_lowercase();
        if lower.starts_with("c-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[2..];
        } else if lower.starts_with("ctrl-") {
            modifiers |= KeyModifiers::CONTROL;
            rest = &rest[5..];
        } else if lower.starts_with("a-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("alt-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[4..];
        } else if lower.starts_with("m-") {
            modifiers |= KeyModifiers::ALT;
            rest = &rest[2..];
        } else if lower.starts_with("s-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[2..];
        } else if lower.starts_with("shift-") {
            modifiers |= KeyModifiers::SHIFT;
            rest = &rest[6..];
        } else {
            break;
        }
    }

    let key = parse_key_name(rest)?;
    Ok((key, modifiers))
}

fn parse_key_name(name: &str) -> Result<KeyCode, String> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        "cr" | "enter" | "return" => Ok(KeyCode::Enter),
        "space" => Ok(KeyCode::Char(' ')),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "bs" | "backspace" => Ok(KeyCode::Backspace),
        "del" | "delete" => Ok(KeyCode::Delete),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "insert" => Ok(KeyCode::Insert),
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| format!("invalid function key: {name}"))?;
            if !(1..=12).contains(&n) {
                return Err(format!("function key out of range: {name}"));
            }
            Ok(KeyCode::F(n))
        }
        _ => {
            if name.len() == 1 {
                Ok(KeyCode::Char(name.chars().next().unwrap()))
            } else {
                Err(format!("unknown key: {name}"))
            }
        }
    }
}

fn publish_keymap_snapshot(lua: &Lua) {
    let Some(store) = lua.app_data_ref::<KeymapStore>() else {
        return;
    };
    let entries = store.snapshot_entries();
    drop(store);
    if let Some(writer) = lua.app_data_ref::<KeymapWriter>() {
        writer.publish(entries);
    }
}

/// Refuses a key the host answers itself, so a binding that could never fire
/// is an error the plugin author reads instead of a mapping that silently
/// never runs. One list, [`RESERVED_KEYS`], answers this, the `keys` a window
/// claims, and the host.
pub(crate) fn reject_reserved(lhs: &str, key: KeyCode, modifiers: KeyModifiers) -> LuaResult<()> {
    if RESERVED_KEYS.contains(&(key, modifiers)) {
        return Err(mlua::Error::runtime(format!("{lhs} {RESERVED_KEY_ERR}")));
    }
    Ok(())
}

fn store_mut(lua: &Lua) -> LuaResult<AppDataRefMut<'_, KeymapStore>> {
    lua.app_data_mut::<KeymapStore>()
        .ok_or_else(|| mlua::Error::runtime(NO_STORE_ERR))
}

/// Bind a key to a Lua function, just like `vim.keymap.set`. Only
/// normal mode (`"n"`) is supported right now. If {lhs} is already
/// mapped, the old binding is replaced and a warning is logged.
///
/// The binding is global and lasts until `del` or the plugin unloads. For a
/// key a popup should own only while it is on screen, declare it in the
/// `keys` of `maki.ui.open_win` instead: the host routes it to that window
/// and hands it back when the window closes.
///
/// A handler that runs owns the key. Its return value is not read, and a
/// handler that raises is logged with the key spent all the same: a keystroke
/// replayed once the UI has moved on lands somewhere the user never aimed it.
/// The key reaches the binding underneath only when the host could not
/// dispatch it at all, which it settles before any of your Lua runs.
///
/// `<C-c>` and `<C-z>` are the two keys no binding takes: quitting and
/// suspending have to work whatever a plugin is doing. Binding one is an
/// error rather than a mapping that never fires.
///
/// @param mode string Mode letter. Currently only `"n"` is accepted.
/// @param lhs string Key in Vim notation, e.g. `"<C-t>"`, `"<Space>"`, `"a"`.
/// @param rhs function Called when the key is pressed. Its return value is not read.
/// @param opts table? Options:
///   `desc` (string) short description shown in the keymap list.
/// @example
/// maki.keymap.set("n", "<C-t>", function()
///   print("toggle!")
/// end, { desc = "Toggle panel" })
#[lua_fn]
fn set(
    lua: &Lua,
    #[ctx] plugin: Arc<str>,
    mode: String,
    lhs: String,
    rhs: mlua::Function,
    opts: Option<Table>,
) -> LuaResult<()> {
    if mode != "n" {
        return Err(mlua::Error::runtime(format!(
            "unsupported keymap mode: {mode}"
        )));
    }
    let (key, modifiers) = parse_key_notation(&lhs).map_err(mlua::Error::runtime)?;
    reject_reserved(&lhs, key, modifiers)?;
    let desc = opts
        .as_ref()
        .and_then(|o| o.get::<String>("desc").ok())
        .unwrap_or_default();
    let registry_key = lua.create_registry_value(rhs)?;
    let shadowed = store_mut(lua)?.set(key, modifiers, registry_key, Arc::clone(&plugin), desc);
    if shadowed {
        tracing::warn!(key = %lhs, plugin = %plugin, "keymap shadowed by plugin");
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

/// Remove the mapping for {lhs} in {mode}. Does nothing if no mapping
/// exists for that key.
///
/// @param mode string Mode letter (reserved for future modes).
/// @param lhs string Key to unmap, in Vim notation.
/// @example
/// maki.keymap.del("n", "<C-t>")
#[lua_fn]
fn del(lua: &Lua, #[ctx] plugin: Arc<str>, mode: String, lhs: String) -> LuaResult<()> {
    let _ = (mode, &plugin);
    let (key, modifiers) = parse_key_notation(&lhs).map_err(mlua::Error::runtime)?;
    if let Some(mut store) = lua.app_data_mut::<KeymapStore>() {
        store.del(key, modifiers);
    }
    publish_keymap_snapshot(lua);
    Ok(())
}

lua_table! {
    /// Key mappings, modeled after `vim.keymap`. If you have written a
    /// Neovim keymap plugin before, this will feel familiar.
    ///
    /// `set` claims a key for the rest of the run. A key a popup should own
    /// only while it is on screen belongs in the `keys` of
    /// `maki.ui.open_win`, which routes it to that window and hands it back
    /// when the window closes.
    ///
    /// ```lua
    /// maki.keymap.set("n", "<C-t>", function()
    ///   print("hello")
    /// end, { desc = "Say hello" })
    /// ```
    "maki.keymap" => pub(crate) fn create_keymap_table(plugin: Arc<str>), DOCS [
        set(plugin), del(plugin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};
    use test_case::test_case;

    #[test_case("<C-t>", KeyCode::Char('t'), KeyModifiers::CONTROL ; "ctrl_t")]
    #[test_case("<C-T>", KeyCode::Char('T'), KeyModifiers::CONTROL ; "ctrl_shift_t")]
    #[test_case("<A-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "alt_x")]
    #[test_case("<M-x>", KeyCode::Char('x'), KeyModifiers::ALT ; "meta_x")]
    #[test_case("<S-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_tab")]
    #[test_case("<CR>", KeyCode::Enter, KeyModifiers::NONE ; "enter_cr")]
    #[test_case("<Enter>", KeyCode::Enter, KeyModifiers::NONE ; "enter_full")]
    #[test_case("<Space>", KeyCode::Char(' '), KeyModifiers::NONE ; "space")]
    #[test_case("<Esc>", KeyCode::Esc, KeyModifiers::NONE ; "escape")]
    #[test_case("<Tab>", KeyCode::Tab, KeyModifiers::NONE ; "tab")]
    #[test_case("<BS>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_short")]
    #[test_case("<Backspace>", KeyCode::Backspace, KeyModifiers::NONE ; "backspace_full")]
    #[test_case("<Del>", KeyCode::Delete, KeyModifiers::NONE ; "delete_short")]
    #[test_case("<Delete>", KeyCode::Delete, KeyModifiers::NONE ; "delete_full")]
    #[test_case("<Up>", KeyCode::Up, KeyModifiers::NONE ; "up")]
    #[test_case("<Down>", KeyCode::Down, KeyModifiers::NONE ; "down")]
    #[test_case("<Left>", KeyCode::Left, KeyModifiers::NONE ; "left")]
    #[test_case("<Right>", KeyCode::Right, KeyModifiers::NONE ; "right")]
    #[test_case("<Home>", KeyCode::Home, KeyModifiers::NONE ; "home")]
    #[test_case("<End>", KeyCode::End, KeyModifiers::NONE ; "end_key")]
    #[test_case("<PageUp>", KeyCode::PageUp, KeyModifiers::NONE ; "page_up")]
    #[test_case("<PageDown>", KeyCode::PageDown, KeyModifiers::NONE ; "page_down")]
    #[test_case("<Insert>", KeyCode::Insert, KeyModifiers::NONE ; "insert")]
    #[test_case("<F1>", KeyCode::F(1), KeyModifiers::NONE ; "f1")]
    #[test_case("<F12>", KeyCode::F(12), KeyModifiers::NONE ; "f12")]
    #[test_case("a", KeyCode::Char('a'), KeyModifiers::NONE ; "plain_a")]
    #[test_case("z", KeyCode::Char('z'), KeyModifiers::NONE ; "plain_z")]
    #[test_case("<C-S-a>", KeyCode::Char('a'), KeyModifiers::from_bits_truncate(KeyModifiers::CONTROL.bits() | KeyModifiers::SHIFT.bits()) ; "ctrl_shift_a")]
    #[test_case("<Ctrl-x>", KeyCode::Char('x'), KeyModifiers::CONTROL ; "ctrl_long_x")]
    #[test_case("<Alt-j>", KeyCode::Char('j'), KeyModifiers::ALT ; "alt_long_j")]
    #[test_case("<Shift-Tab>", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_long_tab")]
    #[test_case("<Return>", KeyCode::Enter, KeyModifiers::NONE ; "return_key")]
    #[test_case("<Escape>", KeyCode::Esc, KeyModifiers::NONE ; "escape_full")]
    fn parse_key_notation_cases(input: &str, code: KeyCode, mods: KeyModifiers) {
        let (key, modifiers) = parse_key_notation(input).unwrap();
        assert_eq!(key, code);
        assert_eq!(modifiers, mods);
    }

    #[test]
    fn parse_key_notation_errors() {
        assert!(parse_key_notation("").is_err());
        assert!(parse_key_notation("<>").is_err());
        assert!(parse_key_notation("<F0>").is_err());
        assert!(parse_key_notation("<F13>").is_err());
        assert!(parse_key_notation("abc").is_err());
    }

    const PLUGIN: &str = "plug";
    const OTHER_PLUGIN: &str = "other";
    const TAB: KeyCode = KeyCode::Tab;
    const NONE: KeyModifiers = KeyModifiers::NONE;

    fn global(store: &mut KeymapStore, lua: &Lua, key: KeyCode, plugin: &str) {
        let f = lua.create_function(|_, ()| Ok(())).unwrap();
        let k = lua.create_registry_value(f).unwrap();
        store.set(key, NONE, k, Arc::from(plugin), String::new());
    }

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn published(store: &KeymapStore) -> KeymapReader {
        let (writer, reader) = KeymapWriter::new();
        writer.publish(store.snapshot_entries());
        reader
    }

    #[test]
    fn keymap_store_set_and_shadow() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, KeyCode::Char('t'), PLUGIN);
        global(&mut store, &lua, KeyCode::Char('t'), OTHER_PLUGIN);
        assert_eq!(store.globals.len(), 1);
    }

    #[test]
    fn keymap_store_del() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, KeyCode::Char('x'), PLUGIN);
        store.del(KeyCode::Char('x'), NONE);
        assert!(store.globals.is_empty());
    }

    #[test]
    fn keymap_store_clear_plugin() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();

        global(&mut store, &lua, KeyCode::Char('t'), PLUGIN);
        global(&mut store, &lua, KeyCode::Char('x'), OTHER_PLUGIN);

        store.clear_plugin(PLUGIN);
        assert_eq!(store.globals.len(), 1);
        assert_eq!(store.globals[0].plugin.as_ref(), OTHER_PLUGIN);
    }

    #[test]
    fn snapshot_reader_writer() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        let (writer, reader) = KeymapWriter::new();
        assert!(reader.load().entries.is_empty());

        global(&mut store, &lua, TAB, PLUGIN);
        writer.publish(store.snapshot_entries());

        let snap = reader.load();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.generation, 1);
    }

    /// The key is consumed the moment `dispatch` says it was claimed, so a
    /// hand-off that did not happen has to report so and let the built-in
    /// binding run.
    #[test_case(true  => true  ; "claimed_when_the_callback_was_reached")]
    #[test_case(false => false ; "falls_through_when_it_was_not")]
    fn dispatch_reports_whether_the_key_was_claimed(handed_off: bool) -> bool {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        published(&store).dispatch(key_event(TAB), |_| handed_off)
    }

    #[test]
    fn dispatch_leaves_a_key_nobody_claimed_alone() {
        let store = KeymapStore::new();
        assert!(!published(&store).dispatch(key_event(TAB), |_| unreachable!()));
    }

    /// A callback that parks holds its ticket, and the plugin that owns it
    /// runs out of budget. Its keys then reach the built-in binding, and every
    /// other plugin keeps dispatching.
    #[test]
    fn a_parked_plugin_stops_claiming_keys_and_leaves_the_others_alone() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        global(&mut store, &lua, KeyCode::Esc, OTHER_PLUGIN);
        let reader = published(&store);

        let parked = exhaust(&reader, TAB);

        assert!(
            !reader.dispatch(key_event(TAB), |_| unreachable!()),
            "the parked plugin is out of budget"
        );
        assert!(
            reader.dispatch(key_event(KeyCode::Esc), |_| true),
            "another plugin's keys still dispatch"
        );

        drop(parked);
        assert!(
            reader.dispatch(key_event(TAB), |_| true),
            "finishing the callbacks gives the budget back"
        );
    }

    /// Fills {plugin}'s budget and returns the tickets holding it.
    fn exhaust(reader: &KeymapReader, key: KeyCode) -> Vec<KeybindTicket> {
        (0..MAX_IN_FLIGHT)
            .map(|_| {
                let mut held = None;
                assert!(reader.dispatch(key_event(key), |t| {
                    held = Some(t);
                    true
                }));
                held.unwrap()
            })
            .collect()
    }

    /// A binding that cannot run its callback is a binding that is not there,
    /// and the host runs its own in the same keystroke. Swallowing the key
    /// instead leaves the user pressing a key nothing will answer.
    #[test]
    fn an_exhausted_binding_falls_through() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        let reader = published(&store);

        let _parked = exhaust(&reader, TAB);

        assert!(!reader.dispatch(key_event(TAB), |_| unreachable!()));
    }

    /// A keystroke claimed a moment before a `/reload` carries the old chunk's
    /// callback with it. Its plugin is gone, so the host has to be told here,
    /// while it can still run the built-in binding for the key.
    #[test]
    fn a_binding_whose_plugin_was_torn_down_claims_nothing() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);
        let reader = published(&store);

        let mut live = None;
        assert!(reader.dispatch(key_event(TAB), |t| {
            live = Some(t.plugin_live());
            true
        }));
        assert_eq!(live, Some(true));

        store.clear_plugin(PLUGIN);
        assert!(
            !reader.dispatch(key_event(TAB), |_| unreachable!()),
            "the stale snapshot no longer claims the key"
        );
    }

    /// A ticket outlives the snapshot it came from, so the answer has to travel
    /// with it: a `/reload` between the claim and the call must reach the
    /// callback that is about to run.
    #[test]
    fn a_ticket_reports_the_teardown_that_landed_after_it_was_claimed() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut held = None;
        published(&store).dispatch(key_event(TAB), |t| {
            held = Some(t);
            true
        });
        let ticket = held.unwrap();
        assert!(ticket.plugin_live());
        assert_eq!(ticket.plugin().as_ref(), PLUGIN);

        store.clear_plugin(PLUGIN);
        assert!(!ticket.plugin_live());
    }

    /// The handler runs on another thread, so the key the host consumed travels
    /// with the ticket: it is what the log names when the callback cannot be
    /// reached at all.
    #[test]
    fn a_ticket_carries_the_key_it_was_claimed_for() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        let mut carried = None;
        published(&store).dispatch(key_event(TAB), |t| {
            carried = Some(t.key());
            true
        });
        assert_eq!(carried, Some(key_event(TAB)));
    }

    /// A reserved key binds nowhere. Accepting it publishes a binding with a
    /// `desc` in the keymap list that can never fire, which is a bug report
    /// about maki rather than about the plugin that wrote it.
    #[test_case("<C-c>" ; "quit")]
    #[test_case("<C-z>" ; "suspend")]
    fn a_reserved_key_is_refused_where_the_author_can_see_it(lhs: &str) {
        let (key, modifiers) = parse_key_notation(lhs).unwrap();
        let err = reject_reserved(lhs, key, modifiers)
            .unwrap_err()
            .to_string();
        assert!(err.contains(RESERVED_KEY_ERR), "got: {err}");
        assert!(
            err.contains(lhs),
            "the error has to name the key, got: {err}"
        );
    }

    /// A tombstone, not a removal: a handler of the load that is gone can
    /// still be running when `/reload` lands, and the bindings it publishes on
    /// its way out belong to nothing. The load that replaces it starts live.
    #[test]
    fn a_torn_down_plugin_cannot_publish_live_bindings_until_it_is_reloaded() {
        let lua = Lua::new();
        let mut store = KeymapStore::new();
        global(&mut store, &lua, TAB, PLUGIN);

        store.clear_plugin(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            !published(&store).dispatch(key_event(TAB), |_| unreachable!()),
            "a straggler of the torn down load publishes nothing that fires"
        );

        store.revive(PLUGIN);
        global(&mut store, &lua, TAB, PLUGIN);
        assert!(
            published(&store).dispatch(key_event(TAB), |_| true),
            "the load that replaced it dispatches"
        );
    }
}
