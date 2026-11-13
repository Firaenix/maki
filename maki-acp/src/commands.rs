//! Slash commands over ACP. A prompt whose text opens with a known `/name` runs
//! that command instead of reaching the model; anything else, a path like
//! `/usr/bin` included, stays a prompt. Clients learn the names from
//! `available_commands_update`, spelled without the slash as ACP expects.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    AvailableCommand, AvailableCommandInput, UnstructuredCommandInput,
};
use futures::future::BoxFuture;
use maki_agent::command::CustomCommand;
use maki_agent::{McpHandle, McpPromptArg, McpPromptInfo};

pub const NEEDS_TUI_SUFFIX: &str = " (TUI only)";
const ARGUMENTS_HINT: &str = "arguments";

/// A builtin as the host's command table describes it, slash included.
pub struct HostBuiltin {
    pub name: &'static str,
    pub description: &'static str,
    pub max_args: usize,
}

#[derive(Clone)]
pub struct LuaCommand {
    pub plugin: Arc<str>,
    pub name: Arc<str>,
    pub description: Arc<str>,
    pub max_args: usize,
    /// Only opens a picker or modal, which an ACP client cannot show.
    pub interactive: bool,
}

/// What only the host process can answer: its command table and its Lua
/// plugins. `maki-acp` depends on neither the TUI nor the Lua runtime.
pub trait CommandHost: Send + Sync {
    fn builtins(&self) -> Vec<HostBuiltin>;
    /// Markdown commands for {cwd}, or none when the host turned them off.
    fn custom_commands(&self, cwd: &Path) -> Vec<CustomCommand>;
    fn lua_commands(&self) -> Vec<LuaCommand>;
    /// {session} is the chat the handler runs in, its `opts.session`.
    fn run_lua(&self, command: &LuaCommand, args: String, session: String);
    /// Prepares and applies a package command, answering with the line to
    /// report. It never exits the process the way the TUI does.
    fn run_pack(&self, name: &str, args: &str, bang: bool) -> BoxFuture<'static, String>;
}

/// The builtins an ACP session runs itself. Every other host builtin is
/// listed as needing the TUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Compact,
    Btw,
    Cd,
    Yolo,
    Fast,
    Workflow,
    Usage,
    Reload,
    Trust,
    PackUpdate,
    PackDel,
    Exit,
}

impl Builtin {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "compact" => Self::Compact,
            "btw" => Self::Btw,
            "cd" => Self::Cd,
            "yolo" => Self::Yolo,
            "fast" => Self::Fast,
            "workflow" => Self::Workflow,
            "usage" => Self::Usage,
            "reload" => Self::Reload,
            "trust" => Self::Trust,
            "packupdate" => Self::PackUpdate,
            "packdel" => Self::PackDel,
            "exit" => Self::Exit,
            _ => return None,
        })
    }

    fn hint(self) -> Option<&'static str> {
        match self {
            Self::Compact => Some("[instructions]"),
            Self::Btw => Some("<question>"),
            Self::Cd => Some("[path]"),
            Self::PackUpdate => Some("[++lockfile] [names]"),
            Self::PackDel => Some("[++all | name]"),
            _ => None,
        }
    }

    /// Restarts the session under it, so it may not cut a running turn.
    pub fn restarts_session(self) -> bool {
        matches!(self, Self::Cd | Self::Reload | Self::Trust | Self::Exit)
    }
}

/// A typed command line: `name` has neither the slash nor the `!`.
#[derive(Debug, PartialEq, Eq)]
pub struct Slash<'a> {
    pub name: &'a str,
    pub bang: bool,
    pub args: &'a str,
}

pub fn parse(text: &str) -> Option<Slash<'_>> {
    let rest = text.trim().strip_prefix('/')?;
    let (word, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (name, bang) = word
        .strip_suffix('!')
        .map_or((word, false), |name| (name, true));
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(Slash {
        name,
        bang,
        args: args.trim(),
    })
}

