//! `RECIPES.md` is config users copy into `config.yaml`, so every block in it
//! has to load the way the page says it does. Each ```yaml block goes through
//! the real loader, and a key, knob, server or preset name that went stale
//! fails here instead of in a user's daemon log.
//!
//! A block that starts indented continues the block before it, the way the
//! page tells the reader to apply it: its indented lines extend the last
//! generic server entry there, and its top-level keys join that block.

use std::collections::BTreeSet;
use std::path::PathBuf;

use llamastash::backend::{Backend, Backends};
use llamastash::config::load_config_from_path;
use llamastash::launch::knobs;
use yaml_serde::{Mapping, Value};

/// Every config block in the page, as `(label, yaml)`, with continuation
/// blocks already folded into the block they extend.
fn recipe_configs() -> Vec<(String, Value)> {
  let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("RECIPES.md");
  let md = std::fs::read_to_string(&path).expect("read RECIPES.md");
  let mut configs: Vec<(String, Value)> = Vec::new();
  let mut heading = "";
  let mut open: Option<(String, String)> = None;
  for (i, line) in md.lines().enumerate() {
    if let Some((_, yaml)) = open.as_mut() {
      if !line.starts_with("```") {
        yaml.push_str(line);
        yaml.push('\n');
        continue;
      }
      let (label, yaml) = open.take().unwrap();
      if yaml.starts_with(' ') {
        let (_, base) = configs
          .last_mut()
          .unwrap_or_else(|| panic!("{label} continues no block"));
        extend(base, &yaml, &label);
      } else {
        let value = yaml_serde::from_str(&yaml).unwrap_or_else(|e| panic!("{label}: {e}"));
        configs.push((label, value));
      }
    } else if line == "```yaml" {
      open = Some((format!("RECIPES.md:{} ({heading})", i + 1), String::new()));
    } else if line.starts_with('#') {
      heading = line.trim_start_matches('#').trim();
    }
  }
  configs
}

/// Folds a continuation block into `base`.
fn extend(base: &mut Value, fragment: &str, label: &str) {
  let lines: Vec<&str> = fragment.lines().collect();
  let split = lines
    .iter()
    .position(|l| !l.is_empty() && !l.starts_with(' '))
    .unwrap_or(lines.len());
  let (head, tail) = lines.split_at(split);
  let indent = head
    .iter()
    .find(|l| !l.trim().is_empty())
    .map_or(0, |l| l.len() - l.trim_start().len());
  let head: String = head
    .iter()
    .map(|l| format!("{}\n", l.get(indent..).unwrap_or("")))
    .collect();
  let head: Value = yaml_serde::from_str(&head).unwrap_or_else(|e| panic!("{label}: {e}"));
  let server = base
    .get_mut("backend")
    .and_then(|b| b.get_mut("generic"))
    .and_then(|g| g.get_mut("servers"))
    .and_then(Value::as_sequence_mut)
    .and_then(|s| s.last_mut())
    .unwrap_or_else(|| panic!("{label} continues a block with no generic server"));
  merge(server, head);
  let tail: Value =
    yaml_serde::from_str(&tail.join("\n")).unwrap_or_else(|e| panic!("{label}: {e}"));
  if !tail.is_null() {
    merge(base, tail);
  }
}

/// Maps merge key by key, lists append, anything else is replaced.
fn merge(dst: &mut Value, src: Value) {
  match (dst, src) {
    (Value::Mapping(dst), Value::Mapping(src)) => {
      for (key, value) in src {
        match dst.get_mut(&key) {
          Some(slot) => merge(slot, value),
          None => {
            dst.insert(key, value);
          }
        }
      }
    }
    (Value::Sequence(dst), Value::Sequence(src)) => dst.extend(src),
    (dst, src) => *dst = src,
  }
}

