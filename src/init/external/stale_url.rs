//! A tool config whose proxy URL names a port the proxy no longer listens on.
//!
//! `init` writes the address the proxy was answering on at patch time. When
//! the proxy later moves — a busy port, `daemon start --proxy-port` /
//! `--host` — every config still names the old one, and nothing but the tool
//! itself notices. This reads those files back and finds the mismatch so
//! [`crate::init::doctor`] can repair it.
//!
//! The repair is textual, not a re-run of a patcher: re-patching would also
//! rewrite the model list to the current defaults, which is a much bigger
//! change than the one broken value. Only the `host:port` authority is
//! rewritten, so the generated `.sh` files, JSON, TOML and YAML configs stay
//! byte-identical apart from the address, and the path each file already
//! carries (`/v1` for the OpenAI-shape tools, none for Claude Code) is kept.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

use super::{resolve_path, tools};

/// The proxy address a running daemon reports, split so a config's `host:port`
/// can be compared against it.
pub struct LiveProxy {
  pub host: String,
  pub port: u16,
}

impl LiveProxy {
  /// Split the base URL
  /// [`crate::init::external::proxy_url::from_running_daemon`] returns
  /// (`http://127.0.0.1:11437/v1`).
  pub fn from_base_url(base_url: &str) -> Option<Self> {
    let url = urls(base_url).into_iter().next()?;
    Some(Self {
      host: url.host,
      port: url.port,
    })
  }

  fn authority(&self) -> String {
    format!("{}:{}", self.host, self.port)
  }
}

/// One `http://host:port` occurrence in a text, as the file writes it.
struct Url {
  host: String,
  port: u16,
  /// What the file carries after the port — `/v1` for the OpenAI-shape
  /// tools, empty for a tool that names no path.
  tail: String,
  /// Byte range the `host:port` occupies in the text.
  authority: Range<usize>,
}

impl Url {
  fn text(&self) -> String {
    self.with_authority(&format!("{}:{}", self.host, self.port))
  }

  /// The same URL with `authority` in place of its own `host:port`.
  fn with_authority(&self, authority: &str) -> String {
    format!("http://{authority}{}", self.tail)
  }
}

/// A URL in a config that names a port other than the live proxy's, with what
/// it becomes once rewritten.
pub struct StaleUrl {
  /// The URL exactly as the file writes it.
  pub found: String,
  /// `found` with the live host and port in place of the stale ones.
  pub rewritten: String,
  /// Byte range of the stale `host:port` — all a repair touches.
  authority: Range<usize>,
}

/// One config file and the stale URLs in it.
pub struct StaleFile {
  pub path: PathBuf,
  pub urls: Vec<StaleUrl>,
}

/// Every stale URL in `text`. A URL is stale when its host is the live
/// proxy's and its port is not; a different host is somebody else's server
/// and a URL naming no port is not an address we could move.
pub fn stale_urls(text: &str, live: &LiveProxy) -> Vec<StaleUrl> {
  urls(text)
    .into_iter()
    .filter(|url| url.host == live.host && url.port != live.port)
    .map(|url| StaleUrl {
      found: url.text(),
      rewritten: url.with_authority(&live.authority()),
      authority: url.authority,
    })
    .collect()
}

/// `text` with every stale authority replaced by the live one. `None` when
/// nothing is stale, so a caller never rewrites a file for nothing.
pub fn rewrite(text: &str, live: &LiveProxy) -> Option<String> {
  let stale = stale_urls(text, live);
  if stale.is_empty() {
    return None;
  }
  let live_authority = live.authority();
  let mut out = String::with_capacity(text.len());
  let mut done = 0;
  for url in &stale {
    out.push_str(&text[done..url.authority.start]);
    out.push_str(&live_authority);
    done = url.authority.end;
  }
  out.push_str(&text[done..]);
  Some(out)
}

/// The `stale → live` pairs of `urls`, one line per file — what a finding
/// about a config says and what the repair ledges against it.
pub fn changes(urls: &[StaleUrl]) -> String {
  urls
    .iter()
    .map(|url| format!("{} \u{2192} {}", url.found, url.rewritten))
    .collect::<Vec<_>>()
    .join(", ")
}

/// Every config `init` writes, filtered to the ones naming a stale port.
pub fn stale_files(live: &LiveProxy) -> Vec<StaleFile> {
  config_files()
    .into_iter()
    .filter_map(|path| {
      // Undecidable rather than stale: a file we cannot read is not one a
      // textual repair may touch.
      let text = std::fs::read_to_string(&path).ok()?;
      let urls = stale_urls(&text, live);
      (!urls.is_empty()).then_some(StaleFile { path, urls })
    })
    .collect()
}

