//! Knob declarations built from `config.yaml` entries.
//!
//! Each entry's knobs become `&'static KnobDef`s in a runtime table keyed by
//! the entry's knob scope (see `crate::launch::knobs::registry::install_scoped`).
//! Strings are leaked once per config load, which is one small table.

use crate::launch::knobs::def::{Concept, Emit, Group, KnobDef, KnobKind, Ring, CTX_LADDER};
use crate::launch::params::LayerLabel;

use super::config::KnobSpec;

fn leak(s: String) -> &'static str {
  Box::leak(s.into_boxed_str())
}

/// The `KnobDef` for one declared knob.
///
/// The `ctx: true` knob is a `U32` context knob, so `--ctx`, the ctx ring, the
/// TUI Context row and `status` ctx all reach it as they reach any backend's
/// context knob. Every other knob is a free-form string the engine validates.
pub fn def_for(spec: &KnobSpec) -> KnobDef {
  let id = leak(spec.knob_id());
  let flag = leak(spec.flag.trim().to_string());
  let label = leak(spec.label.clone().unwrap_or_else(|| id.to_string()));
  let help = leak(match (&spec.help, &spec.default, spec.switch) {
    (Some(h), _, _) => h.clone(),
    (None, Some(d), true) => format!("sends {flag} when on; default {d}"),
    (None, None, true) => format!("sends {flag} when on"),
    (None, Some(d), false) => format!("passed as {flag}; default {d}"),
    (None, None, false) => format!("passed as {flag}; unset sends nothing"),
  });
  let emit = if spec.switch {
    Emit::BareFlagWhenTrue
  } else {
    Emit::FlagValue
  };
  let (kind, concept, group, ring) = if spec.switch {
    (KnobKind::Bool, None, Group::Advanced, Ring::None)
  } else if spec.ctx {
    (
      KnobKind::U32 { max: None },
      Some(Concept::ContextLength),
      Group::Context,
      Ring::Fixed(CTX_LADDER),
    )
  } else {
    (KnobKind::Str, None, Group::Advanced, Ring::None)
  };
  KnobDef {
    id,
    flag: Some(flag),
    concept,
    kind,
    auto: None,
    group,
    label,
    help,
    aliases: &[],
    emit,
    ring,
    volatile: false,
    fallback: LayerLabel::ServerDefault,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ctx_knob_is_a_context_u32_and_the_rest_are_strings() {
    let ctx = def_for(&KnobSpec {
      flag: "--context".into(),
      ctx: true,
      ..KnobSpec::default()
    });
    assert_eq!(ctx.id, "context");
    assert_eq!(ctx.concept, Some(Concept::ContextLength));
    assert!(matches!(ctx.kind, KnobKind::U32 { .. }));
    assert_eq!(ctx.emit_flag(), "--context");

    let temp = def_for(&KnobSpec {
      flag: "-t".into(),
      id: Some("temperature".into()),
      ..KnobSpec::default()
    });
    assert_eq!(temp.id, "temperature");
    assert_eq!(temp.emit_flag(), "-t");
    assert!(matches!(temp.kind, KnobKind::Str));
    assert_eq!(temp.concept, None);
  }
}
