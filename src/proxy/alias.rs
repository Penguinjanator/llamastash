//! `proxy.aliases`: a name a client is hard-wired to, standing in for a local
//! model. A tool that ships `gpt-4o-mini` in its own config can reach a local
//! model without a config edit. `docs/usage.md` (Model ids on the proxy) is the
//! copy a user reads; this module is what enforces it, together with
//! [`crate::proxy::route::resolve_client_reference`], where the table is
//! consulted.
//!
//! The table is built once at daemon start from `config.yaml` and never changes
//! while the daemon runs, so lookups are read-only. Every problem it can report
//! — a name a real model owns, a value that names no model, a value that is
//! another alias name, an alias that moves a working partial match — is logged
//! once per name through [`AliasTable::note_once`], keyed by kind so two problems
//! about one name cannot swallow each other.

use std::collections::HashSet;
use std::sync::RwLock;

use serde::Serialize;

use crate::launch::resolve::{resolve_model_with_candidates, CatalogRow, ResolveError};

/// What one `proxy.aliases` entry means against the catalog right now, as the
/// operator sees it in `llamastash list --json`.
///
/// Every state but [`AliasState::Ok`] is a name that cannot route, for the one
/// reason [`crate::proxy::router`] refuses it, so the audit, the two listings
/// and the daemon's own warning lines never disagree about what is broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AliasState {
  /// The value names exactly one model, so the alias answers.
  Ok,
  /// A real model owns the name, so the alias never answers and the model does.
  Shadowed,
  /// The value names no model, outright or even as a partial match.
  Dead,
  /// Two or more models answer to the value, outright or once the rungs loosen;
  /// naming one is the operator's call.
  Ambiguous,
  /// The value is another alias name, so the chain reaches no model.
  Chain,
}

/// One audited entry: the client's name, the reference it points at, and why it
/// can or cannot route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct AliasIssue {
  pub(crate) name: String,
  pub(crate) target: String,
  pub(crate) state: AliasState,
}

/// One alias value, judged against the catalog the way a request judges it.
///
/// The ladder is the request's own, in one place: a value that names one model
/// outright, one that several models answer to (outright, or once the rungs
/// loosen), one that is another alias name, or one that reaches nothing. Both the
/// client's error and the operator's audit state come from here, so `list --json`
/// cannot send someone hunting a typo while the client is being handed two
/// candidates for the same entry.
pub(crate) enum TargetVerdict {
  /// The value names exactly one model, and this is the row a request reaches.
  Routes(Box<CatalogRow>),
  /// Two or more models answer to the value; these are the candidates to choose
  /// between.
  Ambiguous(Vec<CatalogRow>),
  /// The value is another alias name, so the chain reaches no model.
  Chain,
  /// Nothing answers to the value: no row at all, or only rows that merely
  /// contain it, which an alias may not answer with.
  Dead,
}

/// Client name → model reference, plus the warnings already logged about it.
///
/// One table per daemon, held behind an `Arc` because `ProxyState` derives
/// `Clone` and every per-daemon field on it is shared rather than copied. The
/// warn set has to be one object process-wide for "one line per problem" to hold,
/// which a by-value field on a cloneable struct would not give.
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

/// One spelling for a stored alias name: model references already resolve
/// case-insensitively, so one config entry has to answer to any client casing.
/// Reads fold case in place against these keys instead of calling this per
/// request.
fn normalize(name: &str) -> String {
  name.trim().to_ascii_lowercase()
}

// The kinds a report can be, and therefore the ways one alias name can be
// reported about without swallowing itself.
const KIND_SHADOW: &str = "shadow";
const KIND_PARTIAL: &str = "partial";
const KIND_AMBIGUOUS: &str = "ambiguous";
const KIND_DEAD: &str = "dead";
const KIND_CHAIN: &str = "chain";

