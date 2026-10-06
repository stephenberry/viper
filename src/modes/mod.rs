//! Non-interactive front ends.

pub mod print;
pub mod rpc;

use anyhow::Result;

use crate::agent::Agent;
use crate::message::ContentBlock;

/// Expand `/skill:name args` in the first text block of `content`.
pub fn expand_skill(agent: &Agent, mut content: Vec<ContentBlock>) -> Result<Vec<ContentBlock>> {
    if let Some(ContentBlock::Text { text }) = content.first_mut()
        && let Some(expanded) = crate::context::expand_skill_command(text, &agent.setup().skills)?
    {
        *text = expanded;
    }
    Ok(content)
}
