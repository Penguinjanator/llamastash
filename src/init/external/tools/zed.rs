//! Zed patcher — `~/.config/zed/settings.json`.
//!
//! Schema: `language_models.openai_compatible.<display_name>` with
//! `api_url` + `available_models[]`. Per Zed's docs, API keys live
//! in env (`<DISPLAY_NAME_UPPERCASE>_API_KEY` — i.e.
//! `LLAMASTASH_API_KEY`) and are *not* stored in `settings.json`.
//! We honour that — only the URL and the model list land on disk.
//!
//! `available_models[]` is inside our own `LlamaStash` block so a
//! wholesale array replace only touches entries we own. The default
//! object-recursive merge is fine here — no smart array splicing
//! needed (unlike Continue.dev where the array is at root).
//!
//! **Effort.** Verified against Zed `main` `8e7fbcc13` (release 1.22.0):
//! an `openai_compatible` model turns its effort picker on when it has a
//! `reasoning_effort` default (`provider/open_ai_compatible.rs`), but the
//! picker's list is fixed to `OPENAI_COMPATIBLE_SELECTABLE` (minimal ..
//! max) with no per-model override. So only the default is written, set
//! to the template's own default so nothing changes until the user picks
//! a level; a level the template rejects comes back as the server's error.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::effort::EffortLevels;
use crate::init::external::{Format, PatchContext, ToolPatcher};

pub struct Zed;

/// Levels Zed's `ReasoningEffort` deserialises.
const ZED_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The template's default level, else its highest; `None` when Zed has
/// no name for it.
fn default_effort(effort: &EffortLevels) -> Option<&str> {
  effort
    .default
    .as_deref()
    .or_else(|| effort.levels.last().map(String::as_str))
    .filter(|l| ZED_LEVELS.contains(l))
}

impl ToolPatcher for Zed {
  fn id(&self) -> &'static str {
    "zed"
  }
  fn display_name(&self) -> &'static str {
    "Zed"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".config").join("zed").join("settings.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn required_env_var(&self) -> Option<&'static str> {
    // Zed's own convention: the key for an `openai_compatible` provider
    // comes from `<DISPLAY_NAME>_API_KEY` in the environment and is never
    // read out of settings.json.
    Some(super::env_sh::API_KEY_VAR)
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    // Zed's openai_compatible provider drives chat/inline-assist only, so
    // an embedder in the list would show up as a broken assistant model.
    let available_models: Vec<Value> = ctx
      .models
      .iter()
      .filter(|m| !m.is_embed)
      .map(|m| {
        let mut entry = json!({
          "name": m.id,
          "display_name": m.id,
          "max_tokens": m.declared_context(),
          "capabilities": {
            "tools": true,
            "images": false,
            "parallel_tool_calls": false,
            "prompt_cache_key": false,
          }
        });
        if let Some(level) = m.effort.as_ref().and_then(default_effort) {
          entry["reasoning_effort"] = json!(level);
        }
        entry
      })
      .collect();
    json!({
      "language_models": {
        "openai_compatible": {
          "LlamaStash": {
            "api_url": ctx.proxy_base_url,
            "available_models": available_models,
          }
        }
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::init::external::apply;

  fn ctx() -> PatchContext {
    PatchContext::fixture(&["qwen3-coder-30b"])
  }

  #[test]
  fn every_chat_model_is_listed_and_embedders_are_not() {
    // Zed's openai_compatible provider is assistant-only.
    let ctx = PatchContext::fixture(&["qwen3-coder-30b", "nomic-embed-text-v1.5"]);
    let v = Zed.build_additions(&ctx);
    let models = v["language_models"]["openai_compatible"]["LlamaStash"]["available_models"]
      .as_array()
      .expect("array")
      .clone();
    let names: Vec<&str> = models.iter().filter_map(|m| m["name"].as_str()).collect();
    assert_eq!(names, vec!["qwen3-coder-30b"]);
  }

  #[test]
  fn a_model_with_effort_levels_gets_the_templates_default() {
    let mut ctx = PatchContext::fixture(&["Qwen3.8-27B-UD-Q6_K", "plain"]);
    ctx.models[0].effort = Some(EffortLevels {
      levels: vec!["low".into(), "medium".into(), "xhigh".into()],
      default: Some("xhigh".into()),
      can_disable: true,
    });
    let v = Zed.build_additions(&ctx);
    let models = &v["language_models"]["openai_compatible"]["LlamaStash"]["available_models"];
    assert_eq!(models[0]["reasoning_effort"], "xhigh");
    assert!(models[1].get("reasoning_effort").is_none());
  }

  #[test]
  fn a_level_zed_cannot_name_writes_no_default() {
    let odd = EffortLevels {
      levels: vec!["fast".into(), "deep".into()],
      default: None,
      can_disable: false,
    };
    assert_eq!(default_effort(&odd), None);
  }

  #[test]
  fn writes_openai_compatible_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("zed-empty");
    let path = dir.join("settings.json");
    apply(&Zed, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let llamastash = &body["language_models"]["openai_compatible"]["LlamaStash"];
    assert_eq!(llamastash["api_url"], "http://127.0.0.1:11435/v1");
    assert_eq!(llamastash["available_models"][0]["name"], "qwen3-coder-30b");
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn preserves_user_zed_settings_outside_llm_block() {
    let dir = crate::util::test_temp::unique_temp_dir("zed-coexist");
    let path = dir.join("settings.json");
    std::fs::write(
      &path,
      r#"{"theme":"One Dark","ui_font_size":16,"language_models":{"anthropic":{"version":"1"}}}"#,
    )
    .unwrap();
    apply(&Zed, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(body["theme"], "One Dark");
    assert_eq!(body["ui_font_size"], 16);
    assert_eq!(body["language_models"]["anthropic"]["version"], "1");
    assert!(body["language_models"]["openai_compatible"]["LlamaStash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn api_key_not_written_to_settings_per_zed_convention() {
    let v = Zed.build_additions(&ctx());
    let llamastash = &v["language_models"]["openai_compatible"]["LlamaStash"];
    assert!(
      llamastash.get("api_key").is_none(),
      "Zed reads the key from $LLAMASTASH_API_KEY env, not settings.json"
    );
  }
}
