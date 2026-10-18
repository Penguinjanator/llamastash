//! Integration coverage for `llamastash session`: the exec'd tool gets the
//! proxy address and the published model id in its environment, the tool's own
//! `--` args arrive unchanged, a dry run prints the documented JSON, and a tool
//! that is not installed is a binary-not-found exit.
//!
//! The daemon runs as its own process against an isolated state dir, the way a
//! user runs it, because `session` replaces the calling process — a test
//! process must not be the one that gets replaced.

#![cfg(all(feature = "test-fixtures", unix))]

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use llamastash::gguf::test_fixtures::FixtureBuilder;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

fn fake_server() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-sess", label)
}

/// A port nothing is holding. The proxy prefers 11435, which the developer's own
/// daemon already has, so every test binds its proxy on a port of its own.
fn free_port() -> u16 {
  let l = TcpListener::bind("127.0.0.1:0").expect("free port");
  let port = l.local_addr().expect("addr").port();
  drop(l);
  port
}

/// A daemon with one chat model in its catalog and the fake engine behind it.
struct Daemon {
  state: PathBuf,
  cache: PathBuf,
  model_dir: PathBuf,
  config: PathBuf,
  proxy_port: u16,
}

impl Daemon {
  /// `proxy_api_key` is written into the config the CLI reads, which is what
  /// decides whether a dry run masks the key.
  fn start(proxy_api_key: Option<&str>) -> Self {
    let state = temp("state");
    let cache = temp("cache");
    let model_dir = temp("models");
    std::fs::write(
      model_dir.join("chat.gguf"),
      FixtureBuilder::new()
        .with_arch("llama")
        .with_chat_template("{% for m in messages %}{{ m.content }}{% endfor %}")
        .build(),
    )
    .expect("write chat fixture model");
    // A GGUF whose mode hint the parser cannot read, which is most of them once
    // they carry no chat template.
    std::fs::write(
      model_dir.join("nohint.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .expect("write hint-less fixture model");
    let proxy_port = free_port();
    // Each daemon needs its own launch range too, or parallel tests both hand
    // their fake engine port 41100 and the second launch dies on bind.
    let launch_start = free_port();
    let config = state.join("config.yaml");
    let mut body = format!(
      "proxy:\n  port: {proxy_port}\ndaemon:\n  port_range:\n    start: {launch_start}\n    end: {}\n",
      launch_start.saturating_add(15)
    );
    if let Some(key) = proxy_api_key {
      body.push_str(&format!("  api_key: \"{key}\"\n"));
    }
    std::fs::write(&config, body).expect("write config");

    let daemon = Self {
      state,
      cache,
      model_dir,
      config,
      proxy_port,
    };
    let out = daemon
      .cmd()
      .args(["daemon", "start", "--proxy-port", &proxy_port.to_string()])
      .output()
      .expect("daemon start");
    assert!(
      out.status.success(),
      "daemon start failed: {}",
      String::from_utf8_lossy(&out.stderr)
    );
    daemon
  }

  /// `llamastash` wired to this daemon: isolated state, cache, and config, and
  /// the fake engine as the launcher's `llama-server`.
  fn cmd(&self) -> Command {
    self.cmd_with_config(&self.config)
  }

  /// The same command against a different `config.yaml`. What a dry run masks
  /// depends on whether the caller resolves a real key, and that is a
  /// caller-side config question, not a daemon one.
  fn cmd_with_config(&self, config: &Path) -> Command {
    let mut cmd = bin();
    cmd
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .env("LLAMASTASH_CACHE_DIR", &self.cache)
      .env("LLAMASTASH_CONFIG_DIR", self.state.join("config-dir"))
      .arg("--config")
      .arg(config)
      .arg("--llama-server")
      .arg(fake_server())
      .arg("--model-path")
      .arg(&self.model_dir);
    cmd
  }

  /// Start the catalog model and wait for it, so `session` has a running launch.
  fn start_model(&self) {
    let out = self
      .cmd()
      .args(["start", "chat", "--wait", "--json"])
      .output()
      .expect("start");
    assert!(
      out.status.success(),
      "start failed ({}): {}{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    );
  }
}

impl Drop for Daemon {
  fn drop(&mut self) {
    // Stops the supervised engine before the dirs go, the same way a user would.
    let _ = bin()
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .arg("--config")
      .arg(&self.config)
      .args(["stop", "--all", "--yes"])
      .output();
    let _ = bin()
      .env("LLAMASTASH_STATE_DIR", &self.state)
      .arg("--config")
      .arg(&self.config)
      .args(["daemon", "stop"])
      .output();
    std::fs::remove_dir_all(&self.state).ok();
    std::fs::remove_dir_all(&self.cache).ok();
    std::fs::remove_dir_all(&self.model_dir).ok();
  }
}

/// A stand-in for a coding tool that records what it was started with.
fn fake_tool(dir: &Path, name: &str, dump: &Path) {
  std::fs::create_dir_all(dir).expect("tool dir");
  std::fs::write(
    dir.join(name),
    format!(
      "#!/bin/sh\n{{ echo \"argv:$*\"; env | grep -E '^(ANTHROPIC|CLAUDE|LLAMASTASH|AIDER|OPENCODE)' | sort; }} > '{}'\n",
      dump.display()
    ),
  )
  .expect("write fake tool");
  let mut perms = std::fs::metadata(dir.join(name))
    .expect("stat")
    .permissions();
  perms.set_mode(0o755);
  std::fs::set_permissions(dir.join(name), perms).expect("chmod");
}

fn path_with(dir: &Path) -> String {
  format!(
    "{}:{}",
    dir.display(),
    std::env::var("PATH").unwrap_or_default()
  )
}

fn stdout_json(out: &std::process::Output) -> Value {
  serde_json::from_slice(&out.stdout).expect("valid json on stdout")
}

#[test]
fn the_executed_tool_gets_the_proxy_its_model_and_the_trailing_args() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let tools = temp("tools");
  std::fs::create_dir_all(&tools).expect("tools dir");
  let dump = tools.join("dump");
  fake_tool(&tools, "claude", &dump);

  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    // The whole point of `env_remove`: a real key in the parent env would
    // compete with the auth token the session sets.
    .env("ANTHROPIC_API_KEY", "must-not-reach-the-tool")
    .args(["session", "claude", "--", "--foo"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  let note = String::from_utf8_lossy(&out.stderr).to_string();
  assert!(note.contains("claude → chat (L1"), "note: {note}");

  let seen = std::fs::read_to_string(&dump).expect("fake tool ran and dumped env");
  let lines: Vec<&str> = seen.lines().collect();
  assert_eq!(
    lines.first().copied(),
    Some("argv:--foo"),
    "the tool's args arrive last and unchanged: {seen}"
  );
  assert!(
    lines.contains(&format!("ANTHROPIC_BASE_URL=http://127.0.0.1:{}", daemon.proxy_port).as_str()),
    "base url: {seen}"
  );
  assert!(lines.contains(&"ANTHROPIC_MODEL=chat"), "model id: {seen}");
  assert!(
    lines.contains(&"ANTHROPIC_SMALL_FAST_MODEL=chat"),
    "small model: {seen}"
  );
  assert!(
    lines.contains(&"ANTHROPIC_AUTH_TOKEN=llamastash"),
    "keyless stub: {seen}"
  );
  assert!(
    !seen.contains("ANTHROPIC_API_KEY"),
    "the removed var must be gone: {seen}"
  );
}

#[test]
fn a_dry_run_prints_the_launch_and_changes_nothing() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let out = daemon
    .cmd()
    .args(["session", "codex", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "dry run failed: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  let body = stdout_json(&out);
  assert_eq!(body["tool"], "codex");
  assert_eq!(body["model"], "chat");
  assert_eq!(body["running"], true);
  assert_eq!(body["launch_id"], "L1");
  assert_eq!(body["program"], "codex");
  assert_eq!(body["env"]["LLAMASTASH_API_KEY"], "llamastash");
  assert_eq!(body["tool_args"], serde_json::json!([]));
  let args = body["args"].as_array().expect("args array");
  assert!(
    args.contains(&Value::String("-c".into())),
    "codex takes -c overrides: {args:?}"
  );
  assert!(
    body["files"].as_array().expect("files array").is_empty(),
    "codex writes no file"
  );
  assert!(body["context_window"].is_number(), "ctx: {body}");

  // Under enforced auth the key is a secret, so the dry run masks it. The
  // daemon is unaffected: a dry run never reads the key from it.
  let keyed = daemon.state.join("keyed.yaml");
  std::fs::write(
    &keyed,
    format!(
      "proxy:\n  port: {}\n  api_key: \"sk-real-key\"\n",
      daemon.proxy_port
    ),
  )
  .expect("write keyed config");
  let masked = daemon
    .cmd_with_config(&keyed)
    .args(["session", "codex", "--dry-run", "--json"])
    .output()
    .expect("run masked dry run");
  let body = stdout_json(&masked);
  assert_eq!(
    body["env"]["LLAMASTASH_API_KEY"], "***",
    "the key must not be printed: {body}"
  );
}

#[test]
fn a_model_that_is_not_running_is_reported_not_started_in_a_dry_run() {
  let daemon = Daemon::start(None);
  let out = daemon
    .cmd()
    .args(["session", "codex", "chat", "--dry-run", "--json"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "dry run failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let body = stdout_json(&out);
  assert_eq!(body["running"], false, "{body}");
  assert_eq!(body["launch_id"], Value::Null, "{body}");
}

#[test]
fn a_model_with_no_mode_hint_starts_as_chat_for_a_session() {
  let daemon = Daemon::start(None);
  let tools = temp("tools");
  let dump = tools.join("dump");
  fake_tool(&tools, "codex", &dump);
  let out = daemon
    .cmd()
    .env("PATH", path_with(&tools))
    .args(["session", "codex", "nohint"])
    .output()
    .expect("run session");
  assert!(
    out.status.success(),
    "session failed: {}{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  // A session can only be chat, so the mode is stated rather than left for the
  // daemon to refuse over.
  let note = String::from_utf8_lossy(&out.stderr).to_string();
  assert!(note.contains("codex → nohint (L1"), "note: {note}");
  assert!(std::fs::read_to_string(&dump).is_ok(), "the tool ran");
}

#[test]
fn a_tool_that_is_not_installed_is_a_binary_not_found() {
  let daemon = Daemon::start(None);
  daemon.start_model();
  let empty = temp("empty-bin");
  std::fs::create_dir_all(&empty).expect("empty dir");
  let out = daemon
    .cmd()
    .env("PATH", &empty)
    .args(["session", "aider"])
    .output()
    .expect("run session");
  assert_eq!(out.status.code(), Some(70), "exit 70 expected");
  assert!(
    String::from_utf8_lossy(&out.stderr)
      .to_string()
      .contains("not found on PATH"),
    "message names the missing program"
  );
}