/// A `server:` pin resolves to a generic entry in the same block, or to a
/// backend id or one of its builds (`<backend>-<build>`), whose names come
/// from probing and can't be checked here.
fn server_exists(server: &str, generic: &BTreeSet<&str>, backends: &BTreeSet<&str>) -> bool {
  if let Some(name) = server.strip_prefix("generic-") {
    return generic.contains(name);
  }
  backends.iter().any(|b| {
    server == *b
      || server
        .strip_prefix(b)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|build| !build.is_empty())
  })
}

#[test]
fn every_recipe_block_loads_as_written() {
  let configs = recipe_configs();
  assert!(
    configs.len() >= 10,
    "found only {} yaml blocks in RECIPES.md",
    configs.len()
  );
  let backends: BTreeSet<&str> = Backends::all().iter().map(|b| b.id()).collect();
  let dir = tempfile::tempdir().expect("temp dir");
  for (i, (label, raw)) in configs.iter().enumerate() {
    let path = dir.path().join(format!("recipe-{i}.yaml"));
    std::fs::write(&path, yaml_serde::to_string(raw).expect("serialize")).expect("write");
    let loaded = load_config_from_path(&path);
    assert_eq!(loaded.warning, None, "{label} does not load");
    assert!(
      loaded.relocated_keys.is_empty(),
      "{label} uses keys that have moved: {:?}",
      loaded.relocated_keys
    );
    let cfg = loaded.config;

    let generic: BTreeSet<&str> = cfg
      .backend
      .generic
      .servers
      .iter()
      .map(|s| s.name.as_str())
      .collect();
    let raw_servers = raw
      .get("backend")
      .and_then(|b| b.get("generic"))
      .and_then(|g| g.get("servers"))
      .and_then(Value::as_sequence)
      .map_or(0, Vec::len);
    assert_eq!(
      generic.len(),
      raw_servers,
      "{label}: a generic server was dropped"
    );

    let raw_presets = raw.get("presets").and_then(Value::as_mapping);
    assert_eq!(
      cfg.presets.len(),
      raw_presets.map_or(0, Mapping::len),
      "{label}: a preset key was dropped"
    );
    for (key, block) in &cfg.presets {
      let raw_entries = raw_presets
        .and_then(|p| p.get(key.as_str()))
        .and_then(|b| b.get("entries"));
      if let Some(default) = &block.default {
        assert!(
          default == "auto" || block.entries.contains_key(default),
          "{label}: `{key}` defaults to `{default}`, which is not one of its entries"
        );
      }
      for (name, body) in &block.entries {
        let at = format!("{label}: preset `{key}` / `{name}`");
        let raw_knobs = raw_entries
          .and_then(|e| e.get(name.as_str()))
          .and_then(|e| e.get("knobs"))
          .and_then(Value::as_mapping)
          .map_or(0, Mapping::len);
        assert_eq!(
          body.knobs.iter().count(),
          raw_knobs,
          "{at}: a knob was dropped on load"
        );
        for (id, _) in body.knobs.iter() {
          assert!(
            knobs::def_for(id).is_some(),
            "{at} sets `{id}`, which nothing declares"
          );
        }
        if let Some(backend) = &body.backend {
          assert!(
            backends.contains(backend.as_str()),
            "{at}: no backend `{backend}`"
          );
        }
        if let Some(server) = &body.server {
          assert!(
            server_exists(server, &generic, &backends),
            "{at}: `server: {server}` names no server"
          );
        }
      }
    }

    // Every alias on the page names a generic entry of its own block.
    if let Some(aliases) = raw
      .get("proxy")
      .and_then(|p| p.get("aliases"))
      .and_then(Value::as_mapping)
    {
      for (alias, target) in aliases {
        let (alias, target) = (
          alias.as_str().unwrap_or_default(),
          target.as_str().unwrap_or_default(),
        );
        assert!(
          generic.contains(target),
          "{label}: alias `{alias}` targets `{target}`, which no entry in the block names"
        );
      }
    }
  }
}
