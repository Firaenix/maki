use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::Arc;

use futures_lite::future;

use arc_swap::ArcSwap;
use maki_agent::agent::{self, AgentHooks};
use maki_agent::mcp::config::McpServerStatus;
use maki_agent::mcp::{McpHandle, McpSession};
use maki_agent::permissions::PermissionManager;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::session::Resumed;
use maki_agent::template;
use maki_agent::template::Vars;
use maki_agent::tools::{RequestTools, ToolAudience, ToolRegistry};
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, AgentSeed, CancelMap,
    CancelToken, CancelTrigger, DoneReason, Envelope, EventSender, History, HistorySnapshot,
    Instructions, McpCommand, PromptRole, RunContext, RunContextBuilder, RunLedger, SessionMailbox,
    SharedMessages, SubagentInfo, ToolDoneEvent, ToolOutput, ToolOutputLines,
};
use maki_config::ModelPolicy;
use maki_lua::EventHandle;
use maki_providers::{AgentError, ContextGauge, Message, Model, Role, TokenUsage};
use maki_storage::id::MakiId;
use tracing::error;

use super::ModelSlot;
use super::detach::{
    self, DETACHED_RUN_ID, DETACHED_TASK_NAME, DETACHED_TASK_PREFIX, DetachedRuns, RunInterrupts,
};
use super::run_cancels::{LiveRun, RunCancels};
use super::shared_queue::{self, Compaction, QueueReceiver, QueueRun};

fn base_tools(
    vars: &Vars,
    model: &Model,
    config: &AgentConfig,
    has_mcp: bool,
    workflow: bool,
) -> RequestTools {
    RequestTools::build(
        ToolRegistry::global(),
        vars,
        model,
        config,
        &[],
        workflow,
        has_mcp,
    )
}

pub(super) struct AgentLoop {
    /// The registry only holds the seed weakly, so plugins can reach it for
    /// exactly as long as this loop lives.
    seed: Arc<AgentSeed>,
    vars: Vars,
    instructions: Instructions,
    history: History,
    /// What `history` publishes to. A detached run takes `history` along, and
    /// the loop picks the transcript up again from here.
    mirror: SharedMessages,
    /// Owned beside `history` because it describes that transcript and outlives
    /// every run over it, so the provider's own counts pile up between turns.
    gauge: ContextGauge,
    cancels: Arc<RunCancels>,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<smol::lock::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    mailbox: SessionMailbox,
    lua_handle: EventHandle,
    /// Swapped out when a run is detached, so the map that run registered its
    /// subagents in leaves with it.
    subagent_cancels: Arc<ArcSwap<CancelMap<String>>>,
    detached: Arc<DetachedRuns>,
}

