use std::sync::Arc;

use maki_agent::btw::BtwRequest;
use maki_providers::Model;
use maki_providers::provider::Provider;
use maki_storage::id::SessionRef;

use super::App;

impl App {
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let request = BtwRequest::new(
            snapshot.as_deref(),
            &question,
            provider,
            model,
            self.request_options(),
            SessionRef::from(self.state.session.id),
        );
        let (tx, rx) = flume::bounded(64);
        self.open_prompt(|app| app.btw_modal.open(&question, rx));
        tokio::spawn(request.run(tx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_app;
    use crate::components::test_model;
    use arc_swap::ArcSwap;
    use flume::Sender;
    use maki_agent::HistorySnapshot;
    use maki_agent::btw::{BTW_FALLBACK_SYSTEM, NO_TOOLS};
    use maki_agent::prompt::{PromptId, Slot};
    use maki_agent::tools::ToolRegistry;
    use maki_lua::PluginHost;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        AgentError, ContentBlock, Effort, ModelInfo, Role, StopReason, StreamResponse,
        ThinkingConfig, TokenUsage,
    };
    use maki_providers::{Message, ProviderEvent, RequestOptions};
    use maki_storage::frame::{PromptFacts, StoredFrame};
    use maki_storage::sessions::SessionTools;
    use serde_json::Value;
    use serde_json::json;
    use test_case::test_case;

    const Q: &str = "why sqlite?";
    const FRAME_POLICY: &str = "Use the active conversation policy.";
    const OTHER_MODEL: &str = "anthropic/other-model";
    const THOUGHT: &str = "sqlite needs no server";
    const SIGNATURE: &str = "bound-to-the-old-prefix";

    struct Recorded {
        messages: Vec<Message>,
        system: String,
        tools: Value,
        opts: RequestOptions,
    }

    struct FakeProvider {
        sent: Sender<Recorded>,
        script: Vec<ProviderEvent>,
    }

    impl Provider for FakeProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            events: &'a Sender<ProviderEvent>,
            opts: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let _ = self.sent.send(Recorded {
                    messages: messages.to_vec(),
                    system: system.to_owned(),
                    tools: tools.clone(),
                    opts,
                });
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

    fn rendered_frame() -> StoredFrame {
        StoredFrame::new(FRAME_POLICY.into(), String::new(), None, Vec::new())
    }

    fn wire() -> Value {
        json!([{ "name": "read" }])
    }

    fn thought(signature: Option<&str>) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: THOUGHT.into(),
            signature: signature.map(Into::into),
        }
    }

    fn share_history(app: &mut App, frame: StoredFrame, fitted_for: Option<String>) {
        let mut snapshot = HistorySnapshot::new(vec![Message {
            role: Role::Assistant,
            content: vec![thought(Some(SIGNATURE))],
            ..Default::default()
        }]);
        snapshot.frame = Some(Arc::new(frame));
        snapshot.tools = fitted_for.map(|model| SessionTools {
            model,
            wire: Arc::new(wire()),
        });
        app.shared_history = Some(Arc::new(ArcSwap::from_pointee(snapshot)));
    }

    async fn ask(app: &mut App) -> Recorded {
        let (sent, requests) = flume::unbounded();
        let provider = FakeProvider {
            sent,
            script: Vec::new(),
        };
        app.start_btw(Q.into(), Arc::new(provider), test_model());
        requests.recv_async().await.unwrap()
    }

    /// An imported session holds a frame with facts and no prompt until its
    /// first run, and an empty system is a 400 on Anthropic.
    #[test_case(None, BTW_FALLBACK_SYSTEM ; "native_fallback")]
    #[test_case(Some(rendered_frame()), FRAME_POLICY ; "stored_frame_precedence")]
    #[test_case(Some(StoredFrame::facts_only(PromptFacts::default())), BTW_FALLBACK_SYSTEM ; "imported_before_first_run")]
    #[tokio::test]
    async fn side_question_uses_the_selected_policy(frame: Option<StoredFrame>, expected: &str) {
        let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
        host.load_source(
            "persona",
            r#"
            local calls = 0
            maki.api.set_prompt({ slot = "identity", content = function()
                calls += 1
                return tostring(calls)
            end })
            "#,
        )
        .unwrap();
        let mut app = test_app();
        app.lua_event_handle = host.event_handle();
        if let Some(frame) = frame {
            share_history(&mut app, frame, None);
        }
        assert_eq!(ask(&mut app).await.system, expected);
        let prompts = host.event_handle().collect_prompt_slots();
        assert_eq!(
            prompts.get(PromptId::System, Slot::Identity)[0].content,
            "1"
        );
    }

    /// Some models cannot turn thinking off, so without the session's wire the
    /// old signatures must go or Anthropic rejects the request.
    #[test_case(Some(test_model().spec()), wire(), ThinkingConfig::Effort(Effort::High), Some(SIGNATURE) ; "live_wire_shares_the_cache_prefix")]
    #[test_case(Some(OTHER_MODEL.into()), NO_TOOLS, ThinkingConfig::Off, None ; "switched_model_takes_the_no_tools_path")]
    #[test_case(None, NO_TOOLS, ThinkingConfig::Off, None ; "no_live_wire_unbinds_thinking")]
    #[tokio::test]
    async fn side_question_repeats_the_sessions_tools_and_thinking(
        fitted_for: Option<String>,
        tools: Value,
        thinking: ThinkingConfig,
        signature: Option<&str>,
    ) {
        let mut app = test_app();
        app.state.thinking = ThinkingConfig::Effort(Effort::High);
        share_history(&mut app, rendered_frame(), fitted_for);
        let sent = ask(&mut app).await;
        assert_eq!(sent.tools, tools);
        assert_eq!(sent.opts.thinking, thinking);
        assert_eq!(sent.messages[0].content, [thought(signature)]);
    }
}
