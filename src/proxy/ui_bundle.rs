//! The llama.cpp web UI bundle embedded in the binary, served by `/ui` for
//! engines that ship no web UI of their own.
//!
//! The archive is the prebuilt upstream bundle, pinned in `data/llama-ui/`
//! by `scripts/llama-ui/update.sh`. It is unpacked into an in-memory
//! path-to-bytes map on the first request that needs it, so a daemon that
//! never serves a bundled UI never pays the ~10 MB. Lookups are map keys,
//! never filesystem paths, so there is no traversal surface.
//!
//! The UI also reads one route only its origin engine serves, `/props`,
//! which no other engine answers. [`props`] answers it from what the launch
//! knows.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::OnceLock;

use flate2::read::GzDecoder;
use serde_json::{json, Value};
use tar::Archive;

/// The pinned archive, embedded at compile time.
const ARCHIVE: &[u8] = include_bytes!("../../data/llama-ui/dist.tar.gz");

/// The pinned build and sha, written by `scripts/llama-ui/update.sh`.
const VERSION_FILE: &str = include_str!("../../data/llama-ui/VERSION");

/// The upstream build the bundle came from (`bNNNN`), read out of
/// `VERSION_FILE` so the two cannot drift.
fn pinned_build() -> &'static str {
  VERSION_FILE
    .lines()
    .find_map(|line| line.strip_prefix("build="))
    .filter(|s| !s.is_empty())
    .unwrap_or("unknown")
}

/// What the UI reports as the serving build. It is this binary, not the
/// engine, so the string must not read like an upstream llama-server build.
fn build_info() -> String {
  format!(
    "llamastash {} (bundled llama.cpp UI {})",
    env!("CARGO_PKG_VERSION"),
    pinned_build()
  )
}

/// The unpacked bundle, built on first use and kept for the process's life.
static FILES: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();

fn files() -> &'static HashMap<String, Vec<u8>> {
  FILES.get_or_init(unpack)
}

/// Every regular member of the archive, keyed by its path with the leading
/// `./` removed. The archive is a compile-time input that
/// `scripts/llama-ui/update.sh` verifies by sha256, so a member we cannot
/// read means a broken build, not a bad request.
fn unpack() -> HashMap<String, Vec<u8>> {
  let mut map = HashMap::new();
  let mut archive = Archive::new(GzDecoder::new(ARCHIVE));
  for entry in archive
    .entries()
    .expect("the embedded UI archive must be readable")
  {
    let mut entry = entry.expect("the embedded UI archive must be readable");
    if !entry.header().entry_type().is_file() {
      continue;
    }
    let Ok(path) = entry.path() else { continue };
    let Some(key) = path.to_str().map(normalize_key).filter(|k| !k.is_empty()) else {
      continue;
    };
    let mut buf = Vec::new();
    entry
      .read_to_end(&mut buf)
      .expect("the embedded UI archive must be readable");
    map.insert(key, buf);
  }
  map
}

/// Strip the archive's `./` (and a request's leading `/`) so both sides
/// meet on the same key.
fn normalize_key(path: &str) -> String {
  path
    .strip_prefix("./")
    .or_else(|| path.strip_prefix('/'))
    .unwrap_or(path)
    .to_string()
}

/// The bundle member a request path names. `/` is the index document.
fn member_key(path: &str) -> String {
  let key = normalize_key(path);
  if key.is_empty() {
    "index.html".to_string()
  } else {
    key
  }
}

/// An embedded file by request path (`/`, `/index.html`,
/// `/_app/immutable/bundle.x.js`). The query string is the caller's to drop.
pub(crate) fn get(path: &str) -> Option<&'static [u8]> {
  files().get(&member_key(path)).map(Vec::as_slice)
}

/// Content type by the member's extension. Everything the bundle ships,
/// plus a safe default for anything a future build adds.
pub(crate) fn content_type(path: &str) -> &'static str {
  match member_key(path).rsplit('.').next().unwrap_or_default() {
    "html" => "text/html; charset=utf-8",
    "js" => "text/javascript; charset=utf-8",
    "css" => "text/css; charset=utf-8",
    "json" => "application/json",
    "webmanifest" => "application/manifest+json",
    "svg" => "image/svg+xml",
    "png" => "image/png",
    "ico" => "image/x-icon",
    _ => "application/octet-stream",
  }
}

