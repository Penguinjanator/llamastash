//! `proxy.aliases`: a name a client is hard-wired to, standing in for a local
//! model. A tool that ships `gpt-4o-mini` in its own config can reach a local
//! model without a config edit.
//!
//! The table is built once at daemon start from `config.yaml` and never changes
//! while the daemon runs, so lookups are read-only. The rules the config page
//! documents:
//!
//! - A reference that names a model outright wins. An alias is consulted when the
//!   string the client sent does not name a model on its own (see
//!   [`crate::proxy::route::resolve_client_reference`]), so an alias can never
//!   hide a model that really claims that name — but it does beat a partial match
//!   and a name two models share, which is why it was written. A shadowed alias
//!   logs one warning the first time it is shadowed, and an alias pointing at
//!   nothing logs once the first time a request hits it.
//! - An alias names a model and nothing else. Its target is resolved whole, so
//!   it cannot pin a launch name or a preset, and a target that only matches a
//!   model as a substring is refused rather than guessed at. A target that names
//!   another alias is an error, because a chain names no model; when the same
//!   string is also a real model, that model answers.
//! - Aliases are not published on `/v1/models` or `/api/tags`, so a listing
//!   stays one row per model.

use std::collections::HashSet;
use std::sync::RwLock;

/// Client name → model reference, plus the warnings already logged about it.
///
/// One table per daemon, held behind an `Arc` on `ProxyState` so a connection
/// cloning that struct shares it. That sharing is what makes "one line per
/// problem" true: a table copied per connection would warn again for each copy.
#[derive(Debug, Default)]
pub(crate) struct AliasTable {
  /// `(name, model reference)` in config order, names already through
  /// [`normalize`]. A table is small and scanned once per request that misses,
  /// so the order the operator wrote is worth keeping: it is what makes a
  /// duplicate name resolve to the entry they put last.
  targets: Vec<(String, String)>,
  /// Names already reported, so a warning fires once per name rather than once
  /// per request.
  warned: RwLock<HashSet<String>>,
}

/// One spelling for every alias name: model references already resolve
/// case-insensitively, so one config entry has to answer to any client casing.
fn normalize(name: &str) -> String {
  name.trim().to_ascii_lowercase()
}

/// A report is about one configured alias name, and one kind of problem, so the
/// two together are what a once-per-name line is keyed by.
fn report_key(requested: &str, kind: &str, targets: &[(String, String)]) -> Option<String> {
  let key = normalize(requested);
  targets
    .iter()
    .any(|(name, _)| *name == key)
    .then(|| format!("{kind} {key}"))
}

/// The `kind` tag `note_overrides_partial` claims, so a caller can check
/// [`AliasTable::report_pending`] before paying for the resolve that feeds it.
pub(crate) const REPORT_PARTIAL: &str = "partial";

impl AliasTable {
  /// The table behind `proxy.aliases`, in the order the file lists the entries.
  pub(crate) fn from_config(raw: &crate::config::ProxyAliases) -> Self {
    Self::build(raw.pairs())
  }

