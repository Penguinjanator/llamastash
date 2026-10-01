//! Reasoning-effort levels a model accepts, read from its chat template.
//!
//! The levels belong to the template, not the engine: llama.cpp passes
//! `reasoning_effort` through to the template as a kwarg, and Qwen3.8's
//! template raises `Unexpected reasoning effort <x>` on anything outside
//! its list. The architecture does not decide it either: Qwen3.8-27B
//! reports `qwen35`, the same as Qwen3.5 and 3.6, whose templates have no
//! effort list. So the list is parsed from the template itself.
//!
//! `none` is handled before the template: llama.cpp turns it into
//! `enable_thinking = false` (`tools/server/server-common.cpp`, master
//! `f872b5911`), which only a template that reads `enable_thinking`
//! honours.

use std::path::Path;

/// Levels a client may send as `reasoning_effort`, lowest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffortLevels {
  pub levels: Vec<String>,
  /// The level the template uses when none is sent
  /// (`reasoning_effort|default('xhigh')`), when it names one it accepts.
  pub default: Option<String>,
  /// `reasoning_effort: "none"` turns thinking off.
  pub can_disable: bool,
}

/// Known level names, lowest first. Unknown names sort after them, in
/// template order.
const RANK: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

impl EffortLevels {
  pub fn accepts(&self, level: &str) -> bool {
    self.levels.iter().any(|l| l == level)
  }
}

/// Parse the accepted levels out of a chat template. Recognises the
/// validated form, `<...>reasoning_effort not in ('a', 'b')` or
/// `<...>reasoning_effort in ['a', 'b']`. A template that uses
/// `reasoning_effort` without validating it (any string goes into the
/// prompt) returns `None`: there is no list to offer.
pub fn from_template(template: &str) -> Option<EffortLevels> {
  let mut levels = template
    .match_indices("reasoning_effort")
    .find_map(|(i, m)| validated_list(&template[i + m.len()..]))?;
  levels.sort_by_key(|l| RANK.iter().position(|r| r == l).unwrap_or(RANK.len()));
  let default = template
    .match_indices("reasoning_effort")
    .find_map(|(i, m)| default_value(&template[i + m.len()..]))
    .filter(|d| levels.contains(d));
  Some(EffortLevels {
    levels,
    default,
    can_disable: template.contains("enable_thinking"),
  })
}

/// `rest` starts right after a `reasoning_effort` token. Returns `x` of
/// an immediately following `|default('x')` filter.
fn default_value(rest: &str) -> Option<String> {
  let rest = rest.trim_start().strip_prefix('|')?.trim_start();
  let rest = rest
    .strip_prefix("default")?
    .trim_start()
    .strip_prefix('(')?;
  let rest = rest.trim_start();
  let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
  let body = &rest[1..];
  Some(body[..body.find(quote)?].to_string())
}

/// `rest` starts right after a `reasoning_effort` token. Returns the
/// quoted strings of an immediately following `in (...)` /
/// `not in (...)` test.
fn validated_list(rest: &str) -> Option<Vec<String>> {
  let rest = rest.trim_start();
  let rest = rest
    .strip_prefix("not")
    .map(str::trim_start)
    .unwrap_or(rest);
  let rest = rest.strip_prefix("in")?.trim_start();
  let close = match rest.chars().next()? {
    '(' => ')',
    '[' => ']',
    _ => return None,
  };
  let body = &rest[1..rest.find(close)?];
  let levels: Vec<String> = body
    .split(',')
    .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
    .filter(|s| !s.is_empty())
    .collect();
  (!levels.is_empty()).then_some(levels)
}

/// Whether a local GGUF carries a reasoning token, and the effort levels
/// its template lists. `(false, None)` for anything else (a safetensors
/// repo, a registry entry, an unreadable file).
pub fn from_gguf(path: &Path) -> (bool, Option<EffortLevels>) {
  let is_gguf = path
    .extension()
    .and_then(|e| e.to_str())
    .is_some_and(|e| e.eq_ignore_ascii_case("gguf"));
  if !is_gguf {
    return (false, None);
  }
  let Ok(read) =
    crate::gguf::header::read_path(path, crate::gguf::header::HeaderReadOptions::default())
  else {
    return (false, None);
  };
  let md = crate::gguf::metadata::summarise(&read.header);
  if !md.reasoning_hint {
    return (false, None);
  }
  (true, md.chat_template.as_deref().and_then(from_template))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The validation block of the chat template shipped in
  /// `unsloth/Qwen3.8-27B-GGUF` and `unsloth/Qwen3.8-Flash-Next-GGUF`.
  const QWEN38: &str = "{%- if enable_thinking is undefined or enable_thinking is true %}
    {%- set resolved_reasoning_effort = reasoning_effort|default('xhigh') %}
    {%- if resolved_reasoning_effort == 'high' %}
        {%- set resolved_reasoning_effort = 'xhigh' %}
    {%- endif %}
    {%- if resolved_reasoning_effort not in ('xhigh', 'medium', 'low') %}
        {{- raise_exception('Unexpected reasoning effort ' ~ reasoning_effort ~ '. Supported types are xhigh (default), medium, and low.') }}
    {%- endif %}";

  #[test]
  fn qwen38_levels_come_from_the_validation_tuple() {
    let e = from_template(QWEN38).expect("levels");
    assert_eq!(e.levels, vec!["low", "medium", "xhigh"]);
    assert_eq!(e.default.as_deref(), Some("xhigh"));
    assert!(e.can_disable);
    assert!(!e.accepts("high"));
  }

  #[test]
  fn a_list_form_and_no_thinking_switch_parse_too() {
    let e =
      from_template("{% if reasoning_effort in [\"low\", \"high\"] %}{% endif %}").expect("levels");
    assert_eq!(e.levels, vec!["low", "high"]);
    assert_eq!(e.default, None);
    assert!(!e.can_disable);
  }

  #[test]
  fn an_unvalidated_effort_or_no_effort_offers_nothing() {
    assert!(from_template("Reasoning: {{ reasoning_effort | default('medium') }}").is_none());
    assert!(from_template("{{ messages }}").is_none());
  }

  #[test]
  fn a_gguf_is_read_only_when_it_carries_a_reasoning_token() {
    use crate::gguf::header::GgufValue;
    use crate::gguf::test_fixtures::FixtureBuilder;
    let dir = crate::util::test_temp::unique_temp_dir("effort-gguf");
    let tokens = |think: bool| {
      let mut t = vec![GgufValue::String("<bos>".into())];
      if think {
        t.push(GgufValue::String("<think>".into()));
      }
      GgufValue::Array(t)
    };
    for (name, think) in [("think.gguf", true), ("plain.gguf", false)] {
      let bytes = FixtureBuilder::new()
        .with_arch("qwen35")
        .with_chat_template(QWEN38)
        .with_kv("tokenizer.ggml.tokens", tokens(think))
        .build();
      std::fs::write(dir.join(name), bytes).unwrap();
    }
    let (reasoning, effort) = from_gguf(&dir.join("think.gguf"));
    assert!(reasoning);
    assert_eq!(
      effort.expect("levels").levels,
      vec!["low", "medium", "xhigh"]
    );
    assert_eq!(from_gguf(&dir.join("plain.gguf")), (false, None));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn non_gguf_paths_are_not_read() {
    assert_eq!(
      from_gguf(Path::new("/hub/models--Qwen--Qwen3-0.6B/snapshots/abc")),
      (false, None)
    );
  }
}
