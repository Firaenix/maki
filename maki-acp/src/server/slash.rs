//! Runs the slash commands [`crate::commands`] resolves against the server's
//! one session. A command either answers the prompt itself, right away or from
//! a task, or hands the session an input whose turn answers it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol_schema::v1::{
    AgentResponse, AvailableCommandsUpdate, Error as AcpError, PromptResponse, RequestId, Response,
    SessionId, SessionUpdate, StopReason,
};
use flume::Sender;
use futures::future::BoxFuture;
use maki_agent::agent::live_snapshot;
use maki_agent::btw::{BtwEvent, BtwRequest};
use maki_agent::{
    AgentMode, ImageSource, McpHandle, McpPromptRef, RunTotals, SessionEndReason, SessionInput,
};
use maki_config::project::{self, TrustState};
use maki_providers::RequestOptions;
use maki_providers::model::Model;
use maki_providers::provider;
use maki_storage::id::SessionRef;
use maki_storage::paths;
use serde_json::Value;

use super::{
    Incoming, Server, close_session, no_session, prompt_input, reload_history, restored_cost, send,
    session_update, start_mcp, start_session, submit, trusted_project_config,
};
use crate::commands::{
    self, Builtin, Catalogue, CommandHost, Slash, Target, mcp_arguments, missing_mcp_arguments,
};
use crate::host::{
    HostReply, HostRequest, NO_SESSION_ERR, OTHER_SESSION_ERR, PROMPT_QUEUED, PROMPT_STARTED,
};
use crate::{AcpParams, translate};

const AGENT_BUSY_MSG: &str = "The agent is busy; try again once this turn ends.";
const BTW_USAGE_MSG: &str = "Usage: /btw <question>";
const YOLO_ON_MSG: &str = "YOLO mode enabled";
const YOLO_OFF_MSG: &str = "YOLO mode disabled";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode needs Anthropic Opus 4.6+ with an API key, or an \
eligible Codex model with a ChatGPT subscription";
const WORKFLOW_ON_MSG: &str = "Workflow mode: on";
const WORKFLOW_OFF_MSG: &str = "Workflow mode: off";
const RELOADED_MSG: &str = "Reloaded the project config and MCP servers. Plugins and init.lua \
reload only in the TUI, or when the agent restarts.";
const ALREADY_TRUSTED_MSG: &str = "This folder is already trusted.";
const NOTHING_TO_TRUST_MSG: &str = "Nothing to trust in this folder.";
const TRUSTED_VACANT_MSG: &str = "Trusted this folder; project answers are saved from now on.";
const TRUSTED_PREFIX: &str = "Trusted this folder: ";
const EXITED_MSG: &str = "Session closed. Start or load a session to go on.";
/// How long a Lua command's prompt waits for its handler to start a turn
/// with `maki.session.prompt`. A handler that prompts does so straight away.
const COMMAND_GRACE: Duration = Duration::from_secs(2);

/// `None` when {message} is no command this session knows, so it goes to the
/// model as typed.
pub(super) async fn run(
    srv: &mut Server,
    params: &AcpParams,
    id: &RequestId,
    message: &str,
    images: &mut Vec<ImageSource>,
) -> Option<Result<(), AcpError>> {
    let slash = commands::parse(message)?;
    let session = srv.session.as_ref()?;
    let catalogue = Catalogue::gather(&*params.commands, &session.cwd, session.mcp.as_ref());
    let target = catalogue.resolve(slash.name)?;
    let sid = SessionId::from(session.handle.session_id.to_string());
    Some(match target {
        Target::NeedsTui(name) => reply(srv, id, &sid, &needs_tui(&name)),
        Target::Custom(command) => {
            let input = prompt_input(session, command.render(slash.args), std::mem::take(images));
            submit(srv, id, input.into())
        }
        Target::Mcp(prompt) => {
            let arguments = mcp_arguments(prompt, slash.args);
            let missing = missing_mcp_arguments(prompt, &arguments);
            if !missing.is_empty() {
                let usage = format!("Usage: /{} {}", prompt.display_name, missing.join(" "));
                return Some(reply(srv, id, &sid, &usage));
            }
            let mut input =
                prompt_input(session, message.trim().to_owned(), std::mem::take(images));
            input.prompt = Some(Box::new(McpPromptRef {
                qualified_name: prompt.qualified_name.clone(),
                arguments,
            }));
            submit(srv, id, input.into())
        }
        Target::Skill(skill) => {
            let text = commands::skill_prompt(skill, slash.args);
            let input = prompt_input(session, text, std::mem::take(images));
            submit(srv, id, input.into())
        }
        Target::Lua(command) => {
            let session_id = session.handle.session_id.to_string();
            params
                .commands
                .run_lua(command, slash.args.to_owned(), session_id);
            wait_for_command(srv, id);
            Ok(())
        }
        Target::Builtin(builtin) => run_builtin(srv, params, id, &sid, builtin, &slash).await,
    })
}

