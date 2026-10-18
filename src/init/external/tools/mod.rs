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

#[cfg(test)]
mod tests {
  use super::super::{PatchContext, SessionLaunch};
  use super::registered;

  /// A key that would be a real bearer token if the proxy enforced auth.
  const SECRET: &str = "s3cr3t-bearer-token";
  const SESSION_TOOLS: &[&str] = &["claude-code", "codex", "opencode", "pi", "aider"];

  fn ctx() -> PatchContext {
    let mut ctx = PatchContext::fixture(&["qwen3-coder-30b"]);
    ctx.api_key = SECRET.to_string();
    ctx
  }

  fn session(id: &str) -> Option<SessionLaunch> {
    let patcher = registered()
      .into_iter()
      .find(|p| p.id() == id)
      .expect("registered patcher");
    // Nothing is written: the session hook only describes files.
    patcher.session(&ctx(), std::path::Path::new("/sessions"))
  }

  #[test]
  fn the_five_coding_tools_describe_a_per_run_launch() {
    for id in SESSION_TOOLS {
      assert!(session(id).is_some(), "{id} offers no per-run launch");
    }
  }

  #[test]
  fn an_editor_or_shell_target_declines_to_launch() {
    // Continue is an IDE extension, Zed an editor, and env-sh is not a tool.
    for id in ["continue", "zed", "env-sh"] {
      assert!(session(id).is_none(), "{id} must not offer a session");
    }
  }

  #[test]
  fn the_key_reaches_only_the_child_env() {
    for id in SESSION_TOOLS {
      let launch = session(id).unwrap_or_else(|| panic!("{id} offers no per-run launch"));
      assert!(
        launch.env.iter().any(|(_, v)| v == SECRET),
        "{id} must pass the key through env"
      );
      for arg in &launch.args {
        assert!(!arg.contains(SECRET), "{id} puts the key in argv");
      }
      for file in &launch.files {
        assert!(
          !file.content.contains(SECRET),
          "{id} puts the key in {}",
          file.path.display()
        );
      }
    }
  }
}
