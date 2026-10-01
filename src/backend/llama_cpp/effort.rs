//! Claude Code's effort control on the Anthropic Messages surface.
//!
//! Claude Code sends effort as `output_config.effort`
//! ([gateway protocol](https://code.claude.com/docs/en/llm-gateway-protocol)),
//! and llama.cpp's `/v1/messages` → OpenAI translation copies only
//! `temperature` / `top_p` / `top_k` / `stream` / `chat_template_kwargs` plus
//! `thinking.type: enabled` — `output_config` and a top-level
//! `reasoning_effort` never reach the chat template. Measured on build 11310
//! (`f872b5911`), Qwen3.8-27B Q6_K, temp 0, same prompt: `output_config.effort:
//! xhigh` produced the same completion as sending nothing, while the same value
//! under `chat_template_kwargs` changed it. So copy it there.
//!
//! An effort value the model's own chat template does not define (e.g. `max` on
//! Qwen3.8) is passed through and the template's error names the values it
//! accepts — a proxy-side clamp would only hide that.
//!
//! `thinking` is left alone: llama.cpp's translator already reads it (only
//! `type: enabled`, as a token budget), and Claude Code sends
//! `type: adaptive` for a model id it doesn't recognize, which llama.cpp
//! ignores. Remapping `disabled` would fork the engine's own thinking rules
//! into the proxy for a case the effort control does not need.
//!
//! Drop this when llama.cpp maps the Anthropic effort field itself — upstream
//! ggml-org/llama.cpp#20479, open since 2026-03-13.

use crate::util::json_body;

/// Client-facing Anthropic path the proxy routes (path only, no query).
const MESSAGES_ENDPOINT: &str = "/v1/messages";

/// `body` with `output_config.effort` also set as
/// `chat_template_kwargs.reasoning_effort`, or `None` to forward it unchanged.
/// See [`crate::backend::Backend::rewrite_request_body`].
pub(super) fn rewrite_request_body(endpoint: &str, body: &[u8]) -> Option<Vec<u8>> {
  if endpoint != MESSAGES_ENDPOINT {
    return None;
  }
  let entries = json_body::entries(body)?;
  let kwargs = match entries
    .iter()
    .find(|(key, _)| key == "chat_template_kwargs")
    .map(|(_, value)| json_body::entries(value.get().as_bytes()))
  {
    None => None,
    // A client that sent a non-object kwarg bag is not ours to repair.
    Some(None) => return None,
    Some(Some(inner)) => {
      // A client that set the kwarg itself already chose its effort; the
      // Anthropic field never overrides it.
      if inner.iter().any(|(key, _)| key == "reasoning_effort") {
        return None;
      }
      Some(inner)
    }
  };
  let effort = entries
    .iter()
    .find(|(key, _)| key == "output_config")
    .and_then(|(_, value)| json_body::entries(value.get().as_bytes()))
    .and_then(|config| {
      config
        .into_iter()
        .find(|(key, _)| key == "effort")
        .map(|(_, value)| value.get().as_bytes().to_vec())
    })?;
  // Only a JSON string is an effort. A non-string is not ours to interpret,
  // and the template ignores a non-string kwarg anyway.
  if !effort.starts_with(b"\"") {
    return None;
  }

  let kwargs_body = match kwargs {
    Some(kwargs) => json_body::write_object(
      kwargs
        .iter()
        .map(|(key, value)| (key.as_str(), value.get().as_bytes()))
        .chain(std::iter::once(("reasoning_effort", effort.as_slice()))),
    ),
    None => json_body::write_object([("reasoning_effort", effort.as_slice())]),
  };

  // Replace `chat_template_kwargs` in place so every other entry keeps both
  // its bytes and its position; append it when the body has none.
  let mut out: Vec<(&str, &[u8])> = Vec::with_capacity(entries.len() + 1);
  let mut replaced = false;
  for (key, value) in &entries {
    if key == "chat_template_kwargs" {
      out.push((key.as_str(), kwargs_body.as_slice()));
      replaced = true;
    } else {
      out.push((key.as_str(), value.get().as_bytes()));
    }
  }
  if !replaced {
    out.push(("chat_template_kwargs", kwargs_body.as_slice()));
  }
  Some(json_body::write_object(out))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rewrite(body: &str) -> Option<Vec<u8>> {
    rewrite_request_body(MESSAGES_ENDPOINT, body.as_bytes())
  }

  #[test]
  fn adds_chat_template_kwargs_when_absent() {
    assert_eq!(
      rewrite(r#"{"model":"q","output_config":{"effort":"xhigh"}}"#).expect("rewritten"),
      br#"{"model":"q","output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":"xhigh"}}"#,
      "output_config stays, every other byte is the body's own",
    );
  }

  #[test]
  fn merges_into_existing_kwargs_in_place() {
    // `output_config` is copied verbatim, inner whitespace and all. The two
    // objects this rewrite rebuilds are emitted compactly.
    assert_eq!(
      rewrite(r#"{"stream": true,"chat_template_kwargs": {"preserve_thinking": true },"output_config":{"effort":"low","format":{} }}"#)
        .expect("rewritten"),
      r#"{"stream":true,"chat_template_kwargs":{"preserve_thinking":true,"reasoning_effort":"low"},"output_config":{"effort":"low","format":{} }}"#
        .as_bytes(),
    );
  }

  #[test]
  fn client_set_reasoning_effort_wins() {
    for body in [
      r#"{"output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":"low"}}"#,
      // Present but null still counts as the client's own choice — the engine
      // decides what a null kwarg means, we do not second-guess it.
      r#"{"output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":null}}"#,
    ] {
      assert_eq!(rewrite(body), None, "{body}");
    }
  }

  #[test]
  fn forwards_untouched_when_there_is_nothing_to_map() {
    for body in [
      // No effort field at all — the byte-pure contract Claude Code depends on.
      r#"{"model":"q","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
      r#"{"model":"q","output_config":{}}"#,
      r#"{"model":"q","output_config":{"format":{"type":"json_schema"}}}"#,
      // Non-string effort: the template's business, not ours.
      r#"{"output_config":{"effort":5}}"#,
      r#"{"output_config":{"effort":{"level":"high"}}}"#,
      // output_config as a non-object.
      r#"{"output_config":"xhigh"}"#,
      // chat_template_kwargs as a non-object: cannot be merged into.
      r#"{"chat_template_kwargs":"x","output_config":{"effort":"low"}}"#,
      // Not a JSON object.
      "not json",
      "[1]",
      "",
    ] {
      assert_eq!(rewrite(body), None, "{body}");
    }
  }

  #[test]
  fn only_the_messages_endpoint_is_rewritten() {
    let body = r#"{"output_config":{"effort":"low"}}"#;
    assert_eq!(
      rewrite_request_body("/v1/messages/count_tokens", body.as_bytes()),
      None
    );
    assert_eq!(
      rewrite_request_body("/v1/chat/completions", body.as_bytes()),
      None
    );
    assert!(rewrite_request_body(MESSAGES_ENDPOINT, body.as_bytes()).is_some());
  }

  #[test]
  fn effort_string_is_copied_with_json_escaping_intact() {
    // The raw bytes are reused, so an escaped value arrives escaped.
    assert_eq!(
      rewrite(r#"{"output_config":{"effort":"a\"b"}}"#).expect("rewritten"),
      br#"{"output_config":{"effort":"a\"b"},"chat_template_kwargs":{"reasoning_effort":"a\"b"}}"#,
    );
  }
}