fn needs_tui(name: &str) -> String {
    format!("/{name} needs the maki TUI; it has no equivalent over ACP.")
}

async fn run_builtin(
    srv: &mut Server,
    params: &AcpParams,
    id: &RequestId,
    sid: &SessionId,
    builtin: Builtin,
    slash: &Slash<'_>,
) -> Result<(), AcpError> {
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    if builtin.restarts_session() && session.pending.lock().unwrap().prompt.is_some() {
        return reply(srv, id, sid, AGENT_BUSY_MSG);
    }
    let args = slash.args;
    match builtin {
        Builtin::Compact => {
            let settings = prompt_input(session, String::new(), Vec::new());
            let instructions = (!args.is_empty()).then(|| args.to_owned());
            submit(
                srv,
                id,
                SessionInput::Compact {
                    settings,
                    instructions,
                },
            )
        }
        Builtin::Btw if args.is_empty() => reply(srv, id, sid, BTW_USAGE_MSG),
        Builtin::Btw => {
            let spec = session.current_model.clone();
            let session_ref = session.handle.session_id.clone();
            let opts = RequestOptions {
                thinking: session.toggles.thinking.into(),
                fast: session.toggles.fast,
            };
            let answer = btw(srv, params, session_ref, spec, opts, args.to_owned());
            answer_later(srv, id, sid, answer);
            Ok(())
        }
        Builtin::Cd => {
            let target = cd_target(args, &session.cwd);
            match std::fs::canonicalize(&target) {
                Ok(dir) if dir.is_dir() => {
                    restart(srv, params, dir.clone()).await?;
                    reply(srv, id, sid, &format!("cd {}", dir.display()))
                }
                Ok(dir) => reply(
                    srv,
                    id,
                    sid,
                    &format!("cd: {} is not a directory", dir.display()),
                ),
                Err(e) => reply(srv, id, sid, &format!("cd {}: {e}", target.display())),
            }
        }
        Builtin::Yolo => {
            let on = session.handle.permissions.toggle_yolo();
            reply(srv, id, sid, if on { YOLO_ON_MSG } else { YOLO_OFF_MSG })
        }
        Builtin::Fast => {
            let enable = !session.toggles.fast;
            if enable {
                let spec = session.current_model.clone();
                let model = Model::from_spec_with_policy(&spec, &srv.model_policy).await;
                if !model.is_ok_and(|m| m.supports_fast() || m.fast_pending()) {
                    return reply(srv, id, sid, FAST_UNSUPPORTED_MSG);
                }
            }
            let session = srv.session.as_mut().ok_or_else(no_session)?;
            session.toggles.fast = enable;
            reply(
                srv,
                id,
                sid,
                if enable { FAST_ON_MSG } else { FAST_OFF_MSG },
            )
        }
        Builtin::Workflow => {
            session.toggles.workflow = !session.toggles.workflow;
            let on = session.toggles.workflow;
            reply(
                srv,
                id,
                sid,
                if on {
                    WORKFLOW_ON_MSG
                } else {
                    WORKFLOW_OFF_MSG
                },
            )
        }
        Builtin::Usage => {
            let usage = usage_text(&session.handle.spend.totals());
            reply(srv, id, sid, &usage)
        }
        Builtin::Reload => {
            let cwd = session.cwd.clone();
            restart(srv, params, cwd).await?;
            reply(srv, id, sid, RELOADED_MSG)
        }
        Builtin::Trust => trust(srv, params, id, sid).await,
        Builtin::PackUpdate | Builtin::PackDel => {
            let name = match builtin {
                Builtin::PackUpdate => "/packupdate",
                _ => "/packdel",
            };
            let report = params.commands.run_pack(name, args, slash.bang);
            answer_later(srv, id, sid, report);
            Ok(())
        }
        Builtin::Exit => {
            reply(srv, id, sid, EXITED_MSG)?;
            close_session(srv, SessionEndReason::Delete).await;
            Ok(())
        }
    }
}