/// How a run left the loop.
enum RunEnd {
    Finished(Box<RunOutput>),
    /// Still running, now on its own. {stop} is what still ends it, and
    /// {answer_tx} reaches the prompts it raises from here on.
    Detached {
        answer_tx: flume::Sender<String>,
        stop: CancelTrigger,
    },
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        resumed: Resumed,
        shared_history: SharedMessages,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancels: Arc<RunCancels>,
        mailbox: SessionMailbox,
        timeouts: maki_providers::Timeouts,
        lua_handle: EventHandle,
        subagent_cancels: Arc<ArcSwap<CancelMap<String>>>,
        detached: Arc<DetachedRuns>,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        let mcp = mcp_handle.map(|h| McpSession::new(h, &resumed.history));
        let seed = mailbox.publish_seed(AgentSeed::new(
            resumed.id,
            model_slot,
            config,
            tool_output_lines,
            permissions,
            mcp,
            timeouts,
            model_policy,
        ));
        Self {
            seed,
            vars: Vars::default(),
            instructions: Instructions::default(),
            history: History::restored(resumed.history)
                .with_frame(resumed.frame)
                .with_mirror(Arc::clone(&shared_history)),
            mirror: shared_history,
            gauge: ContextGauge::restored(resumed.context_size),
            cancels,
            agent_tx,
            answer_rx: Arc::new(smol::lock::Mutex::new(answer_rx)),
            queue,
            mailbox,
            lua_handle,
            subagent_cancels,
            detached,
        }
    }

    pub(super) async fn run(mut self) {
        if !self.initialize().await {
            return;
        }

        while let Ok(()) = self.queue.recv_notify().await {
            let mut last_run_id = None;
            while let Some(mut run) = self.queue.pop_run() {
                let Some(run_id) = run.drop_cancelled(self.cancels.min_run_id()) else {
                    continue;
                };
                last_run_id = Some(run_id);
                self.process_run(run, run_id).await;
            }
            if let Some(run_id) = last_run_id {
                let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
                self.queue
                    .publish_if_empty(|| event_tx.try_send(AgentEvent::QueueDrained));
            }
        }
    }

    async fn process_run(&mut self, run: QueueRun, run_id: u64) {
        let live = self.cancels.start(run_id);
        let result = self.dispatch_run(run, run_id, &live).await;
        // A `tool_use_id` only names work inside the run that issued the call,
        // so the run ending is what stops whatever still hangs off one, rather
        // than a group emptying out.
        self.subagent_cancels.load().cancel_all();

        // A cancel arrives here as `Ok`, since esc is what the user asked for.
        // As an error it would draw a second "Cancelled." bubble under the one
        // the app already drew, leave the status bar red and fail the exit
        // under `--exit-on-done`.
        if let Err(e) = result {
            self.emit_error(run_id, e);
        }
    }

    async fn dispatch_run(
        &mut self,
        run: QueueRun,
        run_id: u64,
        live: &LiveRun,
    ) -> Result<(), AgentError> {
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        match run {
            QueueRun::Compact(compaction) => {
                self.do_compact(&event_tx, &compaction, live.token())
                    .await?;
            }
            QueueRun::Messages(messages) => {
                let inputs = messages
                    .into_iter()
                    .map(|queued| {
                        if !queued.displayed {
                            let _ = event_tx.send(AgentEvent::QueueItemConsumed {
                                text: queued.text,
                                images: queued.input.images.clone(),
                            });
                        }
                        queued.input
                    })
                    .collect();
                if let Some(input) = shared_queue::merge_inputs(inputs) {
                    self.do_agent_run(input, event_tx, live).await?;
                }
            }
        }
        Ok(())
    }

    /// Startup belongs to the loop, not to any run, so it watches teardown. Esc
    /// only means the user gave up on a prompt, and ending the loop over that
    /// would leave the session on screen with nothing to serve it.
    async fn initialize(&mut self) -> bool {
        let teardown = self.cancels.teardown().clone();
        self.vars = template::env_vars();
        self.reload_instructions().await;
        if teardown.is_cancelled() {
            return false;
        }

        if let Some(ref mcp) = self.seed.mcp {
            // The queue is drained right after this, and a prompt typed during
            // startup must still carry the MCP tools.
            if teardown.race(mcp.ready()).await.is_err() {
                return false;
            }
            spawn_oauth_for_needs_auth(mcp);
        }
        !teardown.is_cancelled()
    }

    /// What a run would start with now. Every run and `/compact` take it from
    /// here, so the frame a compaction leaves behind is the one the next run
    /// keeps, not a near copy that costs it the cache.
    async fn context_builder(&mut self) -> (RunContextBuilder, Arc<ResolvedPrompts>) {
        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
        }
        let prompt_slots = Arc::new(self.lua_handle.collect_prompt_slots_async().await);
        let vars = self.vars.clone();
        let instructions = self.instructions.text.clone();
        let slots = Arc::clone(&prompt_slots);
        let config = self.seed.config.clone();
        let has_mcp = self.seed.mcp.is_some();
        let context: RunContextBuilder = Arc::new(move |model, workflow| {
            let tools = base_tools(&vars, model, &config, has_mcp, workflow);
            RunContext::render(&vars, &instructions, &slots, model, tools)
        });
        (context, prompt_slots)
    }

    async fn do_compact(
        &mut self,
        event_tx: &EventSender,
        compaction: &Compaction,
        cancel: &CancelToken,
    ) -> Result<DoneReason, AgentError> {
        let (context, _) = self.context_builder().await;
        let slot = self.seed.model_slot.load();
        let (provider, model) = agent::resolve_compaction_model(
            &slot.provider,
            &slot.model,
            self.seed.timeouts,
            &self.seed.model_policy,
        )
        .await;
        // The summary goes out under a fresh frame for the session's own model,
        // so the gauge and `/btw` read the prompt the next run sends.
        let next = context(&slot.model, compaction.workflow);
        let hooks = AgentHooks {
            registry: ToolRegistry::global(),
            session_id: Some(&self.seed.session_id),
            task_id: None,
            model: &slot.model,
            cancel,
            context_size: self.gauge.size(),
        };
        agent::compact(
            &*provider,
            &model,
            &mut self.history,
            &mut self.gauge,
            next,
            self.seed.mcp.as_ref(),
            event_tx,
            &hooks,
            &self.seed.config,
            compaction.instructions.as_deref(),
            self.seed.timeouts.retry,
        )
        .await
    }

    async fn do_agent_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        live: &LiveRun,
    ) -> Result<DoneReason, AgentError> {
        let (context, prompt_slots) = self.context_builder().await;

        if let Some(ref prompt_ref) = input.prompt {
            let Some(ref mcp) = self.seed.mcp else {
                return Err(AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: "MCP not available".into(),
                });
            };
            let messages = mcp
                .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
                .await
                .map_err(|e| AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: e.to_string(),
                })?;
            for pm in messages {
                let text = pm.content.text.unwrap_or_default();
                let msg = match pm.role {
                    PromptRole::Assistant => Message {
                        role: maki_providers::Role::Assistant,
                        content: vec![maki_providers::ContentBlock::Text { text }],
                        ..Default::default()
                    },
                    PromptRole::User => Message::user(text),
                };
                input.preamble.push(msg);
            }
        }

        // Read after the awaits above, not before: a switch can land while the
        // run is still starting up, and the frame and the request both have to
        // name the model that is current now.
        let slot = self.seed.model_slot.load_full();

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let params = AgentParams {
            provider: Arc::clone(&slot.provider),
            model: slot.model.clone(),
            config: self.seed.config.clone(),
            tool_output_lines: self.seed.tool_output_lines,
            permissions: Arc::clone(&self.seed.permissions),
            session_id: Some(self.seed.session_id.clone()),
            background_for: None,
            task_id: None,
            mailbox: Some(self.mailbox.clone()),
            timeouts: self.seed.timeouts,
            file_access: Arc::clone(&self.seed.file_access),
            prompt_slots: Arc::clone(&prompt_slots),
            subagent_cancels: self.subagent_cancels.load_full(),
            ledger: Arc::new(RunLedger::default()),
            registry: Arc::clone(maki_agent::tools::ToolRegistry::global_arc()),
            audience: ToolAudience::MAIN,
            model_policy: Arc::clone(&self.seed.model_policy),
        };
        let prompt = input.message.clone();
        let (run_tx, run_rx) = flume::unbounded();
        let run_event_tx = EventSender::new(run_tx, event_tx.run_id());
        let interrupts = Arc::new(RunInterrupts::new(Arc::clone(&self.queue)));
        // The run answers to a token of its own, so it can outlive `live` once
        // it is detached. Until then esc reaches it through `attach`.
        let (stop, run_cancel) = CancelToken::new();
        let gauge_before = self.gauge.clone();
        let mut history = mem::replace(&mut self.history, History::new(Vec::new()));
        let tap = history.mirror_tap();
        let mut gauge = mem::take(&mut self.gauge);
        let loaded = self.instructions.loaded.clone();
        let answer_rx = Arc::clone(&self.answer_rx);
        let source = Arc::clone(&interrupts) as Arc<dyn maki_agent::InterruptSource>;
        let model_sync = Arc::clone(&self.seed.model_slot);
        let mcp = self.seed.mcp.clone();
        let mut run = Box::pin(async move {
            let mut agent = Agent::new(
                params,
                AgentRunParams {
                    history: &mut history,
                    gauge: &mut gauge,
                    event_tx: run_event_tx,
                    context,
                },
            )
            .with_loaded_instructions(loaded)
            .with_user_response_rx(answer_rx)
            .with_reauth(true)
            .with_interrupt_source(source)
            .with_cancel(run_cancel)
            .with_model_sync(model_sync)
            .with_mcp(mcp);
            let result = agent.run(input).await;
            drop(agent);
            Box::new((history, gauge, result))
        });

        let (answer_tx, stop) = match self.attach(&mut run, &run_rx, live, stop, &event_tx).await {
            RunEnd::Finished(output) => {
                let (history, gauge, result) = *output;
                self.history = history;
                self.gauge = gauge;
                return result;
            }
            RunEnd::Detached { answer_tx, stop } => (answer_tx, stop),
        };

        let task_id = format!("{DETACHED_TASK_PREFIX}{}", MakiId::generate());
        // The run is not polled again until it is spawned, so nothing it
        // writes can slip in between the cut and the pickup.
        tap.cut();
        let inbox = interrupts.detach();
        let subagent_cancels = self.subagent_cancels.swap(Arc::new(CancelMap::new()));
        self.history = History::from_snapshot(HistorySnapshot::clone(&self.mirror.load()))
            .with_mirror(Arc::clone(&self.mirror));
        self.gauge = gauge_before;
        self.record_handoff(&event_tx, &task_id);
        self.detached
            .insert(task_id.clone(), stop, Arc::clone(&subagent_cancels));
        // Ctrl+Enter only offers itself with a message queued, but the run may
        // have taken that message in just before it let go. With nothing left
        // to answer, the chat's turn ends here.
        self.queue.publish_if_empty(|| {
            event_tx.try_send(AgentEvent::Done {
                usage: TokenUsage::default(),
                cost: None,
                list_cost: None,
                context_size: self.gauge.size(),
                context_window: slot.model.context_window,
                num_turns: 0,
                reason: DoneReason::EndTurn,
            })
        });

        let info = SubagentInfo {
            parent_tool_use_id: task_id,
            name: DETACHED_TASK_NAME.into(),
            prompt: Some(prompt),
            model: Some(slot.model.spec()),
            opts: None,
            answer_tx: Some(answer_tx),
            inbox: Some(inbox),
        };
        smol::spawn(finish_detached(
            run,
            run_rx,
            info,
            self.agent_tx.clone(),
            subagent_cancels,
            Arc::clone(&self.detached),
            self.seed.session_id.id(),
        ))
        .detach();
        Ok(DoneReason::EndTurn)
    }

    /// Drives the run while the chat owns it, passing its events on as the
    /// chat's own.
    async fn attach<F: Future<Output = Box<RunOutput>>>(
        &mut self,
        run: &mut Pin<Box<F>>,
        run_rx: &flume::Receiver<Envelope>,
        live: &LiveRun,
        stop: CancelTrigger,
        event_tx: &EventSender,
    ) -> RunEnd {
        let mut stop = Some(stop);
        let mut detachable = true;
        loop {
            let esc_armed = stop.is_some();
            let detach_armed = esc_armed && detachable;
            let agent_tx = &self.agent_tx;
            let step = future::or(
                async { Step::Done(run.as_mut().await) },
                future::or(
                    forward(run_rx, agent_tx),
                    future::or(
                        async {
                            if !esc_armed {
                                future::pending::<()>().await;
                            }
                            live.token().cancelled().await;
                            Step::Esc
                        },
                        async {
                            if !detach_armed {
                                future::pending::<()>().await;
                            }
                            live.detach_signal().cancelled().await;
                            Step::Detach
                        },
                    ),
                ),
            )
            .await;
            match step {
                Step::Done(output) => {
                    for envelope in run_rx.try_iter() {
                        let _ = self.agent_tx.send(envelope);
                    }
                    return RunEnd::Finished(output);
                }
                // Dropping the trigger is what stops the run.
                Step::Esc => drop(stop.take()),
                Step::Detach => {
                    detachable = false;
                    // Held means the run is waiting on the user, and the
                    // prompt it waits on is already up in the chat. Moving it
                    // now would send the answer to the wrong run.
                    let Some(mut answers) = self.answer_rx.try_lock() else {
                        let _ = event_tx.send(AgentEvent::Notice {
                            text: DETACH_REFUSED_MSG.into(),
                        });
                        continue;
                    };
                    for envelope in run_rx.try_iter() {
                        let _ = self.agent_tx.send(envelope);
                    }
                    let (answer_tx, own) = flume::unbounded();
                    let chat_answers = mem::replace(&mut *answers, own);
                    drop(answers);
                    self.answer_rx = Arc::new(smol::lock::Mutex::new(chat_answers));
                    let Some(stop) = stop.take() else {
                        unreachable!("detach is only armed while the run can be stopped")
                    };
                    return RunEnd::Detached { answer_tx, stop };
                }
            }
        }
    }

    /// The chat's transcript keeps the calls the run had open, so it has to
    /// close them, and the chat has to stop showing them as running.
    fn record_handoff(&mut self, event_tx: &EventSender, task_id: &str) {
        let result = detach::tool_handoff_result(task_id);
        let open: Vec<(String, Arc<str>)> = self
            .history
            .as_slice()
            .last()
            .filter(|m| matches!(m.role, Role::Assistant))
            .map(|m| {
                m.tool_uses()
                    .map(|(id, name, _)| (id.to_owned(), Arc::from(name)))
                    .collect()
            })
            .unwrap_or_default();
        self.history.close_dangling_tool_calls(&result);
        self.history
            .push(Message::synthetic(detach::handoff_note(task_id)));
        for (id, tool) in open {
            let _ = event_tx.send(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
                id,
                tool,
                output: Arc::new(ToolOutput::Plain(result.clone().into())),
                is_error: false,
                annotation: None,
                written_path: None,
                call: None,
            })));
        }
        let _ = event_tx.send(AgentEvent::Notice {
            text: format!("Moved the running turn to {task_id}, it reports back when done"),
        });
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        self.instructions = smol::unblock(move || agent::load_instructions(&cwd)).await;
    }

    fn emit_error(&self, run_id: u64, error: AgentError) {
        error!(error = %error, "agent error");
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        let _ = event_tx.send(AgentEvent::error(&error));
    }
}

