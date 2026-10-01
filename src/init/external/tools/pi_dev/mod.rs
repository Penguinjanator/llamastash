//! pi.dev patcher — `~/.pi/agent/models.json` plus the `enabledModels`
//! scope in [`settings`], so a registered model is also *reachable* in
//! pi's model switcher without the user widening the scope by hand.
//!
//! Schema: `providers.<id>` with `baseUrl`, `api: "openai-completions"`,
//! `apiKey`, and a `models[]` array.
//!
//! The `apiKey` field takes a literal, a `$ENV_VAR` reference, or a
//! `!command` that pi runs and reads stdout from. We use the command form
//! (`!llamastash api-key`): the literal token never lands on disk, and
//! unlike the env reference it works in a terminal that has not sourced
//! anything — `$LLAMASTASH_API_KEY` is unset in a fresh shell, which left
//! the provider configured but unusable with "No API key found".
//!
//! The `models[]` array is inside our own `llamastash` provider
//! block, so a wholesale replace only touches our entries — the
//! default object-recursive merge is fine.
//!
//! **Effort.** A model whose chat template lists effort levels gets
//! `reasoning: true` and a `thinkingLevelMap`. Verified against pi `main`
//! `17f3dccbe` (release 0.99.2): without `reasoning` pi sends no effort
//! at all; `getSupportedThinkingLevels` (`packages/ai/src/models.ts`)
//! hides a level mapped to `null` and shows `xhigh`/`max` only when
//! mapped; the default `openai` thinking format sends the mapped string
//! as `reasoning_effort`, and a string `off` is sent when thinking is
//! off (`packages/ai/src/api/openai-completions.ts`).
//!
//! **Chat models only.** `api` is provider-level and pi's api registry
//! has exactly one OpenAI-shaped entry, `openai-completions` — there is no
//! embeddings api (verified against pi 0.84.2, `BUILTIN_APIS` in
//! `packages/ai/dist/compat.js`). pi is a coding agent and never calls
//! `/v1/embeddings`, so an embedder registered here would only fail at
//! stream time. They are left out.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::init::external::effort::EffortLevels;
use crate::init::external::{Format, PatchContext, ToolPatcher};

pub mod settings;

/// pi's levels, in its own order (`EXTENDED_THINKING_LEVELS`).
const PI_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// Each pi level mapped to what the template accepts, or `null` to hide
/// it. `off` maps to `none`, which llama.cpp turns into thinking off,
/// when the template honours that; otherwise `off` is hidden, since
/// sending nothing leaves the template's default effort on.
fn thinking_level_map(effort: &EffortLevels) -> Value {
  let mut map = serde_json::Map::new();
  map.insert(
    "off".into(),
    if effort.can_disable {
      json!("none")
    } else {
      Value::Null
    },
  );
  for level in PI_LEVELS {
    // Aliases stay hidden: they do the same as the level they map to.
    let v = if effort.levels.iter().any(|l| l == level) {
      json!(level)
    } else {
      Value::Null
    };
    map.insert((*level).into(), v);
  }
  Value::Object(map)
}

pub struct PiDev;

pub const PROVIDER: &str = "llamastash";

/// pi runs this and uses stdout as the credential. Resolved per pi
/// process, so a rotated key is picked up on the next start without
/// re-patching anything.
const API_KEY_COMMAND: &str = "!llamastash api-key";