/// Holds {id} open for the turn the command's handler may start. Whatever
/// comes first answers it: that turn, the grace running out, or the client's
/// next prompt.
fn wait_for_command(srv: &mut Server, id: &RequestId) {
    settle_command(srv);
    srv.command_wait = Some(id.clone());
    let tx = srv.in_tx.clone();
    let id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(COMMAND_GRACE).await;
        if let Some(tx) = tx.upgrade() {
            let _ = tx.send(Incoming::CommandGrace(id));
        }
    });
}

/// Ends a Lua command's prompt that is still waiting, with nothing said.
pub(super) fn settle_command(srv: &mut Server) {
    let Some(id) = srv.command_wait.take() else {
        return;
    };
    let Some(session) = srv.session.as_ref() else {
        return;
    };
    let sid = SessionId::from(session.handle.session_id.to_string());
    finish(&srv.out_tx, id, &sid, "");
}

pub(super) fn command_grace(srv: &mut Server, id: &RequestId) {
    if srv.command_wait.as_ref() == Some(id) {
        settle_command(srv);
    }
}

pub(super) fn host_request(srv: &mut Server, request: HostRequest) {
    match request {
        HostRequest::Prompt {
            session,
            text,
            images,
            reply,
        } => {
            let _ = reply.send(host_prompt(srv, session, text, images));
        }
        HostRequest::Say { text } => say(srv, &text),
    }
}

/// Only while a Lua command waits on its handler: plugins also toast and open
/// status windows on their own, and as notices those would flood the client.
fn say(srv: &Server, text: &str) {
    let (Some(_), Some(session)) = (&srv.command_wait, srv.session.as_ref()) else {
        return;
    };
    let sid = SessionId::from(session.handle.session_id.to_string());
    session_update(
        &srv.out_tx,
        &sid,
        translate::text_delta(&format!("{text}\n\n")),
    );
}

/// `maki.session.prompt` from a plugin. A Lua command waiting on its handler
/// hands its prompt id to this turn; otherwise the turn runs on its own, or
/// queues behind the one running.
fn host_prompt(
    srv: &mut Server,
    session: Option<String>,
    text: String,
    images: Vec<ImageSource>,
) -> HostReply {
    let state = srv.session.as_ref().ok_or(NO_SESSION_ERR)?;
    if session.is_some_and(|id| id != state.handle.session_id.to_string()) {
        return Err(OTHER_SESSION_ERR.to_owned());
    }
    let input = prompt_input(state, text, images);
    if let Some(id) = srv.command_wait.take() {
        return match submit(srv, &id, input.into()) {
            Ok(()) => Ok(Value::from(PROMPT_STARTED)),
            Err(e) => {
                let sid = SessionId::from(state.handle.session_id.to_string());
                finish(&srv.out_tx, id, &sid, "");
                Err(e.message)
            }
        };
    }
    let queued = state.pending.lock().unwrap().prompt.is_some();
    state
        .handle
        .input_tx
        .send(input.into())
        .map_err(|_| NO_SESSION_ERR.to_owned())?;
    Ok(Value::from(if queued {
        PROMPT_QUEUED
    } else {
        PROMPT_STARTED
    }))
}

/// Answers {id} with {text} as the agent's message, then ends the turn. Empty
/// text ends it with nothing said.
fn reply(srv: &Server, id: &RequestId, sid: &SessionId, text: &str) -> Result<(), AcpError> {
    finish(&srv.out_tx, id.clone(), sid, text);
    Ok(())
}

fn finish(out_tx: &Sender<Value>, id: RequestId, sid: &SessionId, text: &str) {
    if !text.is_empty() {
        session_update(out_tx, sid, translate::text_delta(text));
    }
    let resp = PromptResponse::new(StopReason::EndTurn);
    send(
        out_tx,
        Response::<AgentResponse>::new(id, Ok(AgentResponse::PromptResponse(resp))),
    );
}

