//! `proxy.aliases`: a name a client is hard-wired to, standing in for a local
//! model. A tool that ships `gpt-4o-mini` in its own config can reach a local
//! model without a config edit.
//!
//! The table is built once at daemon start from `config.yaml` and never changes
//! while the daemon runs, so lookups are read-only. The rules the config page
//! documents:
//!
//! - A real model id always wins. An alias is consulted only after the string
//!   the client sent fails to resolve on its own (see
//!   [`crate::proxy::route::resolve_client_reference`]), so an alias can never
//!   hide a model that really claims that name. A shadowed alias logs one
//!   warning the first time it is shadowed.
//! - An alias names a model and nothing else. Its target is resolved whole, so
//!   it cannot pin a launch name or a preset.
//! - Aliases are not published on `/v1/models` or `/api/tags`, so a listing
//!   stays one row per model.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

/// Client name → model reference, plus the shadow warnings already logged.
///
/// Cloned per proxy connection, so the warn set is shared behind an `Arc`:
/// a shadowed alias has to warn once for the daemon, not once per connection.
#[derive(Debug, Default, Clone)]
pub(crate) struct AliasTable {
  /// Keys are normalized with [`normalize`]; values are the trimmed reference
  /// from config.
  targets: BTreeMap<String, String>,
  /// Names already reported as shadowed by a real model id, so the warning
  /// fires once per name rather than once per request.
  warned: Arc<RwLock<HashSet<String>>>,
}

/// One spelling for every alias name: model references already resolve
/// case-insensitively, so one config entry has to answer to any client casing.
fn normalize(name: &str) -> String {
  name.trim().to_ascii_lowercase()
}

impl AliasTable {
  pub(crate) fn from_config(raw: &BTreeMap<String, String>) -> Self {
    let mut targets: BTreeMap<String, String> = BTreeMap::new();
    for (name, target) in raw {
      let (key, target) = (normalize(name), target.trim().to_string());
      if key.is_empty() || target.is_empty() {
        log::warn!("proxy.aliases: ignoring `{name}` — both the name and the model it points at are required");
        continue;
      }
      if targets.insert(key, target).is_some() {
        log::warn!("proxy.aliases: `{name}` is listed twice; the last entry wins");
      }
    }
    Self {
      targets,
      warned: Arc::new(RwLock::new(HashSet::new())),
    }
  }

  /// The model reference `requested` stands in for, or `None` when it is not an
  /// alias.
  pub(crate) fn target(&self, requested: &str) -> Option<&str> {
    if self.targets.is_empty() {
      return None;
    }
    self.targets.get(&normalize(requested)).map(String::as_str)
  }

  /// Note that `requested` reached a real model even though it is also an alias
  /// name. Returns `true` on the first such report, which is the only one worth
  /// logging.
  pub(crate) fn note_shadowed(&self, requested: &str) -> bool {
    if self.targets.is_empty() {
      return false;
    }
    let key = normalize(requested);
    if !self.targets.contains_key(&key) {
      return false;
    }
    self
      .warned
      .write()
      .map(|mut seen| seen.insert(key))
      .unwrap_or(false)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn table(pairs: &[(&str, &str)]) -> AliasTable {
    let raw: BTreeMap<String, String> = pairs
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect();
    AliasTable::from_config(&raw)
  }

  #[test]
  fn target_resolves_any_client_spelling() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert_eq!(t.target("gpt-4o-mini"), Some("qwen3.8-27b"));
    assert_eq!(t.target("GPT-4o-Mini"), Some("qwen3.8-27b"));
    assert_eq!(t.target("  gpt-4o-mini  "), Some("qwen3.8-27b"));
    assert_eq!(t.target("gpt-4o"), None);
  }

  #[test]
  fn blank_entries_are_dropped_not_stored() {
    let t = table(&[("", "qwen3.8-27b"), ("claude-haiku", "  "), ("ok", " x ")]);
    assert_eq!(t.target(""), None);
    assert_eq!(t.target("claude-haiku"), None);
    // A target keeps its text minus the surrounding blanks.
    assert_eq!(t.target("ok"), Some("x"));
  }

  #[test]
  fn shadow_is_reported_once_per_name() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.note_shadowed("some-real-model"), "not an alias name");
    assert!(t.note_shadowed("gpt-4o-mini"));
    assert!(
      !t.note_shadowed("GPT-4o-MINI"),
      "the same name must not warn twice"
    );
  }

  #[test]
  fn an_empty_table_answers_nothing_and_never_warns() {
    let t = AliasTable::from_config(&BTreeMap::new());
    assert_eq!(t.target("anything"), None);
    assert!(!t.note_shadowed("anything"));
  }
}
