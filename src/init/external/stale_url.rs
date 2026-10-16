//! A tool config whose proxy URL names a port the proxy no longer listens on.
//!
//! `init` writes the address the proxy was answering on at patch time. When
//! the proxy later moves — a busy port, `daemon start --proxy-port` /
//! `--host` — every config still names the old one, and nothing but the tool
//! itself notices. This reads those files back and finds the mismatch so
//! [`crate::init::doctor`] can repair it.
//!
//! Only a port the proxy could itself have bound counts as stale: the
//! configured port and the default of the mode it runs in, each with the scan
//! window the listener probes above it. The scanned files are shared configs —
//! Zed's and OpenCode's settings also carry MCP servers and other local
//! engines — and a URL outside that window belongs to whoever wrote it.
//! Repointing one at llamastash would break it, silently and ledged as a
//! success. In particular default mode never claims `11434`: that port is what
//! llamastash leaves to Ollama on purpose, so an Ollama URL in a shared config
//! is another program's live server, not a moved proxy.
//!
//! The repair is textual, not a re-run of a patcher: re-patching would also
//! rewrite the model list to the current defaults, which is a much bigger
//! change than the one broken value. Only the `host:port` authority is
//! rewritten, so the generated `.sh` files, JSON, TOML and YAML configs stay
//! byte-identical apart from the address, and the path each file already
//! carries (`/v1` for the OpenAI-shape tools, none for Claude Code) is kept.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::config::{Config, ProxyConfig};
use crate::proxy::server::DEFAULT_PORT_SCAN_MAX_OFFSET;

use super::{resolve_path, tools};

/// The proxy address a running daemon reports, plus the ports it could itself
/// have been asked to bind.
pub struct LiveProxy {
  host: String,
  port: u16,
  /// The ports this proxy could have been asked to bind; each one carries
  /// [`DEFAULT_PORT_SCAN_MAX_OFFSET`] ports above it, which is as far as the
  /// listener roams.
  bases: Vec<u16>,
}

impl LiveProxy {
  /// The live proxy from the base URL
  /// [`crate::init::external::proxy_url::from_running_daemon`] returns
  /// (`http://127.0.0.1:11437/v1`) and the bases from [`bind_bases`].
  pub fn from_base_url(base_url: &str, bases: &[u16]) -> Option<Self> {
    let url = urls(base_url).into_iter().next()?;
    Some(Self {
      host: url.host,
      port: url.port,
      bases: bases.to_vec(),
    })
  }

  /// Whether `port` is one this proxy could have bound. Anything else on the
  /// same host is another program's server.
  fn could_bind(&self, port: u16) -> bool {
    self.bases.iter().any(|base| {
      let high = base.saturating_add(DEFAULT_PORT_SCAN_MAX_OFFSET);
      port >= *base && port <= high
    })
  }

  fn authority(&self) -> String {
    format!("{}:{}", self.host, self.port)
  }
}