/// Work that can take a while, a network fetch or a model reply, so the
/// server keeps reading the client meanwhile. {work} streams what it has to
/// say as it goes, and the line it returns closes the turn.
fn answer_later(srv: &Server, id: &RequestId, sid: &SessionId, work: BoxFuture<'static, String>) {
    let out_tx = srv.out_tx.clone();
    let id = id.clone();
    let sid = sid.clone();
    tokio::spawn(async move {
        let text = work.await;
        finish(&out_tx, id, &sid, &text);
    });
}

/// Asks {question} over the session's live transcript. The answer streams as
/// the agent's message and never lands in the history.
fn btw(
    srv: &Server,
    params: &AcpParams,
    session: SessionRef,
    spec: String,
    opts: RequestOptions,
    question: String,
) -> BoxFuture<'static, String> {
    let out_tx = srv.out_tx.clone();
    let policy = Arc::clone(&srv.model_policy);
    let timeouts = params.timeouts;
    let sid = SessionId::from(session.to_string());
    Box::pin(async move {
        let mut model = match Model::from_spec_with_policy(&spec, &policy).await {
            Ok(model) => model,
            Err(e) => return format!("/btw: {e}"),
        };
        let provider = match provider::from_model_async(&mut model, timeouts).await {
            Ok(provider) => provider,
            Err(e) => return format!("/btw: {e}"),
        };
        let snapshot = live_snapshot(session.id());
        let request = BtwRequest::new(
            snapshot.as_deref(),
            &question,
            Arc::from(provider),
            model,
            opts,
            session,
        );
        let (tx, rx) = flume::bounded(64);
        tokio::spawn(request.run(tx));
        while let Ok(event) = rx.recv_async().await {
            match event {
                BtwEvent::TextDelta(text) => {
                    session_update(&out_tx, &sid, translate::text_delta(&text));
                }
                BtwEvent::ThinkingDelta(text) => {
                    session_update(&out_tx, &sid, translate::thinking_delta(&text));
                }
                BtwEvent::Done => break,
                BtwEvent::Error(e) => return format!("\n\n/btw failed: {e}"),
            }
        }
        String::new()
    })
}

/// Like the TUI's `/cd`: empty is home, `~` is home, and a relative path is
/// taken from where the session runs, never the server's own cwd.
fn cd_target(args: &str, cwd: &Path) -> PathBuf {
    let home = || paths::home().unwrap_or_default();
    match args {
        "" | "~" => home(),
        _ => match args.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => cwd.join(args),
        },
    }
}

fn usage_text(totals: &RunTotals) -> String {
    let usage = &totals.usage;
    let cost = totals
        .cost
        .map(|cost| format!(", ${cost:.4}"))
        .unwrap_or_default();
    format!(
        "Tokens this session has spent in this agent: {} input, {} output, {} cache read, {} \
         cache write{cost}",
        usage.input, usage.output, usage.cache_read, usage.cache_creation
    )
}

async fn trust(
    srv: &mut Server,
    params: &AcpParams,
    id: &RequestId,
    sid: &SessionId,
) -> Result<(), AcpError> {
    let cwd = srv.session.as_ref().ok_or_else(no_session)?.cwd.clone();
    let decision = project::resolve(&params.storage, &cwd, params.trust_mode);
    let Some(question) = decision.state.grantable().cloned() else {
        let reason = match decision.state {
            TrustState::Trusted => ALREADY_TRUSTED_MSG,
            _ => NOTHING_TO_TRUST_MSG,
        };
        return reply(srv, id, sid, reason);
    };
    if let Err(error) = project::grant(&params.storage, &question) {
        return reply(srv, id, sid, &error);
    }
    restart(srv, params, cwd).await?;
    let said = match question.present.is_empty() {
        true => TRUSTED_VACANT_MSG.to_owned(),
        false => {
            let covered: Vec<String> = question.present.iter().map(ToString::to_string).collect();
            format!("{TRUSTED_PREFIX}{}", covered.join(", "))
        }
    };
    reply(srv, id, sid, &said)
}

