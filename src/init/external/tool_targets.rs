//! The files an integration writes, in the order `init` writes them.
//!
//! Both the `init` wizard and the `doctor` stale-address check have to answer
//! the same question — which file does this integration actually patch — and
//! they must never disagree: a check that reads a different file than the one
//! `init` wrote repairs nothing and can point at a file the user never asked
//! for. One walk, one order, shared by both callers.

use super::tools;
use super::ToolPatcher;

/// Every registered patcher with its companions inline, in patch order. A
/// companion is one integration's second file (pi.dev's provider block and its
/// model scope), so it sits beside the patcher that pulls it in rather than at
/// the end of the list. Same shape as [`tools::registered`], so a caller can
/// swap one for the other and keep its apply loop.
pub fn tool_targets() -> Vec<Box<dyn ToolPatcher>> {
  let mut out: Vec<Box<dyn ToolPatcher>> = Vec::new();
  for patcher in tools::registered() {
    let companions = patcher.companions();
    for target in std::iter::once(patcher).chain(companions) {
      out.push(target);
    }
  }
  out
}

/// The targets for `ids`, each with its companions, in the order the caller
/// gave them. An id repeats its group, which is what a repeated `--integrations`
/// entry always meant: write that integration again.
pub fn targets_for(ids: &[&str]) -> Vec<Box<dyn ToolPatcher>> {
  let offered: Vec<&str> = tools::registered().iter().map(|p| p.id()).collect();
  let mut out: Vec<Box<dyn ToolPatcher>> = Vec::new();
  for id in ids {
    let mut in_group = false;
    for target in tool_targets() {
      if offered.contains(&target.id()) {
        in_group = target.id() == *id;
      }
      if in_group {
        out.push(target);
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn walked_ids() -> Vec<&'static str> {
    tool_targets().iter().map(|t| t.id()).collect()
  }

  fn registered_ids() -> Vec<&'static str> {
    tools::registered().iter().map(|p| p.id()).collect()
  }

  #[test]
  fn a_companion_sits_beside_the_patcher_that_pulls_it_in() {
    // pi.dev writes two files: the provider block and the model scope that
    // makes it reachable. The scope has to land next to the entry that
    // pulls it in, not at the end of the list, or a caller that applies a
    // range of this walk patches the wrong files.
    let walked = walked_ids();
    let pi = walked
      .iter()
      .position(|id| *id == "pi")
      .expect("pi.dev is registered");
    assert_eq!(walked.get(pi + 1).copied(), Some("pi-settings"));
  }

  #[test]
  fn the_walk_is_at_least_the_registered_list() {
    // Companions only ever add entries; nothing the picker offers drops out.
    let walked = walked_ids();
    let registered = registered_ids();
    assert!(walked.len() >= registered.len());
    for id in &registered {
      assert!(walked.contains(id), "{id} missing from the walk");
    }
  }

  #[test]
  fn a_selection_keeps_the_callers_order_and_brings_its_companions() {
    // The integrations step takes `--integrations a,b` and used to patch in
    // the order the user wrote, companions with their owner. Selecting through
    // the shared walk has to keep that, not fall back to registration order.
    let picked: Vec<&'static str> = targets_for(&["pi", "aider"])
      .iter()
      .map(|t| t.id())
      .collect();
    assert_eq!(picked, vec!["pi", "pi-settings", "aider"]);
    // An id named twice is applied twice: that is what a repeated flag always
    // meant, and nothing here is order-sensitive enough to make it wrong.
    let twice: Vec<&'static str> = targets_for(&["aider", "aider"])
      .iter()
      .map(|t| t.id())
      .collect();
    assert_eq!(twice, vec!["aider", "aider"]);
    assert!(targets_for(&[]).is_empty());
  }

  #[test]
  fn the_offered_entries_keep_the_order_the_picker_shows() {
    // A companion is offered to nobody, so it never takes a slot of its own:
    // strip the companions out of the walk and what is left is exactly
    // `tools::registered()`, in picker order, once each.
    let registered = registered_ids();
    let offered: Vec<&'static str> = walked_ids()
      .into_iter()
      .filter(|id| registered.contains(id))
      .collect();
    assert_eq!(offered, registered);
  }
}
