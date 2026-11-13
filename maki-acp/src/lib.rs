pub mod commands;
pub mod elicitation;
pub mod host;
pub mod methods;
pub mod permissions;
pub mod server;
pub mod translate;

use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use maki_agent::permissions::PluginRuleStore;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::{AgentConfig, CancelToken, HostPrompt, SessionEndReason};
use maki_config::project::TrustMode;
use maki_config::{ModelPolicy, SessionDefaults, TrustConfig};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_storage::id::MakiId;

pub type SessionEndHook =
    Arc<dyn Fn(MakiId, SessionEndReason) -> BoxFuture<'static, ()> + Send + Sync>;

pub struct AcpParams {
    pub model: Model,
    pub config: AgentConfig,
    pub timeouts: Timeouts,
    pub initial_wd: PathBuf,
    pub prompt_slots: Arc<ResolvedPrompts>,
    pub yolo: bool,
    /// What a session starts with. `/fast` and `/workflow` flip that session's
    /// copy, never this one.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    /// How a session cwd with no stored trust decision is treated. `--trust`
    /// makes it [`TrustMode::Session`], which covers every cwd the client picks.
    pub trust_mode: TrustMode,
    /// Folders the global `init.lua` answered for in advance. ACP never asks,
    /// so this is the only yes a cwd with no stored decision can get.
    pub trust_policy: Arc<TrustConfig>,
    /// Called with the reason when an ACP session is replaced or the server
    /// exits. The hook answers with a future so its wait rides the executor
    /// instead of holding stdin for the whole `SessionEnd` grace period.
    pub on_session_end: Option<SessionEndHook>,
    pub storage: StateDir,
    /// `false` keeps every tool call blocking. Otherwise calls can move to the
    /// background and their results ride the next prompt. The server is long
    /// lived, so it never waits for them: closing the session stops them.
    pub background: bool,
    pub host_prompt: HostPrompt,
    /// Firing it shuts the server down as end of input does, without waiting
    /// for the client to close stdin.
    pub stop: CancelToken,
    /// Where slash commands the session cannot answer alone are looked up and run.
    pub commands: Arc<dyn commands::CommandHost>,
    /// Plugin requests for the session, `maki.session.prompt` first of all.
    pub host_requests: Option<flume::Receiver<host::HostRequest>>,
}

pub async fn run(params: AcpParams) -> color_eyre::Result<()> {
    server::serve(params).await
}
