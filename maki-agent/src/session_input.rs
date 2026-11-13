use maki_providers::{AgentError, ContentBlock, Message, Role};

use crate::mcp::McpHandle;
use crate::mcp::transport::PromptMessage;
use crate::{AgentInput, PromptRole};

/// What a headless host puts on a running session's input. One channel for
/// both keeps a `/compact` in line with the prompts sent around it.
pub enum SessionInput {
    Prompt(AgentInput),
    /// Summarizes the transcript and runs no turn. `settings` carries the
    /// mode and toggles the session records for it; its message is unused.
    Compact {
        settings: AgentInput,
        instructions: Option<String>,
    },
}

impl From<AgentInput> for SessionInput {
    fn from(input: AgentInput) -> Self {
        Self::Prompt(input)
    }
}

/// Turns an MCP prompt reference on {input} into the messages its server
/// returns, landed ahead of the typed text.
pub async fn expand_mcp_prompt(
    mcp: Option<&McpHandle>,
    input: &mut AgentInput,
) -> Result<(), AgentError> {
    let Some(prompt_ref) = input.prompt.take() else {
        return Ok(());
    };
    let Some(mcp) = mcp else {
        return Err(mcp_prompt_error("MCP not available".into()));
    };
    let messages = mcp
        .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
        .await
        .map_err(|e| mcp_prompt_error(e.to_string()))?;
    input
        .preamble
        .extend(messages.into_iter().map(prompt_message));
    Ok(())
}

fn mcp_prompt_error(message: String) -> AgentError {
    AgentError::Tool {
        tool: "mcp_prompt".into(),
        message,
    }
}

fn prompt_message(pm: PromptMessage) -> Message {
    match pm.role {
        PromptRole::Assistant => Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: pm.text }],
            ..Default::default()
        },
        PromptRole::User => Message::user(pm.text),
    }
}