pub enum Target<'a> {
    Builtin(Builtin),
    /// A host builtin or plugin command that only works in the TUI.
    NeedsTui(String),
    Custom(&'a CustomCommand),
    Mcp(&'a McpPromptInfo),
    Lua(&'a LuaCommand),
}

/// Every command a session can name, read fresh for its cwd and MCP servers.
#[derive(Default)]
pub struct Catalogue {
    builtins: Vec<HostBuiltin>,
    custom: Vec<CustomCommand>,
    mcp: Vec<McpPromptInfo>,
    lua: Vec<LuaCommand>,
}

impl Catalogue {
    pub fn gather(host: &dyn CommandHost, cwd: &Path, mcp: Option<&McpHandle>) -> Self {
        Self {
            builtins: host.builtins(),
            custom: host.custom_commands(cwd),
            mcp: mcp
                .map(|mcp| mcp.reader().load().prompts.clone())
                .unwrap_or_default(),
            lua: host.lua_commands(),
        }
    }

    /// Exact and case-insensitive, like typing in the TUI. Never fuzzy: a typo
    /// should reach the model as text rather than run a neighbour.
    pub fn resolve(&self, name: &str) -> Option<Target<'_>> {
        let is = |candidate: &str| acp_name(candidate).eq_ignore_ascii_case(name);
        if let Some(builtin) = self.builtins.iter().find(|b| is(b.name)) {
            let bare = acp_name(builtin.name);
            return Some(match Builtin::from_name(bare) {
                Some(builtin) => Target::Builtin(builtin),
                None => Target::NeedsTui(bare.to_owned()),
            });
        }
        if let Some(custom) = self.custom.iter().find(|c| is(&c.display_name())) {
            return Some(Target::Custom(custom));
        }
        if let Some(prompt) = self.mcp.iter().find(|p| is(&p.display_name)) {
            return Some(Target::Mcp(prompt));
        }
        let lua = self.lua.iter().find(|c| is(&c.name))?;
        Some(match lua.interactive {
            true => Target::NeedsTui(acp_name(&lua.name).to_owned()),
            false => Target::Lua(lua),
        })
    }

    pub fn available(&self) -> Vec<AvailableCommand> {
        let builtins = self.builtins.iter().map(|b| {
            let name = acp_name(b.name);
            match Builtin::from_name(name) {
                Some(builtin) => command(name, b.description, builtin.hint()),
                None => needs_tui(name, b.description),
            }
        });
        let custom = self.custom.iter().map(|c| {
            let hint = c
                .argument_hint
                .as_deref()
                .or(c.accepts_args.then_some(ARGUMENTS_HINT));
            command(acp_name(&c.display_name()), &c.description, hint)
        });
        let mcp = self.mcp.iter().map(|p| {
            let hint = mcp_argument_hint(&p.arguments);
            command(&p.display_name, &p.description, hint.as_deref())
        });
        let lua = self.lua.iter().map(|c| {
            let name = acp_name(&c.name);
            match c.interactive {
                true => needs_tui(name, &c.description),
                false => command(
                    name,
                    &c.description,
                    (c.max_args > 0).then_some(ARGUMENTS_HINT),
                ),
            }
        });
        builtins.chain(custom).chain(mcp).chain(lua).collect()
    }
}

fn acp_name(name: &str) -> &str {
    name.strip_prefix('/').unwrap_or(name)
}

fn command(name: &str, description: &str, hint: Option<&str>) -> AvailableCommand {
    let input = hint.map(|hint| {
        AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(hint.to_owned()))
    });
    AvailableCommand::new(name, description).input(input)
}

fn needs_tui(name: &str, description: &str) -> AvailableCommand {
    AvailableCommand::new(name, format!("{description}{NEEDS_TUI_SUFFIX}"))
}

