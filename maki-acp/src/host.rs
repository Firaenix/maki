//! What the host process asks of the ACP session on a plugin's behalf. A Lua
//! command that calls `maki.session.prompt` lands here, since an ACP server
//! has no TUI to take it.

use maki_agent::ImageSource;
use serde_json::Value;
use tokio::sync::oneshot;

/// The words `maki.session.prompt` answers with, matching the TUI's.
pub const PROMPT_STARTED: &str = "started";
pub const PROMPT_QUEUED: &str = "queued";
pub const NO_SESSION_ERR: &str = "no live session";
pub const OTHER_SESSION_ERR: &str = "this agent only runs its current session";

pub type HostReply = Result<Value, String>;

pub enum HostRequest {
    /// `maki.session.prompt`. {session} is `None` for the current one.
    Prompt {
        session: Option<String>,
        text: String,
        images: Vec<ImageSource>,
        reply: oneshot::Sender<HostReply>,
    },
    /// What a plugin showed, a toast, a flash or a window's contents, as text:
    /// ACP has no windows. It joins the turn of the Lua command waiting on its
    /// handler, and is dropped when none waits.
    Say { text: String },
}