/// The stand-in `/props` body. The UI reads `role`, `model_path`,
/// `default_generation_settings.n_ctx` and `.params` unguarded; everything
/// else it reads with `?.`, so omitting it leaves the feature off rather
/// than breaking the page.
pub(crate) fn props(served_name: &str, ctx: u32, vision: bool) -> Value {
  json!({
    "role": "model",
    "model_path": served_name,
    "total_slots": 1,
    "build_info": build_info(),
    "default_generation_settings": {
      "n_ctx": ctx,
      "params": {},
    },
    "modalities": {
      "vision": vision,
      "audio": false,
      "video": false,
    },
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The archive really is embedded and unpacks: the root serves the HTML,
  /// and the app bundle is there under a content-hashed name we discover by
  /// scanning keys rather than by hardcoding the hash.
  #[test]
  fn embedded_lookup_serves_index_and_the_app_bundle() {
    let index = get("/").expect("the root path serves index.html");
    let text = std::str::from_utf8(index).unwrap_or_default();
    assert!(
      index.starts_with(b"<!--") && text.contains("<!doctype html>"),
      "the root serves the UI's index document"
    );
    assert_eq!(content_type("/"), "text/html; charset=utf-8");
    assert_eq!(
      get("/index.html").unwrap_or_default().len(),
      index.len(),
      "`/` and `/index.html` are the same member"
    );

    let bundle = files()
      .iter()
      .find(|(key, _)| key.starts_with("_app/immutable/") && key.ends_with(".js"))
      .expect("the bundle ships an immutable app js file");
    assert!(
      !bundle.1.is_empty(),
      "the app bundle must not be an empty member"
    );
    assert_eq!(
      content_type(&format!("/{}", bundle.0)),
      "text/javascript; charset=utf-8"
    );
    assert!(
      get(&format!("/{}", bundle.0)) == Some(bundle.1.as_slice()),
      "the scanned key reads back the same bytes"
    );
  }

  /// A path the bundle does not ship has no file, so `/ui` answers it with a
  /// 404 rather than falling through to the engine.
  #[test]
  fn embedded_lookup_misses_unknown_paths() {
    assert!(get("/nope.js").is_none());
    assert!(get("/slots").is_none());
    assert!(get("/tools").is_none());
  }

  /// The pinned version record is what `build_info` reports, so a UI bump
  /// cannot leave a stale build number in the page.
  #[test]
  fn props_names_this_binary_and_the_pinned_build() {
    let body = props("qwen3", 4096, false);
    let info = body["build_info"].as_str().unwrap_or_default();
    assert!(info.contains(env!("CARGO_PKG_VERSION")), "{info}");
    assert!(info.contains(pinned_build()), "{info}");
    assert_ne!(pinned_build(), "unknown", "VERSION records a build");
  }

  /// The shape the UI reads: single-model role, the launch's served name, the
  /// ctx it resolved, and an empty params object for its parameter sync.
  #[test]
  fn props_reports_ctx_name_and_modalities() {
    let body = props("my-model", 32768, true);
    assert_eq!(body["role"], "model");
    assert_eq!(body["model_path"], "my-model");
    assert_eq!(body["total_slots"], 1);
    assert_eq!(body["default_generation_settings"]["n_ctx"], 32768);
    assert!(
      body["default_generation_settings"]["params"].is_object(),
      "params must be an object: {body}"
    );
    assert_eq!(body["modalities"]["vision"], true);

    // Vision follows the flag; the modalities the launch can't do stay off.
    let no_vision = props("text-only", 2048, false);
    assert_eq!(no_vision["modalities"]["vision"], false);
    assert_eq!(no_vision["modalities"]["audio"], false);
    assert_eq!(no_vision["modalities"]["video"], false);
    assert_eq!(no_vision["default_generation_settings"]["n_ctx"], 2048);
  }
}