/// `<required> [optional]`, the shape markdown commands spell by hand.
fn mcp_argument_hint(args: &[McpPromptArg]) -> Option<String> {
    let words: Vec<String> = args
        .iter()
        .map(|a| match a.required {
            true => format!("<{}>", a.name),
            false => format!("[{}]", a.name),
        })
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// Fills the prompt's arguments in order, the last one taking the rest of the
/// line, so a free-text argument needs no quoting.
pub fn mcp_arguments(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut remaining = args.trim();
    let last = prompt.arguments.len().saturating_sub(1);
    for (i, arg) in prompt.arguments.iter().enumerate() {
        if remaining.is_empty() {
            break;
        }
        let (word, rest) = match i == last {
            true => (remaining, ""),
            false => remaining
                .split_once(char::is_whitespace)
                .unwrap_or((remaining, "")),
        };
        result.insert(arg.name.clone(), word.to_owned());
        remaining = rest.trim_start();
    }
    result
}

/// The required arguments {arguments} leaves out, as `<name>` words.
pub fn missing_mcp_arguments(
    prompt: &McpPromptInfo,
    arguments: &HashMap<String, String>,
) -> Vec<String> {
    prompt
        .arguments
        .iter()
        .filter(|a| a.required && !arguments.contains_key(&a.name))
        .map(|a| format!("<{}>", a.name))
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    use maki_agent::command::CommandScope;

    use super::*;

    /// Four headless builtins and two TUI-only ones, a markdown command, and a
    /// plain and an interactive Lua command.
    pub(crate) struct Host;

    impl CommandHost for Host {
        fn builtins(&self) -> Vec<HostBuiltin> {
            ["/compact", "/yolo", "/workflow", "/exit", "/theme", "/fork"]
                .into_iter()
                .map(|name| HostBuiltin {
                    name,
                    description: "d",
                    max_args: 0,
                })
                .collect()
        }

        fn custom_commands(&self, _: &Path) -> Vec<CustomCommand> {
            vec![CustomCommand {
                name: "review".into(),
                description: "Review".into(),
                content: "Review $ARGUMENTS".into(),
                scope: CommandScope::Project,
                accepts_args: true,
                argument_hint: None,
            }]
        }

        fn lua_commands(&self) -> Vec<LuaCommand> {
            [("/greet", false), ("/picker", true)]
                .into_iter()
                .map(|(name, interactive)| LuaCommand {
                    plugin: "p".into(),
                    name: name.into(),
                    description: "l".into(),
                    max_args: 1,
                    interactive,
                })
                .collect()
        }

        fn run_lua(&self, _: &LuaCommand, _: String, _: String) {}

        fn run_pack(&self, _: &str, _: &str, _: bool) -> BoxFuture<'static, String> {
            Box::pin(async { String::new() })
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::testing::Host;
    use super::*;

    fn catalogue() -> Catalogue {
        Catalogue::gather(&Host, Path::new("."), None)
    }

    #[test_case("/compact keep the plan", Some(("compact", false, "keep the plan")) ; "args")]
    #[test_case("  /reload!  ", Some(("reload", true, "")) ; "bang")]
    #[test_case("/usr/bin/env is missing", None ; "a path stays a prompt")]
    #[test_case("/ compact", None ; "bare slash")]
    #[test_case("compact", None ; "no slash")]
    fn parses_a_command_line(text: &str, expected: Option<(&str, bool, &str)>) {
        let parsed = parse(text).map(|s| (s.name, s.bang, s.args));
        assert_eq!(parsed, expected);
    }

    #[test_case("compact", "builtin" ; "headless builtin")]
    #[test_case("COMPACT", "builtin" ; "case insensitive")]
    #[test_case("theme", "tui" ; "ui only builtin")]
    #[test_case("fork", "tui" ; "fork needs the tui")]
    #[test_case("project:review", "custom" ; "custom command")]
    #[test_case("greet", "lua" ; "lua command")]
    #[test_case("picker", "tui" ; "interactive lua command")]
    #[test_case("nope", "none" ; "unknown passes through")]
    fn resolves_every_source(name: &str, expected: &str) {
        let catalogue = catalogue();
        let got = match catalogue.resolve(name) {
            Some(Target::Builtin(_)) => "builtin",
            Some(Target::NeedsTui(_)) => "tui",
            Some(Target::Custom(_)) => "custom",
            Some(Target::Mcp(_)) => "mcp",
            Some(Target::Lua(_)) => "lua",
            None => "none",
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn lists_every_command_without_its_slash() {
        let listed: Vec<(String, String, bool)> = catalogue()
            .available()
            .into_iter()
            .map(|c| (c.name, c.description, c.input.is_some()))
            .collect();
        let tui = format!("d{NEEDS_TUI_SUFFIX}");
        let lua_tui = format!("l{NEEDS_TUI_SUFFIX}");
        assert_eq!(
            listed,
            [
                ("compact".into(), "d".into(), true),
                ("yolo".into(), "d".into(), false),
                ("workflow".into(), "d".into(), false),
                ("exit".into(), "d".into(), false),
                ("theme".into(), tui.clone(), false),
                ("fork".into(), tui, false),
                ("project:review".into(), "Review".into(), true),
                ("greet".into(), "l".into(), true),
                ("picker".into(), lua_tui, false),
            ]
        );
    }

    fn prompt(args: &[(&str, bool)]) -> McpPromptInfo {
        McpPromptInfo {
            display_name: "srv__p".into(),
            qualified_name: "srv__p".into(),
            description: String::new(),
            arguments: args
                .iter()
                .map(|(name, required)| McpPromptArg {
                    name: (*name).into(),
                    description: String::new(),
                    required: *required,
                })
                .collect(),
        }
    }

    #[test]
    fn the_last_mcp_argument_takes_the_rest_of_the_line() {
        let prompt = prompt(&[("file", true), ("note", false)]);
        let args = mcp_arguments(&prompt, "a.rs  please look closely");
        assert_eq!(args["file"], "a.rs");
        assert_eq!(args["note"], "please look closely");
        assert!(missing_mcp_arguments(&prompt, &args).is_empty());
    }

    #[test]
    fn a_missing_required_mcp_argument_is_named() {
        let prompt = prompt(&[("file", true)]);
        let args = mcp_arguments(&prompt, "");
        assert_eq!(missing_mcp_arguments(&prompt, &args), ["<file>"]);
    }
}