/// The `KIND_PARTIAL` tag, exported so a caller can ask
/// [`AliasTable::report_pending`] before paying for the catalog pass that feeds
/// the report.
pub(crate) const REPORT_PARTIAL: &str = KIND_PARTIAL;

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

  /// Every entry as `(name, model reference)` in the order the file lists them,
  /// for the listings that publish the names. The stored names are already
  /// through [`normalize`], which is the spelling a client's request folds to.
  pub(crate) fn pairs(&self) -> &[(String, String)] {
    &self.targets
  }

  /// The model reference `requested` stands in for, or `None` when it is not an
  /// alias.
  pub(crate) fn target(&self, requested: &str) -> Option<&str> {
    self.lookup(requested).map(|(_, target)| target.as_str())
  }

  /// `true` when no alias is configured, which is the common case and the one
  /// with nothing to audit.
  pub(crate) fn is_empty(&self) -> bool {
    self.targets.is_empty()
  }

  /// Every entry in config order, judged against `cat`. This is the operator's
  /// half of the same decisions the listings and the request path make: a row here
  /// reads not `ok` exactly when `/v1/models` and `/api/tags` leave it out, and it
  /// names the same reason a request under that name would be given. So a value
  /// two models answer to reads `ambiguous` here and in the client's `400`, never
  /// one of each.
  pub(crate) fn audit(&self, cat: &[CatalogRow]) -> Vec<AliasIssue> {
    self
      .targets
      .iter()
      .map(|(name, target)| AliasIssue {
        name: name.clone(),
        target: target.clone(),
        state: self.state_of(cat, name, target),
      })
      .collect()
  }

  /// What one alias value amounts to against `cat`, walked in the order a request
  /// walks it: outright first, then whether the value is another alias name, then
  /// the looser rungs that decide *why* nothing answered. Naming a model outright
  /// is the bar an alias has to clear — a value that only some longer file name
  /// contains would auto-start whichever row happened to contain it, and the
  /// client sending the alias name has no way to refine it — so a loose single
  /// match is `Dead` here, while a loose match on two rows is `Ambiguous`, which
  /// is exactly what that client is told.
  pub(crate) fn verdict(&self, cat: &[CatalogRow], target: &str) -> TargetVerdict {
    match crate::launch::resolve::resolve_exact_reference(cat, target) {
      Ok(row) => return TargetVerdict::Routes(Box::new(row)),
      Err(ResolveError::Many(candidates)) => return TargetVerdict::Ambiguous(candidates),
      Err(ResolveError::None | ResolveError::Empty) => {}
    }
    if self.target(target).is_some() {
      return TargetVerdict::Chain;
    }
    match resolve_model_with_candidates(cat, target) {
      Err(ResolveError::Many(candidates)) => TargetVerdict::Ambiguous(candidates),
      _ => TargetVerdict::Dead,
    }
  }

  /// The one judgement behind [`Self::audit`]: the name first, because a model
  /// that owns it answers instead of the alias, then the value the way a
  /// request would.
  fn state_of(&self, cat: &[CatalogRow], name: &str, target: &str) -> AliasState {
    if crate::launch::resolve::resolve_exact_reference(cat, name).is_ok() {
      return AliasState::Shadowed;
    }
    match self.verdict(cat, target) {
      TargetVerdict::Routes(_) => AliasState::Ok,
      TargetVerdict::Ambiguous(_) => AliasState::Ambiguous,
      TargetVerdict::Chain => AliasState::Chain,
      TargetVerdict::Dead => AliasState::Dead,
    }
  }

  /// Say everything in the table that cannot route, once per name and kind.
  ///
  /// These are the same once-per-name reports a request would spend, so the
  /// startup pass cannot double-log and the first request for a name the pass
  /// already named stays quiet — the operator already has the line.
  pub(crate) fn warn_unroutable(&self, cat: &[CatalogRow]) {
    for issue in self.audit(cat) {
      match issue.state {
        AliasState::Ok => {}
        AliasState::Shadowed => {
          self.warn_shadowed(&issue.name);
        }
        AliasState::Ambiguous => {
          self.warn_ambiguous_target(&issue.name, &issue.target);
        }
        AliasState::Dead => {
          self.warn_dead_target(&issue.name, &issue.target);
        }
        AliasState::Chain => {
          self.warn_points_at_alias(&issue.name, &issue.target);
        }
      }
    }
  }

  /// The entry a client's string means, if it means one at all. Nearly every
  /// request asks and gets "no", so the comparison folds case in place instead of
  /// building a lowercased copy of the name first.
  fn lookup(&self, requested: &str) -> Option<&(String, String)> {
    if self.targets.is_empty() {
      return None;
    }
    let name = requested.trim();
    self
      .targets
      .iter()
      .find(|(key, _)| key.eq_ignore_ascii_case(name))
  }

  /// A real model owns `requested`, so the alias of that name never answers. The
  /// client gets the model it named, which is right, so this is a note about the
  /// config rather than about the request.
  pub(crate) fn warn_shadowed(&self, requested: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_SHADOW);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` names a model that exists, so that model is used and the alias is not"
      );
    }
    first
  }

  /// The alias won, but `requested` also reached `other` on its own - a longer
  /// file name that contains it - so clients that were already being served move
  /// to another model. Worth saying, because nothing in a response shows it.
  pub(crate) fn warn_moved_partial(&self, requested: &str, target: &str, other: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_PARTIAL);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` names `{target}`, but it also matched `{other}` on its own; the alias is used"
      );
    }
    first
  }

  /// Two or more models answer to the alias's value. Naming one is the operator's
  /// call, and the client sending the alias name has no way to settle it.
  pub(crate) fn warn_ambiguous_target(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_AMBIGUOUS);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which matches more than one model; name one of them"
      );
    }
    first
  }

  /// The alias's value names no model, so every request under that name is going
  /// to miss until the config changes.
  pub(crate) fn warn_dead_target(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_DEAD);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which names no model on its own; give a full name or a path"
      );
    }
    first
  }

  /// The alias's value is another alias name, which is a chain that reaches no
  /// model. When the same string is also a real model, that model answered
  /// instead and this was never reached.
  pub(crate) fn warn_points_at_alias(&self, requested: &str, target: &str) -> bool {
    let first = self.note_as_alias(requested, KIND_CHAIN);
    if first {
      log::warn!(
        "proxy.aliases: `{requested}` points at `{target}`, which is another alias name; name the model instead"
      );
    }
    first
  }

  /// True when `kind` has not been reported for this alias name yet, without
  /// claiming the report. A caller uses this to skip work that only feeds the
  /// once-per-name line.
  pub(crate) fn report_pending(&self, requested: &str, kind: &str) -> bool {
    match self.report_key(requested, kind) {
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
    match self.report_key(requested, kind) {
      Some(key) => self.note_once(key),
      None => false,
    }
  }

  /// The warning key for `requested`, when it names a configured alias.
  fn report_key(&self, requested: &str, kind: &str) -> Option<String> {
    self
      .lookup(requested)
      .map(|(key, _)| format!("{kind} {key}"))
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

/// How long the startup audit waits for a catalog to have a row in it, and how
/// often it looks. The first scan runs as the daemon comes up, so this is a
/// ceiling for a slow scan root rather than a typical wait.
const AUDIT_CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
const AUDIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Report the alias table once, as soon as there is a catalog to judge it
/// against.
///
/// At daemon start the catalog is empty and every target would read `dead`, so
/// the pass waits for the first row. It is bounded and it runs once: a scan
/// root that discovers nothing is a valid configuration, and an alias that
/// goes unreported there still gets its line from the first real request.
pub(crate) async fn audit_after_first_scan(ctx: &crate::daemon::context::MethodContext) {
  if ctx.aliases.is_empty() {
    return;
  }
  let deadline = std::time::Instant::now() + AUDIT_CATALOG_WAIT;
  while ctx.catalog.len().await == 0 && std::time::Instant::now() < deadline {
    tokio::time::sleep(AUDIT_POLL).await;
  }
  if ctx.catalog.len().await == 0 {
    return;
  }
  let (_snap, cat) = ctx.catalog.shared_pair().await;
  ctx.aliases.warn_unroutable(&cat);
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
    assert!(
      !t.report_pending("some-real-model", REPORT_PARTIAL),
      "a name that is not an alias has nothing to report"
    );
    assert!(t.warn_shadowed("gpt-4o-mini"));
    assert!(
      !t.warn_shadowed("GPT-4o-MINI"),
      "the same name must not warn twice"
    );
  }

  #[test]
  fn a_dead_alias_target_is_reported_once_and_apart_from_a_shadow() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.report_pending("some-real-model", REPORT_PARTIAL));
    assert!(t.warn_dead_target("gpt-4o-mini", "qwen3.8-27b"));
    assert!(
      !t.warn_dead_target("GPT-4o-Mini", "qwen3.8-27b"),
      "the same name must not warn twice"
    );
    // A name can be shadowed once and its dead target reported once: two
    // different problems, so neither swallows the other.
    assert!(t.warn_shadowed("gpt-4o-mini"));
    assert!(!t.warn_shadowed("gpt-4o-mini"), "the shadow warns once too");
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
    assert!(t.warn_points_at_alias("a", "b"));
    assert!(!t.warn_points_at_alias("a", "b"));
    assert!(
      t.warn_dead_target("a", "b"),
      "a chain and a dead target are different problems"
    );
  }

  #[test]
  fn an_ambiguous_target_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen")]);
    assert!(t.warn_ambiguous_target("gpt-4o-mini", "qwen"));
    assert!(!t.warn_ambiguous_target("gpt-4o-mini", "qwen"));
    assert!(
      t.warn_dead_target("gpt-4o-mini", "qwen"),
      "an ambiguous target and a dead one are different problems"
    );
  }

  #[test]
  fn every_kind_of_report_survives_an_alias_named_like_another_kind_of_key() {
    // The reports share one set, so they are keyed by kind as well as by name.
    let t = table(&[("target qwen", "alpha"), ("partial qwen", "alpha")]);
    assert!(t.warn_dead_target("target qwen", "alpha"));
    assert!(
      t.warn_shadowed("target qwen"),
      "the dead-target report must not spend the shadow one"
    );
    assert!(t.warn_moved_partial("partial qwen", "alpha", "other.gguf"));
    assert!(t.warn_shadowed("partial qwen"));
  }

  #[test]
  fn an_alias_overriding_a_working_partial_match_is_reported_once() {
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(
      !t.warn_moved_partial("some-real-model", "qwen3.8-27b", "other.gguf"),
      "not an alias name"
    );
    assert!(t.warn_moved_partial("gpt-4o-mini", "qwen3.8-27b", "other.gguf"));
    assert!(
      !t.warn_moved_partial("GPT-4o-Mini", "qwen3.8-27b", "other.gguf"),
      "the same name must not warn twice"
    );
    assert!(
      t.warn_shadowed("gpt-4o-mini"),
      "a third kind of report is not swallowed by this one"
    );
  }

  #[test]
  fn an_empty_table_has_nothing_to_report() {
    // The common case: no aliases configured, and every routed request still
    // asks whether a name is one of them.
    let t = table(&[]);
    assert!(!t.report_pending("anything", REPORT_PARTIAL));
    assert!(!t.warn_shadowed("anything"));
    assert!(!t.warn_dead_target("anything", "wherever"));
  }

  #[test]
  fn a_report_is_pending_until_it_is_made() {
    // The caller pays a catalog pass to answer the override question, so it asks
    // first whether the line is still unspent.
    let t = table(&[("gpt-4o-mini", "qwen3.8-27b")]);
    assert!(!t.report_pending("some-real-model", REPORT_PARTIAL));
    assert!(t.report_pending("gpt-4o-mini", REPORT_PARTIAL));
    assert!(t.warn_moved_partial("gpt-4o-mini", "qwen3.8-27b", "other.gguf"));
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
    assert!(!t.warn_shadowed("anything"));
  }

  /// Audit state `target` would be given: `ambiguous` for a value two rows answer
  /// to once the rungs loosen, where the exact rung alone would have said `dead`.
  /// The client sending this alias is handed a candidate list, not a typo hint, so
  /// the two surfaces have to agree.
  #[test]
  fn a_loose_match_two_rows_answer_to_is_ambiguous_not_dead() {
    // `qwen3-it` is no row's name, published id or qualified id, but both file
    // names contain it, so a request under the alias gets `ambiguous_model` with
    // two candidates. A `dead` audit here sends the operator hunting a typo.
    let t = table(&[("gpt-4o-mini", "qwen3-it")]);
    let cat = cat(&["/m/qwen3-it-q4.gguf", "/n/qwen3-it-q8.gguf"]);
    assert_eq!(
      t.audit(&cat)[0].state,
      AliasState::Ambiguous,
      "the audit must name the reason a request gets"
    );
  }

  /// Rows in the shape the catalog hands the resolver: `path` and
  /// `display_label` are all the rungs here read.
  fn cat(paths: &[&str]) -> Vec<CatalogRow> {
    paths
      .iter()
      .map(|p| CatalogRow::for_resolution(p.to_string(), None, None))
      .collect()
  }

  /// One row per entry, in config order, each carrying the reason it can or
  /// cannot route. `/one/a/dup.gguf` and `/two/a/dup.gguf` share the qualified
  /// id `a/dup`, so two models answer to it and only the operator can pick.
  #[test]
  fn audit_gives_every_entry_its_state_in_config_order() {
    let t = table(&[
      ("gpt-4o-mini", "qwen3"),
      ("llama", "nowhere"),
      ("hardwired", "gpt-4o-mini"),
      ("qwen3", "llama"),
      ("dual", "a/dup"),
    ]);
    let audit = t.audit(&cat(&[
      "/m/qwen3.gguf",
      "/one/a/dup.gguf",
      "/two/a/dup.gguf",
    ]));
    let states: Vec<(&str, &str, AliasState)> = audit
      .iter()
      .map(|i| (i.name.as_str(), i.target.as_str(), i.state))
      .collect();
    assert_eq!(
      states,
      vec![
        ("gpt-4o-mini", "qwen3", AliasState::Ok),
        ("llama", "nowhere", AliasState::Dead),
        ("hardwired", "gpt-4o-mini", AliasState::Chain),
        ("qwen3", "llama", AliasState::Shadowed),
        ("dual", "a/dup", AliasState::Ambiguous),
      ],
      "one row per entry, each with the reason it can or cannot route"
    );
  }

  /// An audit only reads, so the once-per-name line a real request still owes
  /// is unspent afterwards. A listing that printed a problem and silently
  /// collected the warning would leave the operator with neither.
  #[test]
  fn an_audit_leaves_every_report_unspent() {
    let t = table(&[
      ("gpt-4o-mini", "qwen3"),
      ("llama", "nowhere"),
      ("hardwired", "gpt-4o-mini"),
    ]);
    let cat = cat(&["/m/qwen3.gguf"]);
    assert_eq!(t.audit(&cat).len(), 3);
    assert!(
      t.warn_dead_target("llama", "nowhere"),
      "the audit must not spend the dead-target line"
    );
    assert!(
      t.warn_points_at_alias("hardwired", "gpt-4o-mini"),
      "nor the chain one"
    );
    assert!(
      t.report_pending("gpt-4o-mini", REPORT_PARTIAL),
      "and a routable name has nothing reported at all"
    );
  }

  /// The startup pass says each unroutable name once, and spends the report a
  /// later request for the same alias would otherwise make: the operator has the
  /// line either way, and a second pass adds nothing.
  #[test]
  fn the_startup_pass_says_each_unroutable_name_once() {
    let t = table(&[
      ("gpt-4o-mini", "qwen3"),
      ("llama", "nowhere"),
      ("hardwired", "gpt-4o-mini"),
    ]);
    let cat = cat(&["/m/qwen3.gguf"]);
    t.warn_unroutable(&cat);
    assert!(
      !t.warn_dead_target("llama", "nowhere"),
      "the pass already said the dead target"
    );
    assert!(
      !t.warn_points_at_alias("hardwired", "gpt-4o-mini"),
      "and the chain"
    );
    t.warn_unroutable(&cat);
    assert!(
      t.report_pending("gpt-4o-mini", REPORT_PARTIAL),
      "a routable alias keeps its own reports"
    );
  }

  fn discovered_at(path: &str) -> crate::discovery::DiscoveredModel {
    let p = std::path::PathBuf::from(path);
    crate::discovery::DiscoveredModel {
      parent: p
        .parent()
        .unwrap_or(std::path::Path::new("/"))
        .to_path_buf(),
      path: p,
      source: crate::discovery::ModelSource::UserPath,
      metadata: Some(crate::test_support::fake_metadata("llama")),
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: Vec::new(),
      mtp_head: None,
    }
  }

  /// A catalog that already has a row is the case the wait exists for: with
  /// nothing to judge against every target would read dead, and with a row the
  /// pass reports straight away.
  #[tokio::test]
  async fn the_startup_pass_reports_once_a_catalog_row_exists() {
    let catalog = crate::discovery::ModelCatalog::new();
    catalog.upsert(discovered_at("/m/qwen3.gguf")).await;
    let ctx = crate::daemon::context::MethodContext::with_catalog(
      crate::daemon::shutdown::ShutdownToken::new(),
      catalog,
    )
    .with_aliases(&crate::config::ProxyAliases::from_pairs(vec![
      ("gpt-4o-mini".to_string(), "qwen3".to_string()),
      ("llama".to_string(), "nowhere".to_string()),
    ]));

    super::audit_after_first_scan(&ctx).await;

    assert!(
      !ctx.aliases.warn_dead_target("llama", "nowhere"),
      "the pass reported the dead alias, so the first request for it stays quiet"
    );
    assert!(
      ctx.aliases.report_pending("gpt-4o-mini", REPORT_PARTIAL),
      "the alias that routes was not reported"
    );
  }
}