  fn build<'a>(entries: impl Iterator<Item = (&'a str, &'a str)>) -> Self {
    let mut targets: Vec<(String, String)> = Vec::new();
    for (name, target) in entries {
      let (key, target) = (normalize(name), target.trim().to_string());
      if key.is_empty() || target.is_empty() {
        log::warn!("proxy.aliases: ignoring `{name}` — both the name and the model it points at are required");
        continue;
      }
      // `name@launch` is a client's address for one launch of a model, and an
      // alias is consulted before that split. An alias could not honour the
      // launch half anyway, so a name spelled like an address is refused instead
      // of quietly taking the address over.
      if key.contains('@') {
        log::warn!("proxy.aliases: ignoring `{name}` — an alias names a model, not a `<model>@<launch>` address");
        continue;
      }
      if let Some(at) = targets.iter().position(|(existing, _)| *existing == key) {
        log::warn!("proxy.aliases: `{name}` is listed twice; the later entry is used");
        targets.remove(at);
      }
      targets.push((key, target));
    }
    Self {
      targets,
      warned: RwLock::new(HashSet::new()),
    }
  }

  /// The model reference `requested` stands in for, or `None` when it is not an
  /// alias.
  pub(crate) fn target(&self, requested: &str) -> Option<&str> {
    if self.targets.is_empty() {
      return None;
    }
    let key = normalize(requested);
    self
      .targets
      .iter()
      .find(|(name, _)| *name == key)
      .map(|(_, target)| target.as_str())
  }

  /// Note that `requested` reached a real model even though it is also an alias
  /// name. Returns `true` on the first such report, which is the only one worth
  /// logging.
  pub(crate) fn note_shadowed(&self, requested: &str) -> bool {
    self.note_as_alias(requested, "shadow")
  }

  /// Note that the alias `requested` also reaches a model on its own by partial
  /// match, so adding it moved clients that were already working. Returns `true`
  /// on the first such report.
  pub(crate) fn note_overrides_partial(&self, requested: &str) -> bool {
    self.note_as_alias(requested, "partial")
  }

  /// Note that an alias target matches more than one model, which is a mistake
  /// only the operator can fix: the client sending that name cannot refine it.
  /// Returns `true` on the first such report.
  pub(crate) fn note_ambiguous_target(&self, requested: &str) -> bool {
    self.note_as_alias(requested, "ambiguous")
  }

  /// Note that the alias `requested` points at a reference the catalog has no
  /// model for under its own name, so every request under that alias name is
  /// going to 404. Returns `true` on the first such report.
  pub(crate) fn note_dead_target(&self, requested: &str) -> bool {
    self.note_as_alias(requested, "target")
  }

  /// Note that the alias `requested` points at another alias name that names no
  /// model, which is a chain nobody can follow. Returns `true` on the first such
  /// report.
  pub(crate) fn note_points_at_alias(&self, requested: &str) -> bool {
    self.note_as_alias(requested, "chain")
  }

  /// True when `kind` has not been reported for this alias name yet, without
  /// claiming the report. A caller uses this to skip work that only feeds the
  /// once-per-name line.
  pub(crate) fn report_pending(&self, requested: &str, kind: &str) -> bool {
    match report_key(requested, kind, &self.targets) {
      Some(key) => self
        .warned
        .read()
        .map(|seen| !seen.contains(&key))
        .unwrap_or(false),
      None => false,
    }
  }

  /// Every report is about a configured alias name, and each kind gets its own
  /// prefix so two problems about one name cannot swallow each other.
  fn note_as_alias(&self, requested: &str, kind: &str) -> bool {
    match report_key(requested, kind, &self.targets) {
      Some(key) => self.note_once(key),
      None => false,
    }
  }

  /// First call for this key wins, for the whole daemon.
  fn note_once(&self, key: String) -> bool {
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
    let raw = crate::config::ProxyAliases::from_pairs(
      pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())),
    );
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
  fn a_dead_alias_target_is_reported_once_and_apart_from_a_shadow() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.note_dead_target("some-real-model"), "not an alias name");
    assert!(t.note_dead_target("gpt-4o-mini"));
    assert!(
      !t.note_dead_target("GPT-4o-Mini"),
      "the same name must not warn twice"
    );
    // A name can be shadowed once and its dead target reported once: two
    // different problems, so neither swallows the other.
    assert!(t.note_shadowed("gpt-4o-mini"));
    assert!(!t.note_shadowed("gpt-4o-mini"), "the shadow warns once too");
  }

  #[test]
  fn a_name_spelled_like_a_launch_address_is_refused() {
    // `qwen3@dev` as an alias name would be consulted before the `<model>@<name>`
    // split, so it would take a client's real address away and honour none of it.
    let t = table(&[("qwen3@dev", "somewhere-else")]);
    assert_eq!(t.target("qwen3@dev"), None);
  }

  #[test]
  fn an_alias_pointing_at_another_alias_stays_in_the_table() {
    // The refusal belongs to resolution, not to the table: `hardwired -> y` names
    // no model, but `y` may still be a name a real model answers to, and dropping
    // the entry here would take that away too.
    let t = table(&[("hardwired", "y"), ("y", "realmodel")]);
    assert_eq!(t.target("hardwired"), Some("y"));
    assert_eq!(t.target("y"), Some("realmodel"));
  }

  #[test]
  fn a_chain_is_reported_once_and_apart_from_a_dead_target() {
    let t = table(&[("a", "b")]);
    assert!(t.note_points_at_alias("a"));
    assert!(!t.note_points_at_alias("a"));
    assert!(
      t.note_dead_target("a"),
      "a chain and a dead target are different problems"
    );
  }

  #[test]
  fn an_ambiguous_target_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen")]);
    assert!(t.note_ambiguous_target("gpt-4o-mini"));
    assert!(!t.note_ambiguous_target("gpt-4o-mini"));
    assert!(
      t.note_dead_target("gpt-4o-mini"),
      "an ambiguous target and a dead one are different problems"
    );
  }

  #[test]
  fn every_kind_of_report_survives_an_alias_named_like_another_kind_of_key() {
    // The reports share one set, so they are keyed by kind as well as by name.
    let t = table(&[("target qwen", "alpha"), ("partial qwen", "alpha")]);
    assert!(t.note_dead_target("target qwen"));
    assert!(
      t.note_shadowed("target qwen"),
      "the dead-target report must not spend the shadow one"
    );
    assert!(t.note_overrides_partial("partial qwen"));
    assert!(t.note_shadowed("partial qwen"));
  }

  #[test]
  fn an_alias_overriding_a_working_partial_match_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(
      !t.note_overrides_partial("some-real-model"),
      "not an alias name"
    );
    assert!(t.note_overrides_partial("gpt-4o-mini"));
    assert!(
      !t.note_overrides_partial("GPT-4o-Mini"),
      "the same name must not warn twice"
    );
    assert!(
      t.note_shadowed("gpt-4o-mini"),
      "a third kind of report is not swallowed by this one"
    );
  }

  #[test]
  fn a_report_is_pending_until_it_is_made() {
    // The caller pays a catalog pass to answer the override question, so it asks
    // first whether the line is still unspent.
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.report_pending("some-real-model", REPORT_PARTIAL));
    assert!(t.report_pending("gpt-4o-mini", REPORT_PARTIAL));
    assert!(t.note_overrides_partial("gpt-4o-mini"));
    assert!(
      !t.report_pending("gpt-4o-mini", REPORT_PARTIAL),
      "spent reports are not asked about again"
    );
  }

  #[test]
  fn a_repeated_name_keeps_the_entry_written_last() {
    // Order is the operator's, so the entry they wrote last is the one that
    // answers — for the map spelling as much as for the `name:`/`target:` list.
    let t = table(&[("gpt-4o-mini", "first"), ("gpt-4o-mini", "second")]);
    assert_eq!(t.target("gpt-4o-mini"), Some("second"));
  }

  #[test]
  fn a_pair_table_keeps_file_order() {
    let raw = crate::config::ProxyAliases::from_pairs(vec![
      ("a".to_string(), "first".to_string()),
      ("a".to_string(), "second".to_string()),
      ("B".to_string(), "third".to_string()),
    ]);
    let t = AliasTable::from_config(&raw);
    assert_eq!(t.target("a"), Some("second"));
    assert_eq!(t.target("b"), Some("third"));
  }

  #[test]
  fn an_empty_table_answers_nothing_and_never_warns() {
    let t = AliasTable::from_config(&crate::config::ProxyAliases::default());
    assert_eq!(t.target("anything"), None);
    assert!(!t.note_shadowed("anything"));
  }
}