/// Starts the session again at {cwd} from what it saved, under freshly read
/// project config and MCP servers. Same id and history; the model, mode and
/// toggles carry over. The client already shows the transcript, so nothing
/// is replayed.
async fn restart(srv: &mut Server, params: &AcpParams, cwd: PathBuf) -> Result<(), AcpError> {
    let state = srv.session.as_ref().ok_or_else(no_session)?;
    let claim = state.claim.clone();
    let session_ref = state.handle.session_id.clone();
    let servers = state.client_servers.clone();
    let mode = state.current_mode.clone();
    let spec = state.current_model.clone();
    let toggles = state.toggles;

    close_session(srv, SessionEndReason::Reload).await;
    let restored = reload_history(&params.storage, &claim)?;
    let project_config = trusted_project_config(
        &cwd,
        &params.storage,
        params.trust_mode,
        &params.trust_policy,
    );
    let mcp = start_mcp(&cwd, &servers, project_config.clone()).await;
    let cost = restored_cost(&restored.session, &params.model).await;
    start_session(
        srv,
        params,
        cwd,
        maki_agent::session::Resumed::stored(session_ref, restored.session),
        claim,
        mcp,
        servers,
        project_config,
        cost,
    );

    let model = match spec == params.model.spec() {
        true => None,
        false => Model::from_spec_with_policy(&spec, &srv.model_policy)
            .await
            .ok(),
    };
    let state = srv.session.as_mut().ok_or_else(no_session)?;
    state.toggles = toggles;
    if matches!(mode, AgentMode::Plan(_)) {
        state.handle.switch_mode(mode.clone());
        state.current_mode = mode;
    }
    if let Some(model) = model {
        let _ = state.handle.model_tx.send(model);
        state.current_model = spec;
    }
    Ok(())
}

/// Tells the client what {session} can run. MCP prompts arrive once their
/// servers connect, so the list goes out again then.
pub(super) fn announce_commands(
    out_tx: &Sender<Value>,
    session: &SessionRef,
    host: Arc<dyn CommandHost>,
    cwd: &Path,
    mcp: Option<&McpHandle>,
) {
    let sid = SessionId::from(session.to_string());
    send_commands(out_tx, &sid, &*host, cwd, mcp);
    let Some(mcp) = mcp.cloned() else { return };
    // Weak, so a server that never connects cannot hold stdout open.
    let out_tx = out_tx.downgrade();
    let cwd = cwd.to_owned();
    tokio::spawn(async move {
        mcp.ready().await;
        if mcp.reader().load().prompts.is_empty() {
            return;
        }
        if let Some(out_tx) = out_tx.upgrade() {
            send_commands(&out_tx, &sid, &*host, &cwd, Some(&mcp));
        }
    });
}

