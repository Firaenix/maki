//! What `maki acp` hands its sessions for slash commands: the TUI's command
//! table, markdown commands, and the Lua plugins this process loaded.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flume::Receiver;
use futures::future::BoxFuture;
use maki_acp::commands::{CommandHost, HostBuiltin, LuaCommand};
use maki_acp::host::HostRequest;
use maki_agent::SnapshotLine;
use maki_agent::command::{self, CustomCommand};
use maki_agent::skill::{self, Skill};
use maki_lua::{
    EventHandle, LuaCommandReader, NO_UI_ERR, PackCommand, PackPreparation, SessionRequest,
    UiAction, WinEvent,
};
use tokio::runtime::Handle;

const PACK_PANIC_ERR: &str = "package preparation panicked";
const PACK_REVIEW_MSG: &str = "This package change needs a review first. Review it in the TUI, \
or run the command again with `!` to skip the review.";
const PACK_RESTART_MSG: &str = "Restart the agent to load the change.";
const WINDOW_SETTLE: Duration = Duration::from_millis(150);
const READ_ONLY_NOTE: &str = "_A window in the maki TUI; shown here read-only._";

pub struct AcpCommands {
    pub no_commands: bool,
    pub lua: LuaCommandReader,
    pub events: EventHandle,
}

impl CommandHost for AcpCommands {
    fn builtins(&self) -> Vec<HostBuiltin> {
        maki_ui::BUILTIN_COMMANDS
            .iter()
            .map(|b| HostBuiltin {
                name: b.name,
                description: b.description,
                max_args: b.max_args,
            })
            .collect()
    }

    fn custom_commands(&self, cwd: &Path) -> Vec<CustomCommand> {
        match self.no_commands {
            true => Vec::new(),
            false => command::discover_commands(cwd),
        }
    }

    fn skills(&self, cwd: &Path) -> Vec<Skill> {
        skill::discover_skills(cwd)
    }

    fn lua_commands(&self) -> Vec<LuaCommand> {
        self.lua
            .load()
            .commands
            .iter()
            .map(|c| LuaCommand {
                plugin: Arc::clone(&c.plugin),
                name: Arc::clone(&c.name),
                description: Arc::clone(&c.description),
                max_args: c.max_args,
                interactive: c.interactive,
            })
            .collect()
    }

    fn run_lua(&self, command: &LuaCommand, args: String, session: String) {
        self.events.run_command(
            Arc::clone(&command.plugin),
            Arc::clone(&command.name),
            args,
            0,
            Some(session),
        );
    }

    fn run_pack(&self, name: &str, args: &str, bang: bool) -> BoxFuture<'static, String> {
        let command = PackCommand::parse(name, args, bang);
        let events = self.events.clone();
        Box::pin(async move {
            let command = match command {
                Ok(command) => command,
                Err(message) => return message,
            };
            // Preparation fetches through git and blocks on the Lua thread,
            // so it stays off the executor, like the TUI's.
            let runtime = Handle::current();
            let preparation = tokio::task::spawn_blocking(move || {
                catch_unwind(AssertUnwindSafe(|| match events.package_context() {
                    Ok(context) => {
                        runtime.block_on(maki_lua::prepare_pack_command(&command, &context))
                    }
                    Err(error) => PackPreparation::failed(error),
                }))
                .unwrap_or_else(|_| PackPreparation::failed(PACK_PANIC_ERR.to_owned()))
            })
            .await
            .unwrap_or_else(|_| PackPreparation::failed(PACK_PANIC_ERR.to_owned()));
            match preparation {
                PackPreparation::Complete(report) => report.message(),
                PackPreparation::Review { .. } => PACK_REVIEW_MSG.to_owned(),
                PackPreparation::Ready(plan) => {
                    let report = maki_lua::apply_pack_plan(plan).await;
                    format!("{} {PACK_RESTART_MSG}", report.message())
                }
            }
        })
    }
}

/// Drains the plugin host's UI channel for ACP, which has no TUI to answer it.
/// `maki.session.prompt` goes to the session; every other call that waits
/// for a reply hears there is no UI, and one that waits on nothing is dropped.
pub fn forward_ui(actions: Receiver<UiAction>) -> Receiver<HostRequest> {
    let (tx, rx) = flume::unbounded();
    tokio::spawn(async move {
        while let Ok(action) = actions.recv_async().await {
            match action {
                UiAction::Session {
                    req: SessionRequest::Prompt { id, text, images },
                    reply_tx,
                } => {
                    let request = HostRequest::Prompt {
                        session: id,
                        text,
                        images,
                        reply: reply_tx,
                    };
                    if tx.send(request).is_err() {
                        return;
                    }
                }
                UiAction::Flash(text) => {
                    let _ = tx.send(HostRequest::Say { text });
                }
                UiAction::OpenWin {
                    buf,
                    config,
                    focus,
                    event_tx,
                    cmd_rx,
                    ..
                } => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        // A plugin may fill its window right after opening it.
                        tokio::time::sleep(WINDOW_SETTLE).await;
                        let text = window_text(&config.title, &buf.read(), focus);
                        let _ = tx.send(HostRequest::Say { text });
                        let _ = event_tx.send(WinEvent::Close);
                        drop(cmd_rx);
                    });
                }
                UiAction::Session { reply_tx, .. }
                | UiAction::Model { reply_tx, .. }
                | UiAction::Plan { reply_tx, .. }
                | UiAction::Input { reply_tx, .. }
                | UiAction::Task { reply_tx, .. } => {
                    let _ = reply_tx.send(Err(NO_UI_ERR.to_owned()));
                }
                _ => {}
            }
        }
    });
    rx
}

/// A window as text. One that takes keys (a list or picker) keeps its columns
/// in a code block and says it cannot be used here; a toast is just its words.
fn window_text(title: &str, lines: &[SnapshotLine], takes_keys: bool) -> String {
    let body: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>()
        })
        .collect();
    let body = body.join("\n");
    let title = title.trim();
    match (takes_keys, title.is_empty()) {
        (true, true) => format!("```\n{body}\n```\n{READ_ONLY_NOTE}"),
        (true, false) => format!("**{title}**\n```\n{body}\n```\n{READ_ONLY_NOTE}"),
        (false, true) => body,
        (false, false) => format!("**{title}**: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(texts: &[&str]) -> Vec<SnapshotLine> {
        texts
            .iter()
            .map(|t| SnapshotLine::plain((*t).to_owned()))
            .collect()
    }

    #[test]
    fn a_focused_window_keeps_its_columns_and_says_it_is_read_only() {
        let text = window_text(" Skills (2) ", &lines(&["a   one", "b   two"]), true);
        assert_eq!(
            text,
            format!("**Skills (2)**\n```\na   one\nb   two\n```\n{READ_ONLY_NOTE}")
        );
    }

    #[test]
    fn a_toast_is_just_its_words() {
        assert_eq!(
            window_text("advisor", &lines(&["advisor off"]), false),
            "**advisor**: advisor off"
        );
    }
}
