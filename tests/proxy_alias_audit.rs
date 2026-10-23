//! The read-only `proxy_aliases` surface: what an operator sees of
//! `proxy.aliases` without sending a request that trips it.
//!
//! `list --json`'s `aliases` array is this method's rows, so the states asserted
//! here are what the CLI prints. The listing half — which alias names reach
//! `/v1/models` and `/api/tags` — is pinned in `proxy_models.rs` and
//! `proxy_ollama_compat.rs`.
#![cfg(feature = "test-fixtures")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use llamastash::config::ProxyAliases;
use llamastash::daemon::discovery_task::DiscoveryOptions;
use llamastash::daemon::{run_foreground, DaemonOptions};
use llamastash::discovery::scanner::{ScanOptions, ScanRoot};
use llamastash::discovery::watcher::WatcherOptions;
use llamastash::discovery::ModelSource;
use llamastash::ipc::Client;
use llamastash::test_support::DAEMON_STARTUP_TIMEOUT;
use serde_json::Value;

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-alias", label)
}

fn discovery_for(root: &Path) -> DiscoveryOptions {
  DiscoveryOptions {
    scan_roots: vec![ScanRoot {
      path: root.to_path_buf(),
      source: ModelSource::UserPath,
    }],
    scan: ScanOptions::default(),
    watcher: WatcherOptions {
      debounce: Duration::from_millis(75),
      periodic_rescan: Duration::from_secs(2),
      channel_capacity: 16,
    },
    backend: Default::default(),
    backend_force: Default::default(),
  }
}

async fn wait_for_socket(path: &Path) {
  let deadline = std::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
  loop {
    if std::time::Instant::now() > deadline {
      panic!("daemon never came up: {}", path.display());
    }
    if Client::connect(path).await.is_ok() {
      return;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

/// A daemon with one model on disk and one alias that names it beside one that
/// names nothing.
async fn boot(aliases: &[(&str, &str)]) -> (PathBuf, Client) {
  let state = unique_temp("state");
  let scan = unique_temp("scan");
  fs::write(
    scan.join("qwen3.gguf"),
    llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
  )
  .unwrap();
  let mut opts = DaemonOptions {
    discovery: discovery_for(&scan),
    ..DaemonOptions::rooted_at(state.clone())
  };
  opts.proxy.enabled = false;
  opts.proxy.aliases = ProxyAliases::from_pairs(
    aliases
      .iter()
      .map(|(name, target)| (name.to_string(), target.to_string())),
  );
  let socket = opts.state_dir.clone();
  let _daemon = tokio::spawn(async move { run_foreground(opts).await });
  wait_for_socket(&socket).await;
  let client = Client::connect(&socket).await.expect("connect");
  (state, client)
}

/// The alias states, once the catalog has the row the live alias names: before
/// the first scan lands, every target reads dead, which is what the daemon's own
/// startup report waits past too.
async fn audit(client: &mut Client) -> Vec<(String, String, String)> {
  let deadline = std::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
  loop {
    let body = client
      .call("proxy_aliases", None)
      .await
      .expect("proxy_aliases");
    let rows: Vec<(String, String, String)> = body["aliases"]
      .as_array()
      .expect("aliases array")
      .iter()
      .map(|row| {
        (
          row["name"].as_str().unwrap_or_default().to_string(),
          row["target"].as_str().unwrap_or_default().to_string(),
          row["state"].as_str().unwrap_or_default().to_string(),
        )
      })
      .collect();
    if rows.iter().any(|(_, _, state)| state == "ok") || std::time::Instant::now() > deadline {
      return rows;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
}

/// A live name reads `ok` and a name that points at nothing reads `dead`, in the
/// order the config lists them, so `list --json` is enough to find a typo without
/// waiting for a client to hit it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_alias_reads_dead_and_a_live_one_reads_ok() {
  let (state, mut client) = boot(&[
    ("gpt-4o-mini", "qwen3"),
    ("hardwired-name", "no-such-model"),
  ])
  .await;

  assert_eq!(
    audit(&mut client).await,
    vec![
      (
        "gpt-4o-mini".to_string(),
        "qwen3".to_string(),
        "ok".to_string()
      ),
      (
        "hardwired-name".to_string(),
        "no-such-model".to_string(),
        "dead".to_string()
      ),
    ],
    "one row per entry, in config order, each with its state"
  );

  let _ = client.call("shutdown", None).await;
  let _ = std::fs::remove_dir_all(&state);
}

/// With nothing configured the key is still there and empty, like every other
/// `--json` key, so a consumer never has to tell "no aliases" from "old daemon".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_aliases_configured_answers_an_empty_array() {
  let (state, mut client) = boot(&[]).await;

  let body = client
    .call("proxy_aliases", None)
    .await
    .expect("proxy_aliases");
  assert_eq!(
    body.get("aliases"),
    Some(&Value::Array(Vec::new())),
    "empty, not absent or null: {body}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = std::fs::remove_dir_all(&state);
}

/// A client feature-detects the surface through `capabilities`, which advertises
/// exactly the methods the dispatcher answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_method_is_advertised_in_capabilities() {
  let (state, mut client) = boot(&[("gpt-4o-mini", "qwen3")]).await;

  let body = client
    .call("capabilities", None)
    .await
    .expect("capabilities");
  let methods: Vec<&str> = body["methods"]
    .as_array()
    .expect("methods array")
    .iter()
    .filter_map(Value::as_str)
    .collect();
  assert!(
    methods.contains(&"proxy_aliases"),
    "the audit surface must be discoverable: {methods:?}"
  );

  let _ = client.call("shutdown", None).await;
  let _ = std::fs::remove_dir_all(&state);
}
