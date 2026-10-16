use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use maki_agent::{
    CancelMap, CancelTrigger, Envelope, ExtractedCommand, InterruptSource, SubagentInbox,
    SubagentInfo,
};
use maki_providers::{ContentBlock, Message, Role};

use super::shared_queue::QueueReceiver;

pub(super) const DETACHED_TASK_PREFIX: &str = "background-";
pub(super) const DETACHED_TASK_NAME: &str = "background";
/// Stamped on everything a detached run sends. The chat bumps its own run id
/// for every turn it starts, and a background task has to keep reaching its
/// chat through all of them.
pub(crate) const DETACHED_RUN_ID: u64 = u64::MAX - 1;

/// What a run polls for queued input. Until it is detached that is the
/// chat's queue. After, it is the task's own inbox, or the run would eat the
/// very messages the chat moved on to answer.
pub(super) struct RunInterrupts {
    queue: Arc<QueueReceiver>,
    inbox: Arc<SubagentInbox>,
    detached: AtomicBool,
}

impl RunInterrupts {
    pub(super) fn new(queue: Arc<QueueReceiver>) -> Self {
        Self {
            queue,
            inbox: Arc::default(),
            detached: AtomicBool::new(false),
        }
    }

    pub(super) fn detach(&self) -> Arc<SubagentInbox> {
        self.detached.store(true, Ordering::Release);
        Arc::clone(&self.inbox)
    }
}

impl InterruptSource for RunInterrupts {
    fn poll(&self) -> Option<ExtractedCommand> {
        if self.detached.load(Ordering::Acquire) {
            self.inbox.poll()
        } else {
            self.queue.poll()
        }
    }
}

/// Runs the loop let go of, so esc in their chat and teardown still reach
/// them. Each keeps the subagent map it started with: the loop's next run
/// clears its own map when it ends, and that must not stop work a background
/// task still waits on.
#[derive(Default)]
pub(super) struct DetachedRuns(Mutex<HashMap<String, DetachedRun>>);

struct DetachedRun {
    /// Held, never read. Dropping it is what stops the run.
    _stop: CancelTrigger,
    subagents: Arc<CancelMap<String>>,
}

impl DetachedRuns {
    pub(super) fn insert(
        &self,
        task_id: String,
        stop: CancelTrigger,
        subagents: Arc<CancelMap<String>>,
    ) {
        self.lock().insert(
            task_id,
            DetachedRun {
                _stop: stop,
                subagents,
            },
        );
    }

    pub(super) fn remove(&self, task_id: &str) {
        self.lock().remove(task_id);
    }

    /// {id} is a background task or a subagent one of them started. Ids are
    /// unique, so marking it in every map stops only the one that owns it.
    pub(super) fn cancel(&self, id: &str) {
        let mut runs = self.lock();
        if runs.remove(id).is_some() {
            return;
        }
        for run in runs.values() {
            run.subagents.cancel(id.to_owned());
        }
    }

    pub(super) fn cancel_all(&self) {
        for (_, run) in self.lock().drain() {
            run.subagents.cancel_all();
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, DetachedRun>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Events of the run itself move to the task's chat. A nested subagent's
/// already name their own chat, so they pass as they are.
pub(super) fn reroute(mut envelope: Envelope, info: &SubagentInfo) -> Envelope {
    if envelope.subagent.is_none() {
        envelope.subagent = Some(info.clone());
    }
    envelope
}

/// What the main chat records in place of the work that left it. The model
/// would otherwise read the open calls as failed and start them over.
pub(super) fn handoff_note(task_id: &str) -> String {
    format!(
        "[The user moved the work above to background task {task_id} so you can answer their \
         next message right away. It keeps running on its own and its result will arrive as \
         a notification. Do not redo it.]"
    )
}

pub(super) fn tool_handoff_result(task_id: &str) -> String {
    format!("[Continues in background task {task_id}]")
}

pub(super) fn finished_report(task_id: &str, transcript: &[Message], outcome: &str) -> String {
    let answer = transcript
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && m.first_text_content().is_some())
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
    let mut report = format!(
        "Background task {task_id} {outcome}. This was the detached background work: the turn \
         the user sent to the background so you could handle their next message."
    );
    if let Some(answer) = answer {
        report.push_str("\n\nIts final answer:\n");
        report.push_str(&answer);
    }
    report
}

#[cfg(test)]
mod tests {
    use maki_agent::{AgentEvent, CancelToken};

    use super::*;

    const TASK: &str = "background-1";
    const NESTED: &str = "nested-tool-use";

    fn info() -> SubagentInfo {
        SubagentInfo {
            parent_tool_use_id: TASK.into(),
            name: DETACHED_TASK_NAME.into(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx: None,
            inbox: None,
        }
    }

    fn envelope(subagent: Option<SubagentInfo>) -> Envelope {
        Envelope {
            event: AgentEvent::Nudge,
            subagent,
            run_id: 0,
        }
    }

    #[test]
    fn reroute_moves_the_runs_own_events_and_leaves_nested_ones() {
        let own = reroute(envelope(None), &info());
        assert_eq!(own.subagent.unwrap().parent_tool_use_id, TASK);

        let nested = SubagentInfo {
            parent_tool_use_id: NESTED.into(),
            ..info()
        };
        let nested = reroute(envelope(Some(nested)), &info());
        assert_eq!(nested.subagent.unwrap().parent_tool_use_id, NESTED);
    }

    #[test]
    fn cancelling_a_task_stops_its_run_and_nothing_else() {
        let runs = DetachedRuns::default();
        let (stop, token) = CancelToken::new();
        let (other_stop, other) = CancelToken::new();
        runs.insert(TASK.into(), stop, Arc::default());
        runs.insert("background-2".into(), other_stop, Arc::default());

        runs.cancel(TASK);

        assert!(token.is_cancelled());
        assert!(!other.is_cancelled());
    }

    #[test]
    fn cancelling_a_nested_subagent_reaches_the_task_that_started_it() {
        let runs = DetachedRuns::default();
        let (stop, task) = CancelToken::new();
        let subagents: Arc<CancelMap<String>> = Arc::default();
        let (nested_stop, nested) = CancelToken::new();
        subagents.insert(NESTED.into(), nested_stop);
        runs.insert(TASK.into(), stop, subagents);

        runs.cancel(NESTED);

        assert!(nested.is_cancelled());
        assert!(!task.is_cancelled(), "the task itself keeps going");
    }

    #[test]
    fn report_carries_the_last_answer() {
        let transcript = [
            Message::user("go".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "all done".into(),
                }],
                ..Default::default()
            },
        ];
        let report = finished_report(TASK, &transcript, "finished");
        assert!(report.contains(TASK));
        assert!(report.ends_with("all done"));
    }
}