fn send_commands(
    out_tx: &Sender<Value>,
    sid: &SessionId,
    host: &dyn CommandHost,
    cwd: &Path,
    mcp: Option<&McpHandle>,
) {
    let update = AvailableCommandsUpdate::new(Catalogue::gather(host, cwd, mcp).available());
    session_update(out_tx, sid, SessionUpdate::AvailableCommandsUpdate(update));
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::RequestId;
    use flume::Receiver;
    use maki_storage::StateDir;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::super::tests::{acp_params, test_server};
    use super::*;
    use crate::host::HostReply;

    const PROMPT_ID: i64 = 7;

    struct Fixture {
        srv: Server,
        params: AcpParams,
        wire: Receiver<Value>,
        inputs: Receiver<SessionInput>,
        _dir: TempDir,
    }

    fn fixture() -> Fixture {
        let (mut srv, _answers, wire) = test_server();
        let (input_tx, inputs) = flume::unbounded();
        srv.session.as_mut().unwrap().handle.input_tx = input_tx;
        let dir = TempDir::new().unwrap();
        let params = acp_params(StateDir::from_path(dir.path().to_path_buf()));
        Fixture {
            srv,
            params,
            wire,
            inputs,
            _dir: dir,
        }
    }

    impl Fixture {
        async fn send(&mut self, message: &str) -> Option<Result<(), AcpError>> {
            let id = RequestId::Number(PROMPT_ID);
            run(&mut self.srv, &self.params, &id, message, &mut Vec::new()).await
        }

        /// The text said and whether the prompt was answered, in wire order.
        fn said(&self) -> (String, bool) {
            let mut text = String::new();
            let mut answered = false;
            for msg in self.wire.try_iter() {
                if let Some(chunk) = msg.pointer("/params/update/content/text") {
                    text.push_str(chunk.as_str().unwrap());
                }
                if msg.pointer("/result/stopReason").is_some() {
                    assert_eq!(msg["id"], PROMPT_ID);
                    answered = true;
                }
            }
            (text, answered)
        }

        fn pending(&self) -> bool {
            let session = self.srv.session.as_ref().unwrap();
            session.pending.lock().unwrap().prompt.is_some()
        }
    }

    #[tokio::test]
    async fn a_line_naming_no_command_reaches_the_model() {
        let mut f = fixture();
        assert!(f.send("/usr/bin/env is missing").await.is_none());
        assert!(f.send("/nope").await.is_none());
        assert!(f.wire.is_empty());
    }

    #[test_case("/theme", "/theme needs the maki TUI" ; "ui only builtin")]
    #[test_case("/fork", "/fork needs the maki TUI" ; "fork")]
    #[test_case("/picker", "/picker needs the maki TUI" ; "interactive lua command")]
    #[tokio::test]
    async fn a_tui_only_command_says_so_and_ends_the_turn(line: &str, expected: &str) {
        let mut f = fixture();
        f.send(line).await.unwrap().unwrap();
        let (text, answered) = f.said();
        assert!(text.starts_with(expected), "{text}");
        assert!(answered);
        assert!(f.inputs.is_empty(), "nothing reaches the model");
    }

    #[test_case("/compact keep the plan", Some("keep the plan") ; "with guidance")]
    #[test_case("/compact", None ; "bare")]
    #[tokio::test]
    async fn compact_rides_the_input_channel(line: &str, expected: Option<&str>) {
        let mut f = fixture();
        f.send(line).await.unwrap().unwrap();
        let Ok(SessionInput::Compact { instructions, .. }) = f.inputs.try_recv() else {
            panic!("a compaction is queued");
        };
        assert_eq!(instructions.as_deref(), expected);
        assert!(f.pending(), "the compaction's Done answers the prompt");
    }

    #[tokio::test]
    async fn a_markdown_command_is_sent_rendered() {
        let mut f = fixture();
        f.send("/project:review a.rs").await.unwrap().unwrap();
        let Ok(SessionInput::Prompt(input)) = f.inputs.try_recv() else {
            panic!("a prompt is queued");
        };
        assert_eq!(input.message, "Review a.rs");
        assert!(f.pending());
    }

    /// The dotfiles plugin also registers `/release` as a Lua command, whose
    /// prompt the skill must win over.
    #[tokio::test]
    async fn a_skill_is_sent_as_a_prompt_naming_its_file() {
        let mut f = fixture();
        f.send("/release the hotfix").await.unwrap().unwrap();
        let Ok(SessionInput::Prompt(input)) = f.inputs.try_recv() else {
            panic!("a prompt is queued");
        };
        assert_eq!(
            input.message,
            "Use the release skill: read /skills/release/SKILL.md and follow it. Focus: the hotfix"
        );
        assert!(f.pending());
        assert!(f.srv.command_wait.is_none(), "no Lua handler is waited on");
    }

    #[tokio::test]
    async fn workflow_flips_what_later_prompts_run_under() {
        let mut f = fixture();
        f.send("/workflow").await.unwrap().unwrap();
        assert_eq!(f.said(), (WORKFLOW_ON_MSG.to_owned(), true));
        let session = f.srv.session.as_ref().unwrap();
        assert!(prompt_input(session, String::new(), Vec::new()).workflow);
        assert!(
            !f.params.defaults.workflow,
            "the server's defaults stay put"
        );
    }

    #[tokio::test]
    async fn yolo_toggles_the_sessions_permissions() {
        let mut f = fixture();
        f.send("/yolo").await.unwrap().unwrap();
        assert_eq!(f.said(), (YOLO_ON_MSG.to_owned(), true));
        f.send("/yolo").await.unwrap().unwrap();
        assert_eq!(f.said(), (YOLO_OFF_MSG.to_owned(), true));
    }

    #[tokio::test]
    async fn a_restarting_command_waits_for_the_running_turn() {
        let mut f = fixture();
        let session = f.srv.session.as_ref().unwrap();
        session.pending.lock().unwrap().prompt = Some(RequestId::Number(1));
        f.send("/exit").await.unwrap().unwrap();
        assert_eq!(f.said(), (AGENT_BUSY_MSG.to_owned(), true));
        assert!(f.srv.session.is_some(), "the running session is left alone");
    }

    #[tokio::test]
    async fn exit_closes_the_session() {
        let mut f = fixture();
        f.send("/exit").await.unwrap().unwrap();
        assert_eq!(f.said(), (EXITED_MSG.to_owned(), true));
        assert!(f.srv.session.is_none());
    }

    fn host_prompt_request(session: Option<String>) -> (HostRequest, flume::Receiver<HostReply>) {
        let (reply, answer) = flume::bounded(1);
        let request = HostRequest::Prompt {
            session,
            text: "Use the release skill".into(),
            images: Vec::new(),
            reply,
        };
        (request, answer)
    }

    /// A skill command's handler prompts the session from Lua. That turn has
    /// to answer the client's prompt, or the reply lands after Paseo thinks
    /// the turn ended.
    #[tokio::test]
    async fn a_lua_commands_prompt_answers_the_clients_turn() {
        let mut f = fixture();
        f.send("/greet world").await.unwrap().unwrap();
        assert_eq!(
            f.said(),
            (String::new(), false),
            "held open for the handler"
        );

        let (request, answer) = host_prompt_request(None);
        host_request(&mut f.srv, request);
        assert_eq!(answer.try_recv().unwrap(), Ok(Value::from(PROMPT_STARTED)));
        let Ok(SessionInput::Prompt(input)) = f.inputs.try_recv() else {
            panic!("the handler's prompt is queued");
        };
        assert_eq!(input.message, "Use the release skill");
        assert!(f.pending(), "the turn now owns the client's prompt");
        assert!(f.srv.command_wait.is_none());
    }

    /// `/skills` opens a window listing them; ACP shows that as the turn's text.
    #[tokio::test]
    async fn what_a_lua_command_shows_lands_in_its_turn() {
        let mut f = fixture();
        f.send("/greet").await.unwrap().unwrap();
        host_request(
            &mut f.srv,
            HostRequest::Say {
                text: "Skills (2)".into(),
            },
        );
        command_grace(&mut f.srv, &RequestId::Number(PROMPT_ID));
        assert_eq!(f.said(), ("Skills (2)\n\n".to_owned(), true));
    }

    #[tokio::test]
    async fn what_a_plugin_shows_outside_a_command_is_dropped() {
        let mut f = fixture();
        host_request(
            &mut f.srv,
            HostRequest::Say {
                text: "verdict: allow".into(),
            },
        );
        assert!(f.wire.is_empty());
    }

    #[tokio::test]
    async fn a_lua_command_that_never_prompts_ends_at_the_grace() {
        let mut f = fixture();
        f.send("/greet").await.unwrap().unwrap();
        command_grace(&mut f.srv, &RequestId::Number(PROMPT_ID + 1));
        assert_eq!(
            f.said(),
            (String::new(), false),
            "a stale grace changes nothing"
        );
        command_grace(&mut f.srv, &RequestId::Number(PROMPT_ID));
        assert_eq!(f.said(), (String::new(), true));
    }

    #[tokio::test]
    async fn a_plugin_prompt_with_no_command_waiting_runs_on_its_own() {
        let mut f = fixture();
        let (request, answer) = host_prompt_request(None);
        host_request(&mut f.srv, request);
        assert_eq!(answer.try_recv().unwrap(), Ok(Value::from(PROMPT_STARTED)));
        assert!(f.inputs.try_recv().is_ok());
        assert!(!f.pending(), "no client prompt to answer");
    }

    #[tokio::test]
    async fn a_plugin_prompt_for_another_session_is_refused() {
        let mut f = fixture();
        let (request, answer) = host_prompt_request(Some("someone-else".into()));
        host_request(&mut f.srv, request);
        assert_eq!(
            answer.try_recv().unwrap(),
            Err(OTHER_SESSION_ERR.to_owned())
        );
        assert!(f.inputs.is_empty());
    }

    #[test_case("sub", "/project/sub" ; "relative to the session")]
    #[test_case("/abs", "/abs" ; "absolute")]
    fn cd_resolves_against_the_session_cwd(args: &str, expected: &str) {
        assert_eq!(
            cd_target(args, Path::new("/project")),
            PathBuf::from(expected)
        );
    }

    #[test]
    fn cd_home_shorthands() {
        let home = paths::home().unwrap_or_default();
        assert_eq!(cd_target("", Path::new("/project")), home);
        assert_eq!(cd_target("~", Path::new("/project")), home);
        assert_eq!(cd_target("~/x", Path::new("/project")), home.join("x"));
    }
}
