//! `/btw`: a side question over a session's transcript that runs no tools and
//! leaves the history alone. Every frontend asks it the same way.

use std::sync::Arc;

use flume::Sender;
use maki_providers::provider::Provider;
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionTools;
use serde_json::Value;

use crate::{HistorySnapshot, UNAVAILABLE_RESULT, close_dangling_tool_calls, unbind_thinking};

pub const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly \
in a single response.\n- You have NO tools: you cannot read files, run commands, or take any \
action.\n- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing \
conversation context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If \
you don't know, say so; do not offer to look it up.\n</system-reminder>";

pub const BTW_FALLBACK_SYSTEM: &str = "You are a helpful coding assistant. Answer concisely \
from the conversation context.";

pub const NO_TOOLS: Value = Value::Array(Vec::new());

#[derive(Debug, PartialEq)]
pub enum BtwEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Done,
    Error(String),
}

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

/// The tools ride along only for the cache; the reminder keeps the model off
/// them. When it reaches for one anyway, say so instead of showing half an answer.
pub fn reached_for_tool(name: &str) -> String {
    format!(
        "The model tried to call `{name}`, but a side question has no tools. Ask it another way, \
         or send it as a normal message."
    )
}

pub struct BtwRequest {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub system: String,
    pub tools: Arc<Value>,
    pub messages: Vec<Message>,
    pub opts: RequestOptions,
    pub session_id: SessionRef,
}

impl BtwRequest {
    /// Asks {question} over {snapshot}, the session's live transcript, under
    /// {opts}, the options its own turns run with.
    pub fn new(
        snapshot: Option<&HistorySnapshot>,
        question: &str,
        provider: Arc<dyn Provider>,
        model: Model,
        opts: RequestOptions,
        session_id: SessionRef,
    ) -> Self {
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        let mut messages = snapshot
            .map(|s| Vec::clone(&s.messages))
            .unwrap_or_default();
        close_dangling_tool_calls(&mut messages, UNAVAILABLE_RESULT);
        messages.push(btw_question(question));
        // The session's own prompt, so the side question reads the transcript
        // under the instructions it was written under.
        let system = snapshot
            .and_then(|s| s.frame.as_ref())
            .and_then(|f| f.prefix())
            .unwrap_or(BTW_FALLBACK_SYSTEM)
            .to_owned();
        // Anthropic keys its cache on tools, system, then messages under one
        // thinking setting, so the request repeats the session's tools and
        // thinking (`tool_choice: none` would void it). A resumed session has
        // no wire before its first run, and a switched model has no cache to
        // hit and may reject the old wire. Either way the transcript's
        // thinking signatures are bound to a prefix this request cannot
        // rebuild. With thinking off the API would ignore them, but some
        // models cannot turn it off, so the signatures go and the reasoning
        // text stays.
        let fitted = snapshot
            .and_then(|s| s.tools.as_ref())
            .filter(|tools| tools.model == model.spec());
        let (tools, opts) = match fitted {
            Some(SessionTools { wire, .. }) => (Arc::clone(wire), opts),
            None => {
                unbind_thinking(&mut messages);
                (Arc::new(NO_TOOLS), RequestOptions::default())
            }
        };
        Self {
            opts: opts.clamped(&model),
            provider,
            model,
            system,
            tools,
            messages,
            session_id,
        }
    }

    pub async fn run(self, btw_tx: Sender<BtwEvent>) {
        let (event_tx, event_rx) = flume::unbounded();
        let messages = maki_providers::adapt_images_for_model(&self.model, &self.messages).await;

        // The pinned future outlives the stream, so the sender is dropped by
        // hand or `forward` never sees the channel close.
        let stream = async {
            let result = self
                .provider
                .stream_message(
                    &self.model,
                    &messages,
                    &self.system,
                    &self.tools,
                    &event_tx,
                    self.opts,
                    Some(&self.session_id),
                )
                .await;
            drop(event_tx);
            result
        };

        // False when it stopped early, on a tool call or a closed receiver,
        // so the stream's outcome is never sent.
        let forward = async {
            while let Ok(event) = event_rx.recv_async().await {
                let event = match event {
                    ProviderEvent::TextDelta { text } => BtwEvent::TextDelta(text),
                    ProviderEvent::ThinkingDelta { text } => BtwEvent::ThinkingDelta(text),
                    ProviderEvent::ToolUseStart { name, .. } => {
                        let _ = btw_tx.send(BtwEvent::Error(reached_for_tool(&name)));
                        return false;
                    }
                    ProviderEvent::PromptProgress { .. } => continue,
                };
                if btw_tx.send(event).is_err() {
                    return false;
                }
            }
            true
        };

        // The channel only closes once `stream` is done, so `forward` ending
        // first means it stopped early, and returning cancels the request
        // instead of streaming for nobody.
        tokio::pin!(stream, forward);
        let result = tokio::select! {
            biased;
            result = &mut stream => result,
            _ = &mut forward => return,
        };
        if forward.await {
            let _ = btw_tx.send(match result {
                Ok(_) => BtwEvent::Done,
                Err(e) => BtwEvent::Error(e.to_string()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        AgentError, ContentBlock, ModelInfo, StopReason, StreamResponse, TokenUsage,
    };
    use test_case::test_case;

    use super::*;

    const Q: &str = "why sqlite?";
    const ANSWER: &str = "Let me check.";

    struct ScriptedProvider {
        script: Vec<ProviderEvent>,
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            events: &'a Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                for event in self.script.iter().cloned() {
                    events.send(event).unwrap();
                }
                Ok(StreamResponse {
                    message: Message::default(),
                    usage: TokenUsage::default(),
                    stop_reason: Some(StopReason::EndTurn),
                    input_transformations: Vec::new(),
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    /// `Done` once never came: the provider's sender outlived the stream, so
    /// the modal spun forever.
    #[test_case(None, BtwEvent::Done ; "stream_end_is_done")]
    #[test_case(Some("read"), BtwEvent::Error(reached_for_tool("read")) ; "tool_call_is_a_note")]
    #[tokio::test]
    async fn forwards_every_delta_then_stops(tool: Option<&str>, last: BtwEvent) {
        let (tx, rx) = flume::bounded(64);
        let mut script = vec![ProviderEvent::TextDelta {
            text: ANSWER.into(),
        }];
        script.extend(tool.map(|name| ProviderEvent::ToolUseStart {
            id: "t1".into(),
            name: name.into(),
        }));
        BtwRequest::new(
            None,
            Q,
            Arc::new(ScriptedProvider { script }),
            model(),
            RequestOptions::default(),
            SessionRef::generate(),
        )
        .run(tx)
        .await;
        let events: Vec<BtwEvent> = rx.drain().collect();
        assert_eq!(events, [BtwEvent::TextDelta(ANSWER.into()), last]);
    }

    #[test]
    fn injects_reminder_before_question() {
        let text: String = btw_question(Q)
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }
}