const DETACH_REFUSED_MSG: &str = "The turn is waiting on your answer, so it stays here";

type RunOutput = (History, ContextGauge, Result<DoneReason, AgentError>);

enum Step {
    Done(Box<RunOutput>),
    Esc,
    Detach,
}

/// Never finishes, so it only ever loses the race it is in.
async fn forward(run_rx: &flume::Receiver<Envelope>, agent_tx: &flume::Sender<Envelope>) -> Step {
    while let Ok(envelope) = run_rx.recv_async().await {
        let _ = agent_tx.send(envelope);
    }
    future::pending().await
}

/// The rest of a detached run. Its events go to the task's chat under
/// [`DETACHED_RUN_ID`], which the chat never moves past, and its result goes
/// to the session's mailbox. Only a run that got to finish wakes the
/// session: one stopped by the user or by teardown has nothing to report
/// that anyone waits on.
#[allow(clippy::too_many_arguments)]
async fn finish_detached<F: Future<Output = Box<RunOutput>>>(
    run: Pin<Box<F>>,
    run_rx: flume::Receiver<Envelope>,
    info: SubagentInfo,
    agent_tx: flume::Sender<Envelope>,
    subagent_cancels: Arc<CancelMap<String>>,
    detached: Arc<DetachedRuns>,
    session: MakiId,
) {
    let send = |envelope: Envelope| {
        let mut envelope = detach::reroute(envelope, &info);
        envelope.run_id = DETACHED_RUN_ID;
        let _ = agent_tx.send(envelope);
    };
    let pump = async {
        while let Ok(envelope) = run_rx.recv_async().await {
            send(envelope);
        }
        future::pending::<Box<RunOutput>>().await
    };
    let (history, _, result) = *future::or(run, pump).await;
    for envelope in run_rx.try_iter() {
        send(envelope);
    }
    subagent_cancels.cancel_all();
    detached.remove(&info.parent_tool_use_id);

    let task_id = &info.parent_tool_use_id;
    let transcript = history.into_vec();
    let (outcome, wake) = match &result {
        Ok(DoneReason::Cancelled) | Err(AgentError::Cancelled) => ("was cancelled", false),
        Ok(_) => ("finished", true),
        Err(e) => {
            error!(error = %e, task = %task_id, "background task failed");
            let _ = agent_tx.send(Envelope {
                event: AgentEvent::error(e),
                subagent: Some(info.clone()),
                run_id: DETACHED_RUN_ID,
            });
            ("failed", true)
        }
    };
    let report = detach::finished_report(task_id, &transcript, outcome);
    let _ = agent_tx.send(Envelope {
        event: AgentEvent::SubagentHistory {
            tool_use_id: task_id.clone(),
            messages: transcript,
        },
        subagent: None,
        run_id: DETACHED_RUN_ID,
    });
    if let Err(e) = SessionMailbox::notify(session, report, wake) {
        tracing::warn!(error = %e, task = %task_id, "background task outlived its session");
    }
}

fn spawn_oauth_for_needs_auth(handle: &McpHandle) {
    let snapshot = handle.reader().load().clone();
    for info in snapshot.infos.iter() {
        let McpServerStatus::NeedsAuth { ref url } = info.status else {
            continue;
        };
        let Some(ref server_url) = info.url else {
            continue;
        };
        let handle = handle.clone();
        let server_name = info.name.clone();
        let server_url = server_url.clone();
        let www_auth = url.clone();
        let oauth = info.oauth.clone();
        let ca_file = info.ca_file.clone();
        smol::spawn(async move {
            let storage = match maki_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            if let Err(e) = maki_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                maki_agent::mcp::oauth::Interaction::Background,
                oauth,
                ca_file.as_deref(),
            )
            .await
            {
                tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                return;
            }
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        })
        .detach();
    }
}