/// The ports the proxy could have been asked to bind: the configured one and
/// the default of the mode it runs in. Each one owns the scan window above it,
/// so a config written at a port none of them covers is left for its owner to
/// edit — an old one-shot `--proxy-port`, or the other mode's default. Missing
/// those is the safe direction: the tool keeps its own endpoint and fails with
/// a connection error the user can see, and the finding's `init --integrations`
/// hint covers the re-point.
pub fn bind_bases(config: &Config) -> Vec<u16> {
  let mode_default = if config.proxy.ollama_compat {
    ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT
  } else {
    ProxyConfig::DEFAULT_PORT
  };
  let base = config.proxy.effective_port();
  if base == mode_default {
    vec![base]
  } else {
    vec![base, mode_default]
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
#[derive(Debug)]
pub struct StaleUrl {
  /// The URL exactly as the file writes it.
  pub found: String,
  /// `found` with the live host and port in place of the stale ones.
  pub rewritten: String,
  /// Byte range of the stale `host:port` — all a repair touches.
  authority: Range<usize>,
}

/// One scanned config file and what the scan could say about it.
#[derive(Debug)]
pub enum Scan {
  /// The file names proxy addresses the live proxy has moved off.
  Stale { path: PathBuf, urls: Vec<StaleUrl> },
  /// The file exists but is not readable text, so what it names is unknown.
  /// Reporting that beats letting an unreadable config read as a clean one.
  Unreadable { path: PathBuf, reason: String },
}

/// Every stale URL in `text` — the one predicate both the finding and the
/// repair use: the live proxy's host, a port it is not on now, and a port it
/// could have bound.
pub fn stale_urls(text: &str, live: &LiveProxy) -> Vec<StaleUrl> {
  urls(text)
    .into_iter()
    .filter(|url| url.host == live.host && url.port != live.port && live.could_bind(url.port))
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

/// How many `stale → live` pairs one message names before counting the rest.
const CHANGES_SHOWN: usize = 3;

/// The `stale → live` pairs of `urls` — what a finding about a config says and
/// what the repair ledges against it. One settings file can hold many local
/// servers, so the list is capped and the remainder counted.
pub fn changes(urls: &[StaleUrl]) -> String {
  let mut line = urls
    .iter()
    .take(CHANGES_SHOWN)
    .map(|url| format!("{} \u{2192} {}", url.found, url.rewritten))
    .collect::<Vec<_>>()
    .join(", ");
  let rest = urls.len().saturating_sub(CHANGES_SHOWN);
  if rest > 0 {
    line.push_str(&format!(" (+{rest} more)"));
  }
  line
}

/// Every config `init` writes, filtered to the ones that name a stale port or
/// could not be read at all.
pub fn scan(live: &LiveProxy) -> Vec<Scan> {
  config_files()
    .iter()
    .filter_map(|path| scan_file(path, live))
    .collect()
}

/// What can be said about one config file. `None` when it holds no stale URL.
fn scan_file(path: &Path, live: &LiveProxy) -> Option<Scan> {
  match std::fs::read_to_string(path) {
    Ok(text) => {
      let urls = stale_urls(&text, live);
      (!urls.is_empty()).then(|| Scan::Stale {
        path: path.to_path_buf(),
        urls,
      })
    }
    Err(e) => Some(Scan::Unreadable {
      path: path.to_path_buf(),
      reason: e.to_string(),
    }),
  }
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
    // A default install whose proxy roamed to 11437, so the one port it could
    // have been asked to bind is default mode's.
    LiveProxy::from_base_url("http://127.0.0.1:11437/v1", &bind_bases(&Config::default()))
      .expect("live base splits")
  }

  #[test]
  fn the_live_base_splits_into_host_and_port() {
    let live = LiveProxy::from_base_url("http://127.0.0.1:11437/v1", &[11435]).expect("ipv4 base");
    assert_eq!((live.host.as_str(), live.port), ("127.0.0.1", 11437));
    // A wildcard bind arrives as loopback; an IPv6 loopback keeps its brackets.
    let v6 = LiveProxy::from_base_url("http://[::1]:11437/v1", &[11435]).expect("ipv6 base");
    assert_eq!((v6.host.as_str(), v6.port), ("[::1]", 11437));
    assert!(LiveProxy::from_base_url("not a url", &[11435]).is_none());
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
  fn a_port_the_proxy_could_not_have_bound_belongs_to_another_program() {
    // An MCP server on 8080 or a second engine on 12000 is not the proxy
    // having moved, and is never found or rewritten.
    let text = r#"{"mcp":{"api":"http://127.0.0.1:8080/v1"},"other":"http://127.0.0.1:12000/v1"}"#;
    assert!(stale_urls(text, &live()).is_empty(), "{text}");
    // Window edges: default mode scans 11435..=11440, so 11441 and 11434 are
    // somebody else's listener — 11434 is the port llamastash keeps Ollama on
    // purpose, so an Ollama URL there survives every repair.
    assert_eq!(stale_urls("http://127.0.0.1:11440/v1", &live()).len(), 1);
    assert!(stale_urls("http://127.0.0.1:11441/v1", &live()).is_empty());
    assert!(stale_urls("http://127.0.0.1:11434/v1", &live()).is_empty());
    assert!(rewrite("http://127.0.0.1:11434/v1", &live()).is_none());
  }

  #[test]
  fn ollama_keeps_its_port_beside_a_proxy_on_another_base() {
    // The repro: `proxy.port: 11500` in default mode with Ollama live on
    // 11434, both named in one merged config. Ollama's URL is untouched.
    let mut config = Config::default();
    config.proxy.port = Some(11500);
    let proxy =
      LiveProxy::from_base_url("http://127.0.0.1:11501/v1", &bind_bases(&config)).unwrap();
    let text = r#"{"ollama":{"options":{"baseURL":"http://127.0.0.1:11434/v1"}},"stash":{"options":{"baseURL":"http://127.0.0.1:11500/v1"}}}"#;
    let stale = stale_urls(text, &proxy);
    assert_eq!(stale.len(), 1, "{text}");
    assert_eq!(stale[0].found, "http://127.0.0.1:11500/v1");
    assert_eq!(
      rewrite(text, &proxy).expect("the moved one rewritten"),
      r#"{"ollama":{"options":{"baseURL":"http://127.0.0.1:11434/v1"}},"stash":{"options":{"baseURL":"http://127.0.0.1:11501/v1"}}}"#
    );
  }

  #[test]
  fn the_configured_port_widens_the_window_to_wherever_it_is() {
    // `proxy.port: 11500` means a config written there is repairable; a port
    // far from every base is still not.
    let mut config = Config::default();
    config.proxy.port = Some(11500);
    let pinned =
      LiveProxy::from_base_url("http://127.0.0.1:11502/v1", &bind_bases(&config)).unwrap();
    assert_eq!(stale_urls("http://127.0.0.1:11505/v1", &pinned).len(), 1);
    assert!(stale_urls("http://127.0.0.1:11506/v1", &pinned).is_empty());
    assert!(stale_urls("http://127.0.0.1:8080/v1", &pinned).is_empty());
  }

  #[test]
  fn the_bind_bases_are_the_configured_port_and_this_modes_default() {
    // Default mode never reaches Ollama's port, compat mode never reaches the
    // default-mode one: each mode owns a six-port window from its own base.
    assert_eq!(
      bind_bases(&Config::default()),
      vec![ProxyConfig::DEFAULT_PORT]
    );
    let mut compat = Config::default();
    compat.proxy.ollama_compat = true;
    assert_eq!(
      bind_bases(&compat),
      vec![ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT]
    );
    // A pinned port adds this mode's default, because a config written before
    // the pin still names it.
    let mut config = Config::default();
    config.proxy.port = Some(11500);
    assert_eq!(bind_bases(&config), vec![11500, ProxyConfig::DEFAULT_PORT]);
    let mut pinned_compat = Config::default();
    pinned_compat.proxy.port = Some(11500);
    pinned_compat.proxy.ollama_compat = true;
    assert_eq!(
      bind_bases(&pinned_compat),
      vec![11500, ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT]
    );
    // The windows are exactly six ports wide, so Ollama's port sits outside a
    // default-mode window and inside a compat-mode one.
    let default_mode =
      LiveProxy::from_base_url("http://127.0.0.1:11436/v1", &bind_bases(&Config::default()))
        .unwrap();
    assert!(!default_mode.could_bind(ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT));
    let compat_mode =
      LiveProxy::from_base_url("http://127.0.0.1:11437/v1", &bind_bases(&compat)).unwrap();
    assert!(compat_mode.could_bind(ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT));
    assert!(!compat_mode.could_bind(ProxyConfig::DEFAULT_OLLAMA_COMPAT_PORT + 6));
  }

  #[test]
  fn a_config_holding_both_kinds_of_url_rewrites_only_the_stale_one() {
    // The regression: a settings file holding the llamastash provider the
    // proxy moved away from, and the user's own endpoint on a port the proxy
    // could never have bound.
    let text = r#"{"provider":{"llamastash":{"options":{"baseURL":"http://127.0.0.1:11435/v1"}},"mcp":{"options":{"baseURL":"http://127.0.0.1:8080/v1"}}}}"#;
    let stale = stale_urls(text, &live());
    assert_eq!(stale.len(), 1, "{text}");
    assert_eq!(stale[0].found, "http://127.0.0.1:11435/v1");
    let out = rewrite(text, &live()).expect("the stale one rewritten");
    assert_eq!(
      out,
      r#"{"provider":{"llamastash":{"options":{"baseURL":"http://127.0.0.1:11437/v1"}},"mcp":{"options":{"baseURL":"http://127.0.0.1:8080/v1"}}}}"#
    );
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

  #[test]
  fn a_file_that_cannot_be_read_is_reported_not_skipped() {
    let dir = crate::test_support::unique_temp_dir("stale-url", "unreadable");
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("config.yaml");
    // Not UTF-8: whatever address it holds, the repair cannot know.
    std::fs::write(&binary, [0xff_u8, 0xfe, 0x00, 0x90]).unwrap();
    let clean = dir.join("clean.json");
    std::fs::write(&clean, r#"{"baseURL":"http://127.0.0.1:11437/v1"}"#).unwrap();
    let stale = dir.join("stale.json");
    std::fs::write(&stale, r#"{"baseURL":"http://127.0.0.1:11435/v1"}"#).unwrap();

    let reason = match scan_file(&binary, &live()) {
      Some(Scan::Unreadable { path, reason }) => {
        assert_eq!(path, binary);
        reason
      }
      other => panic!("an unreadable config must not vanish: {other:?}"),
    };
    assert!(!reason.is_empty(), "the finding needs a reason to print");
    assert!(
      scan_file(&clean, &live()).is_none(),
      "a live address is not a finding"
    );
    assert!(matches!(
      scan_file(&stale, &live()),
      Some(Scan::Stale { .. })
    ));
    std::fs::remove_dir_all(&dir).ok();
  }

  #[test]
  fn a_long_change_list_is_capped_and_counted() {
    let many = "http://127.0.0.1:11435/v1 http://127.0.0.1:11436/v1 http://127.0.0.1:11438/v1 \
                http://127.0.0.1:11439/v1 http://127.0.0.1:11440/v1";
    let stale = stale_urls(many, &live());
    assert_eq!(stale.len(), 5);
    let line = changes(&stale);
    assert_eq!(
      line,
      "http://127.0.0.1:11435/v1 \u{2192} http://127.0.0.1:11437/v1, \
       http://127.0.0.1:11436/v1 \u{2192} http://127.0.0.1:11437/v1, \
       http://127.0.0.1:11438/v1 \u{2192} http://127.0.0.1:11437/v1 (+2 more)"
    );
    // At the cap nothing is counted, and an empty list stays empty.
    assert!(!changes(&stale[..3]).contains("more"));
    assert_eq!(changes(&[]), "");
  }
}