/// The `http://host:port…` occurrences in `text`. The URL ends at a quote,
/// whitespace or `<` — the delimiters the generated `.sh` files and the JSON,
/// TOML and YAML configs use.
fn urls(text: &str) -> Vec<Url> {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| {
    Regex::new(r#"http://([^\s"'<>/?#]+):([0-9]{1,5})([^\s"'<>]*)"#).expect("static pattern")
  });
  re.captures_iter(text)
    .filter_map(|cap| {
      let (Some(host), Some(port_match)) = (cap.get(1), cap.get(2)) else {
        return None;
      };
      let port = port_match.as_str().parse::<u16>().ok()?;
      Some(Url {
        host: host.as_str().to_string(),
        port,
        tail: cap
          .get(3)
          .map(|m| m.as_str().to_string())
          .unwrap_or_default(),
        authority: host.start()..port_match.end(),
      })
    })
    .collect()
}

/// The config file each registered patcher (and its companions) would patch,
/// when it exists on disk.
fn config_files() -> Vec<PathBuf> {
  let mut paths: Vec<PathBuf> = Vec::new();
  for patcher in tools::registered() {
    let companions = patcher.companions();
    let targets = std::iter::once(patcher.as_ref()).chain(companions.iter().map(|c| c.as_ref()));
    for target in targets {
      // Exactly the path `init` would write — its accepted alt when the user
      // has one — so check and repair can never point at different files.
      let Ok(path) = resolve_path(target, None) else {
        continue;
      };
      if path.is_file() && !paths.contains(&path) {
        paths.push(path);
      }
    }
  }
  paths
}

#[cfg(test)]
mod tests {
  use super::*;

  fn live() -> LiveProxy {
    LiveProxy::from_base_url("http://127.0.0.1:11437/v1").expect("live base splits")
  }

  #[test]
  fn the_live_base_splits_into_host_and_port() {
    let live = LiveProxy::from_base_url("http://127.0.0.1:11437/v1").expect("ipv4 base");
    assert_eq!((live.host.as_str(), live.port), ("127.0.0.1", 11437));
    // A wildcard bind arrives as loopback; an IPv6 loopback keeps its brackets.
    let v6 = LiveProxy::from_base_url("http://[::1]:11437/v1").expect("ipv6 base");
    assert_eq!((v6.host.as_str(), v6.port), ("[::1]", 11437));
    assert!(LiveProxy::from_base_url("not a url").is_none());
  }

  #[test]
  fn a_stale_port_is_found_and_the_live_port_is_not() {
    let text = r#"{"baseURL":"http://127.0.0.1:11435/v1"}"#;
    let stale = stale_urls(text, &live());
    assert_eq!(stale.len(), 1, "{text}");
    assert_eq!(stale[0].found, "http://127.0.0.1:11435/v1");
    assert_eq!(stale[0].rewritten, "http://127.0.0.1:11437/v1");
    assert!(stale_urls(r#"{"baseURL":"http://127.0.0.1:11437/v1"}"#, &live()).is_empty());
  }

  #[test]
  fn another_host_on_the_same_port_is_somebody_elses_server() {
    assert!(stale_urls(r#"base_url = "http://localhost:11435/v1""#, &live()).is_empty());
    assert!(stale_urls(r#"base_url = "http://192.168.1.20:11435/v1""#, &live()).is_empty());
    // No port named at all: not an address a repair could move.
    assert!(stale_urls("http://127.0.0.1/v1", &live()).is_empty());
  }

  #[test]
  fn a_url_naming_no_path_keeps_naming_none() {
    // Claude Code's `ANTHROPIC_BASE_URL` carries no `/v1` — the SDK appends it.
    let text = "export ANTHROPIC_BASE_URL=\"http://127.0.0.1:11435\"\n";
    let stale = stale_urls(text, &live());
    assert_eq!(stale.len(), 1, "{text}");
    assert_eq!(stale[0].found, "http://127.0.0.1:11435");
    assert_eq!(stale[0].rewritten, "http://127.0.0.1:11437");
  }

  #[test]
  fn the_rewrite_changes_only_the_stale_authority() {
    let body = "[provider]\nname = \"llamastash\"\nbase_url = \"http://127.0.0.1:11435/v1\"\nmodel = \"qwen3-coder-30b\"\n";
    let out = rewrite(body, &live()).expect("stale url rewritten");
    assert_eq!(
      out,
      "[provider]\nname = \"llamastash\"\nbase_url = \"http://127.0.0.1:11437/v1\"\nmodel = \"qwen3-coder-30b\"\n"
    );

    // Two stale URLs in one file, one live one: all stale ones move, the
    // rest of the bytes stay where they were.
    let both = "OPENAI_API_BASE=http://127.0.0.1:11435/v1\n\
                AIDER_API_BASE=http://127.0.0.1:11436/v1\n\
                KEEP=http://127.0.0.1:11437/v1\n";
    assert_eq!(
      rewrite(both, &live()).expect("both rewritten"),
      "OPENAI_API_BASE=http://127.0.0.1:11437/v1\n\
       AIDER_API_BASE=http://127.0.0.1:11437/v1\n\
       KEEP=http://127.0.0.1:11437/v1\n"
    );
  }

  #[test]
  fn nothing_is_rewritten_when_nothing_is_stale() {
    assert!(rewrite("base_url = \"http://127.0.0.1:11437/v1\"\n", &live()).is_none());
    assert!(rewrite("base_url = \"http://localhost:11435/v1\"\n", &live()).is_none());
    assert!(rewrite("", &live()).is_none());
  }
}