impl ToolPatcher for PiDev {
  fn id(&self) -> &'static str {
    "pi"
  }
  fn display_name(&self) -> &'static str {
    "pi.dev"
  }
  fn default_path(&self) -> Option<PathBuf> {
    crate::util::paths::home_dir().map(|h| h.join(".pi").join("agent").join("models.json"))
  }
  fn format(&self) -> Format {
    Format::Json
  }
  fn build_additions(&self, ctx: &PatchContext) -> Value {
    let models: Vec<Value> = ctx
      .models
      .iter()
      .filter(|m| !m.is_embed)
      .map(|m| {
        // `maxTokens` goes out as `max_completion_tokens`, and llama.cpp
        // stops there, so it must leave room for a long think.
        let mut entry = json!({
          "id": m.id,
          "name": m.id,
          "contextWindow": m.declared_context(),
          "maxTokens": m.declared_output(),
        });
        if m.vision {
          entry["input"] = json!(["text", "image"]);
        }
        if let Some(effort) = &m.effort {
          entry["reasoning"] = json!(true);
          entry["thinkingLevelMap"] = thinking_level_map(effort);
        }
        entry
      })
      .collect();
    json!({
      "providers": {
        PROVIDER: {
          "name": "LlamaStash",
          "baseUrl": ctx.proxy_base_url,
          "api": "openai-completions",
          "apiKey": API_KEY_COMMAND,
          "models": models,
        }
      }
    })
  }
  fn companions(&self) -> Vec<Box<dyn ToolPatcher>> {
    vec![Box::new(settings::PiSettings)]
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
  fn writes_provider_block_into_empty_file() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-empty");
    let path = dir.join("models.json");
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["llamastash"]["baseUrl"],
      "http://127.0.0.1:11435/v1"
    );
    assert_eq!(body["providers"]["llamastash"]["api"], "openai-completions");
    assert_eq!(
      body["providers"]["llamastash"]["models"][0]["id"],
      "qwen3-coder-30b"
    );
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn every_chat_model_lands_in_the_models_array() {
    let ctx = PatchContext::fixture(&["qwen3-coder-30b", "Qwen/Qwen3-0.6B"]);
    let v = PiDev.build_additions(&ctx);
    let ids: Vec<&str> = v["providers"]["llamastash"]["models"]
      .as_array()
      .expect("array")
      .iter()
      .filter_map(|m| m["id"].as_str())
      .collect();
    assert_eq!(ids, vec!["qwen3-coder-30b", "Qwen/Qwen3-0.6B"]);
  }

  #[test]
  fn preserves_user_providers_alongside_llamastash() {
    let dir = crate::util::test_temp::unique_temp_dir("pi-coexist");
    let path = dir.join("models.json");
    std::fs::write(
      &path,
      r#"{"providers":{"openai":{"baseUrl":"https://api.openai.com/v1","api":"openai-completions"}}}"#,
    )
    .unwrap();
    apply(&PiDev, &ctx(), Some(path.clone())).expect("apply");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
      body["providers"]["openai"]["baseUrl"],
      "https://api.openai.com/v1"
    );
    assert!(body["providers"]["llamastash"].is_object());
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn the_api_key_is_resolved_by_shelling_out_never_written_literally() {
    let v = PiDev.build_additions(&ctx());
    // An env reference would leave a fresh terminal with no key at all,
    // and a literal would put the secret in a file people commit.
    assert_eq!(
      v["providers"]["llamastash"]["apiKey"],
      "!llamastash api-key"
    );
    assert!(
      !serde_json::to_string(&v)
        .unwrap()
        .contains("llamastash-secret"),
      "the resolved key never appears in the file"
    );
  }

  #[test]
  fn embedders_are_left_out_entirely() {
    // pi 0.84.2 has no embeddings api — registering one would only fail at
    // stream time, and pi never calls `/v1/embeddings` anyway.
    let v = PiDev.build_additions(&PatchContext::fixture(&[
      "nomic-embed-text-v1.5",
      "qwen3-coder-30b",
    ]));
    let ids: Vec<&str> = v["providers"]["llamastash"]["models"]
      .as_array()
      .expect("array")
      .iter()
      .filter_map(|m| m["id"].as_str())
      .collect();
    assert_eq!(ids, vec!["qwen3-coder-30b"]);
    assert_eq!(v["providers"].as_object().expect("providers").len(), 1);
  }

  #[test]
  fn the_model_scope_is_patched_as_a_companion() {
    // The provider block alone leaves the models out of pi's switcher
    // scope, so the second file travels with the first.
    let ids: Vec<&str> = PiDev.companions().iter().map(|c| c.id()).collect();
    assert_eq!(ids, vec!["pi-settings"]);
  }

  #[test]
  fn a_model_with_effort_levels_gets_reasoning_and_a_level_map() {
    let mut ctx = PatchContext::fixture(&["Qwen3.8-27B-UD-Q6_K", "qwen3-coder-30b"]);
    ctx.models[0].effort = Some(EffortLevels {
      levels: vec!["low".into(), "medium".into(), "xhigh".into()],
      aliases: vec!["high".into()],
      default: Some("xhigh".into()),
      can_disable: true,
    });
    let v = PiDev.build_additions(&ctx);
    let models = &v["providers"]["llamastash"]["models"];
    assert_eq!(models[0]["reasoning"], true);
    // `high` is an alias of `xhigh`, so it is not offered beside it.
    assert_eq!(
      models[0]["thinkingLevelMap"],
      json!({"off": "none", "minimal": null, "low": "low", "medium": "medium",
             "high": null, "xhigh": "xhigh", "max": null})
    );
    // No levels, no fields: pi's defaults stay as they were.
    assert!(models[1].get("reasoning").is_none());
    assert!(models[1].get("thinkingLevelMap").is_none());
  }

  #[test]
  fn the_output_cap_leaves_room_to_think_and_vision_models_take_images() {
    let mut ctx = PatchContext::fixture(&["Qwen3.8-27B-UD-Q6_K", "small-vl"]);
    ctx.models[0].context_window = Some(262_144);
    ctx.models[1].context_window = Some(8192);
    ctx.models[1].vision = true;
    let v = PiDev.build_additions(&ctx);
    let models = &v["providers"]["llamastash"]["models"];
    assert_eq!(models[0]["maxTokens"], 32_000);
    assert!(models[0].get("input").is_none(), "pi's default is text");
    assert_eq!(models[1]["maxTokens"], 4096);
    assert_eq!(models[1]["input"], json!(["text", "image"]));
  }

  #[test]
  fn off_is_hidden_when_the_template_ignores_enable_thinking() {
    let map = thinking_level_map(&EffortLevels {
      levels: vec!["low".into(), "high".into()],
      default: None,
      can_disable: false,
      ..Default::default()
    });
    assert_eq!(map["off"], Value::Null);
    assert_eq!(map["high"], "high");
  }

  #[test]
  fn chat_model_keeps_openai_completions() {
    let v = PiDev.build_additions(&ctx());
    assert_eq!(v["providers"]["llamastash"]["api"], "openai-completions");
  }
}
