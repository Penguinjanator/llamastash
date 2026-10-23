//! Path shapes shared by the proxy's routing decisions and the config that feeds
//! them.

/// A path segment the way a URL parser reads it before removing dot segments:
/// `%2e` in any case is a dot there, so `/a/%2e%2e/b` arrives at an engine as
/// `/b` and has to be read the same way before it is matched against anything.
fn decoded_segment(segment: &str) -> String {
  segment.replace("%2e", ".").replace("%2E", ".")
}

/// Whether a segment is `.` or `..`, percent-encoded spellings included.
pub fn is_dot_segment(segment: &str) -> bool {
  matches!(decoded_segment(segment).as_str(), "." | "..")
}

/// A request path with its dot segments resolved: one form to match a declared
/// prefix on, one to ask the engine for.
pub struct ResolvedPath {
  /// What the prefix match uses: dot segments gone, repeated slashes collapsed, no
  /// trailing slash.
  pub matched: String,
  /// What the engine is asked for: the same resolution with the request's trailing
  /// slash kept, because FastAPI and ComfyUI route `/x/` and `/x` apart.
  pub forwarded: String,
}

/// The resolved form of a request path.
///
/// `None` when nothing is left below the root, which would name the whole
/// service, or when a `..` would climb out of it: what an engine would be asked
/// for then is not the path the client wrote, so the caller refuses rather than
/// matches on a guess.
///
/// Nothing else is decoded. Only the `%2e` spelling is read, to recognize a dot
/// segment; a `%2f` inside a segment stays the escape it was written as. Backslash
/// splits because the URL parser splits it for `http`.
pub fn resolve_path(path: &str) -> Option<ResolvedPath> {
  let mut kept: Vec<&str> = Vec::new();
  for segment in path.split(['/', '\\']) {
    if segment.is_empty() {
      continue;
    }
    if is_dot_segment(segment) {
      if decoded_segment(segment) == ".." && kept.pop().is_none() {
        return None;
      }
      continue;
    }
    kept.push(segment);
  }
  if kept.is_empty() {
    return None;
  }
  let matched = format!("/{}", kept.join("/"));
  let forwarded = match path.ends_with(['/', '\\']) {
    true => format!("{matched}/"),
    false => matched.clone(),
  };
  Some(ResolvedPath { matched, forwarded })
}

/// The matching form of a path, as a declared prefix is stored and compared.
pub fn normalize_path(path: &str) -> Option<String> {
  resolve_path(path).map(|resolved| resolved.matched)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_path_below_the_root_keeps_its_own_shape() {
    assert_eq!(normalize_path("/inference").as_deref(), Some("/inference"));
    assert_eq!(
      normalize_path("//inference/").as_deref(),
      Some("/inference")
    );
    assert_eq!(
      normalize_path("/sdapi/v1/txt2img").as_deref(),
      Some("/sdapi/v1/txt2img")
    );
    assert_eq!(normalize_path("/").as_deref(), None);
    assert_eq!(normalize_path("").as_deref(), None);
  }

  #[test]
  fn dot_segments_resolve_to_the_path_that_would_reach_the_engine() {
    assert_eq!(
      normalize_path("/internal/./x").as_deref(),
      Some("/internal/x")
    );
    assert_eq!(
      normalize_path("/internal/x/../y").as_deref(),
      Some("/internal/y")
    );
    assert_eq!(normalize_path("/internal/..").as_deref(), None);
    // The same path spelled the way a URL parser decodes it.
    assert_eq!(
      normalize_path("/internal/%2e%2e/metrics").as_deref(),
      Some("/metrics")
    );
    assert_eq!(
      normalize_path("/internal/%2E/x").as_deref(),
      Some("/internal/x")
    );
  }

  #[test]
  fn a_backslash_splits_the_way_the_url_parser_reads_one() {
    assert_eq!(
      resolve_path("/internal/x\\..\\..\\metrics")
        .map(|r| r.matched)
        .as_deref(),
      Some("/metrics")
    );
    assert_eq!(
      resolve_path("/internal/x\\..\\metrics")
        .map(|r| r.matched)
        .as_deref(),
      Some("/internal/metrics")
    );
  }

  #[test]
  fn a_trailing_slash_reaches_the_engine_though_matching_drops_it() {
    let bare = resolve_path("/internal").unwrap();
    assert_eq!(
      (bare.matched.as_str(), bare.forwarded.as_str()),
      ("/internal", "/internal")
    );
    let slashed = resolve_path("/internal/").unwrap();
    assert_eq!(
      (slashed.matched.as_str(), slashed.forwarded.as_str()),
      ("/internal", "/internal/")
    );
    let doubled = resolve_path("/internal//").unwrap();
    assert_eq!(
      (doubled.matched.as_str(), doubled.forwarded.as_str()),
      ("/internal", "/internal/")
    );
  }

  #[test]
  fn a_dot_that_climbs_past_the_root_is_not_a_path() {
    assert_eq!(normalize_path("/.."), None);
    assert_eq!(normalize_path("/../metrics"), None);
    // Two levels up from `/internal/x` land on the root, which is a path again.
    assert_eq!(
      normalize_path("/internal/x/../../metrics").as_deref(),
      Some("/metrics")
    );
    assert_eq!(normalize_path("/internal/x/../../../metrics"), None);
  }

  #[test]
  fn only_a_segment_that_is_all_dots_is_a_dot_segment() {
    assert!(!is_dot_segment("a.b"));
    assert!(!is_dot_segment("..hidden"));
    assert!(!is_dot_segment("dot"));
    assert!(is_dot_segment("."));
    assert!(is_dot_segment(".."));
    assert!(is_dot_segment("%2e%2E"));
  }
}
