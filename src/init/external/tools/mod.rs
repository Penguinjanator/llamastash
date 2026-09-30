//! Per-tool patcher modules. Each module declares one
//! [`super::ToolPatcher`]; [`registered`] returns the full list in
//! picker display order.

pub mod aider;
pub mod claude_code;
pub mod codex;
pub mod continue_dev;
pub mod env_sh;
pub mod opencode;
pub mod pi_dev;
pub mod zed;

use super::ToolPatcher;

/// Every registered patcher, in picker display order. Order matters:
/// it's what `cliclack::multiselect` presents to the user. The two
/// sourceable-env writers (`env-sh` for OpenAI, `claude-code` for
/// Anthropic) sit last as the "and also set shell envs" affordances
/// after the per-tool config entries. Each writes its own `.sh`
/// snippet so a user can pick one without the other; the Claude Code
/// vars never land in that tool's global `~/.claude/settings.json`.
pub fn registered() -> Vec<Box<dyn ToolPatcher>> {
  vec![
    Box::new(opencode::OpenCode),
    Box::new(aider::Aider),
    Box::new(continue_dev::ContinueDev),
    Box::new(zed::Zed),
    Box::new(pi_dev::PiDev),
    Box::new(codex::Codex),
    Box::new(env_sh::EnvSh),
    Box::new(claude_code::ClaudeCode),
  ]
}
