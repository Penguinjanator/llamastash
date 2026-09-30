//! Generic backend: config-declared servers, driven through the production
//! daemon with `fake_llama_server` as the declared binary.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
  let Ok(entries) = std::fs::read_dir(dir) else {
    return;
  };
  for entry in entries.flatten() {
    let path = entry.path();
    if path.is_dir() {
      rust_sources(&path, out);
    } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
      out.push(path);
    }
  }
}

/// "generic" is an ordinary English word in this tree, so the guard looks for
/// the ways code names the backend rather than the bare word: the id literal,
/// the module path, and its types and constants.
#[test]
fn backend_id_does_not_leak_outside_its_module() {
  const ALLOWED: &[&str] = &["src/backend/mod.rs", "src/config/mod.rs"];
  let root = repo_root();
  let mut files = Vec::new();
  rust_sources(&root.join("src"), &mut files);
  let needles = [
    concat!("\"gen", "eric\""),
    concat!("gen", "eric::"),
    concat!("Gen", "ericBackend"),
    concat!("Gen", "ericConfig"),
    concat!("GEN", "ERIC_"),
    concat!("gen", "eric://"),
  ];
  let mut leaks = Vec::new();
  for file in files {
    let rel = file
      .strip_prefix(&root)
      .unwrap_or(&file)
      .to_string_lossy()
      .replace('\\', "/");
    if rel.starts_with(concat!("src/backend/gen", "eric/")) || ALLOWED.contains(&rel.as_str()) {
      continue;
    }
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    if let Some(n) = needles.iter().find(|n| text.contains(**n)) {
      leaks.push(format!("{rel} ({n})"));
    }
  }
  assert!(
    leaks.is_empty(),
    "backend named outside its module: {leaks:?}"
  );
}

#[cfg(feature = "test-fixtures")]
mod lifecycle {
  use std::path::PathBuf;
  use std::time::{Duration, Instant};

  use llamastash::backend::BackendConfig;
  use llamastash::config::ProxyConfig;
  use llamastash::daemon::{run_foreground, DaemonOptions};
  use llamastash::ipc::Client;
  use serde_json::{json, Value};

  fn fake() -> String {
    env!("CARGO_BIN_EXE_fake_llama_server").to_string()
  }

  fn unique_temp(label: &str) -> PathBuf {
    llamastash::test_support::unique_temp_dir("ls-generic", label)
  }

  /// Every entry the tests use. Installed per daemon; entries merge by name,
  /// so parallel tests share one consistent table.
  fn backend_config() -> BackendConfig {
    let yaml = format!(
      r#"
servers:
  - name: gen-a
    binary: {bin}
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv, --print-env, GEN_CTX, --sigterm-exit-delay-ms, "2000"]
    knobs:
      - {{flag: --context, id: gen-ctx, ctx: true, default: "4096"}}
      - {{flag: --speculative, default: mtp}}
      - --seed
    env:
      GEN_CTX: "{{gen-ctx}}"
    ready: /health
    memory_gib: 1.5
    stop_grace_secs: 10
  - name: gen-trap
    binary: {bin}
    args: [--port, "{{port}}", --trap-sigterm]
    ready: /health
    stop_grace_secs: 3
  - name: gen-gguf
    binary: {bin}
    model: "*-served.gguf"
    args: [--port, "{{port}}", -m, "{{name}}", --print-argv, --model-file, "{{model}}"]
    knobs:
      - --gguf-knob
    ready: /health
  - name: gen-chat
    binary: {bin}
    args: [--port, "{{port}}"]
    ready: /health
    modes: [chat]
  - name: gen-slow
    binary: {bin}
    args: [--port, "{{port}}", --health-delay-ms, "60000"]
    ready: /health
    ready_timeout_secs: 2
"#,
      bin = fake()
    );
    BackendConfig {
      generic: yaml_serde::from_str(&yaml).unwrap(),
      ..BackendConfig::default()
    }
  }

  fn opts(state: PathBuf, proxy_port: Option<u16>) -> DaemonOptions {
    let base = DaemonOptions::rooted_at(state);
    DaemonOptions {
      binary: Some(PathBuf::from(fake())),
      port_range: llamastash::test_support::allocate_port_range(8),
      metrics_interval: Duration::from_secs(60),
      backend: backend_config(),
      proxy: ProxyConfig {
        enabled: proxy_port.is_some(),
        port: proxy_port,
        ..ProxyConfig::default()
      },
      ..base
    }
  }

  async fn boot(
    opts: DaemonOptions,
  ) -> (
    Client,
    tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
  ) {
    let dir = opts.state_dir.clone();
    let daemon = tokio::spawn(async move { run_foreground(opts).await });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      if let Ok(c) = Client::connect(&dir).await {
        return (c, daemon);
      }
      assert!(Instant::now() < deadline, "daemon never came up");
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
  }

  async fn start(client: &mut Client, params: Value) -> String {
    let resp = client
      .call("start_model", Some(params))
      .await
      .expect("start_model");
    resp["launch_id"].as_str().expect("launch_id").to_string()
  }

  async fn row(client: &mut Client, launch_id: &str) -> Value {
    let status = client.call("status", None).await.expect("status");
    status["models"]
      .as_array()
      .unwrap()
      .iter()
      .find(|m| m["launch_id"] == launch_id)
      .cloned()
      .unwrap_or(Value::Null)
  }

  fn state_of(row: &Value) -> &str {
    row["state"]["state"].as_str().unwrap_or("")
  }

  async fn wait_state(client: &mut Client, launch_id: &str, want: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
      let r = row(client, launch_id).await;
      if state_of(&r) == want {
        return r;
      }
      assert!(
        Instant::now() < deadline,
        "{launch_id} never reached {want}: {r}"
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  async fn log_lines(client: &mut Client, launch_id: &str, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
      let body = client
        .call(
          "logs_tail",
          Some(json!({"launch_id": launch_id, "lines": 200})),
        )
        .await
        .expect("logs_tail");
      let lines: Vec<String> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str().map(str::to_string))
        .filter(|l| l.contains(needle))
        .collect();
      if !lines.is_empty() || Instant::now() > deadline {
        return lines;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  /// The server catalog fills in the background after boot.
  async fn wait_server(client: &mut Client, server: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let status = client.call("status", None).await.unwrap();
      if status["servers"].to_string().contains(server) {
        return;
      }
      assert!(
        Instant::now() < deadline,
        "never listed as a server: {}",
        status["servers"]
      );
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  async fn stop_timed(client: &mut Client, launch_id: &str, grace: u64) -> Duration {
    let t = Instant::now();
    client
      .call_with_timeout(
        "stop_model",
        Some(json!({"launch_id": launch_id, "grace_secs": grace})),
        Duration::from_secs(30),
      )
      .await
      .expect("stop_model");
    t.elapsed()
  }

  async fn shutdown(
    mut client: Client,
    daemon: tokio::task::JoinHandle<anyhow::Result<llamastash::daemon::StartOutcome>>,
  ) {
    let _ = client.call("shutdown", None).await;
    let _ = tokio::time::timeout(Duration::from_secs(30), daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_is_listed_and_composes_argv_env_and_name() {
    let (mut client, daemon) = boot(opts(unique_temp("compose"), None)).await;

    // Catalog row: file-less, named after the entry, sized by `memory_gib`.
    let deadline = Instant::now() + Duration::from_secs(10);
    let listed = loop {
      let models = client.call("list_models", None).await.unwrap()["models"].clone();
      if let Some(m) = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["path"] == "generic://gen-a")
      {
        break m.clone();
      }
      assert!(Instant::now() < deadline, "entry never listed: {models}");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(listed["name"], "gen-a", "{listed}");
    assert_eq!(listed["source"], "config", "{listed}");
    assert_eq!(listed["backend"], "generic", "{listed}");

    // First, before any `last_params` exist: the recorder stores a launch's
    // params once it sees Ready, so a plain launch after the named one below
    // would race it and could inherit ctx 32768.
    let id2 = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    let r2 = wait_state(&mut client, &id2, "ready").await;
    assert_eq!(r2["params"]["ctx"], 4096, "entry default fills in: {r2}");

    // Named launch, `--ctx`, an entry knob, and raw extras.
    let id = start(
      &mut client,
      json!({
        "model_path": "generic://gen-a",
        "name": "coder",
        "ctx": 32768,
        "knobs": {"speculative": "none"},
        "extras": ["--engine-flag", "x"],
      }),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic");
    assert_eq!(r["params"]["ctx"], 32768, "{r}");
    assert_eq!(r["params"]["knobs"]["speculative"], "none", "{r}");
    assert_eq!(r["params"]["extras"], json!(["--engine-flag", "x"]), "{r}");
    assert_eq!(r["stop_grace_secs"], 10, "{r}");

    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      argv.contains("-m gen-a@coder"),
      "{{name}} is the published address: {argv}"
    );
    assert!(
      argv.contains("--speculative none --engine-flag x"),
      "knobs, then extras: {argv}"
    );
    assert!(
      !argv.contains("--context"),
      "a knob referenced in env is not also a flag: {argv}"
    );
    assert!(!argv.contains("--seed"), "unset, no default: {argv}");
    let env = log_lines(&mut client, &id, "env GEN_CTX=").await;
    assert!(
      env
        .first()
        .is_some_and(|l| l.ends_with("env GEN_CTX=32768")),
      "{env:?}"
    );

    assert_ne!(r["port"], r2["port"], "each launch gets its own port");

    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_launches_on_a_host_without_llama_server() {
    let opts = DaemonOptions {
      binary: None,
      ..opts(unique_temp("no-llama"), None)
    };
    let (mut client, daemon) = boot(opts).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = loop {
      match client
        .call(
          "start_model",
          Some(json!({"model_path": "generic://gen-a"})),
        )
        .await
      {
        Ok(resp) => break resp["launch_id"].as_str().unwrap().to_string(),
        Err(e) => {
          assert!(Instant::now() < deadline, "start_model never accepted: {e}");
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
      }
    };
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_entry_grace_is_a_floor_under_every_stop() {
    let (mut client, daemon) = boot(opts(unique_temp("grace"), None)).await;

    // The fixture's SIGTERM delay and trap are unix-only; Windows has no SIGTERM.
    #[cfg(unix)]
    {
      // Exits 2 s after SIGTERM; a 1 s caller grace must not SIGKILL it.
      let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
      wait_state(&mut client, &id, "ready").await;
      let took = stop_timed(&mut client, &id, 1).await;
      assert!(
        took >= Duration::from_millis(1900) && took < Duration::from_secs(6),
        "clean exit after its own drain, not a 1 s kill: {took:?}"
      );

      // Ignores SIGTERM; the 3 s floor applies, then SIGKILL.
      let id = start(&mut client, json!({"model_path": "generic://gen-trap"})).await;
      wait_state(&mut client, &id, "ready").await;
      let took = stop_timed(&mut client, &id, 1).await;
      assert!(
        took >= Duration::from_millis(2900) && took < Duration::from_secs(6),
        "killed after the entry floor: {took:?}"
      );
    }

    // `shutdown` reports the longest grace it will wait for.
    let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    wait_state(&mut client, &id, "ready").await;
    let resp = client.call("shutdown", None).await.unwrap();
    assert_eq!(resp["stop_grace_secs"], 10, "{resp}");
    let _ = tokio::time::timeout(Duration::from_secs(30), daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn ready_timeout_secs_bounds_the_readiness_wait() {
    let (mut client, daemon) = boot(opts(unique_temp("timeout"), None)).await;
    let t = Instant::now();
    let id = start(&mut client, json!({"model_path": "generic://gen-slow"})).await;
    wait_state(&mut client, &id, "error").await;
    assert!(
      t.elapsed() < Duration::from_secs(15),
      "gave up after the entry's 2 s, not the default probe budget: {:?}",
      t.elapsed()
    );
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_entry_knob_is_remembered_across_a_daemon_restart() {
    let state = unique_temp("restart");
    let (mut client, daemon) = boot(opts(state.clone(), None)).await;
    let id = start(
      &mut client,
      json!({"model_path": "generic://gen-a", "knobs": {"seed": "7"}}),
    )
    .await;
    wait_state(&mut client, &id, "ready").await;
    // last_params is written on Ready by a background recorder.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("\"seed\":\"7\"") {
        break;
      }
      assert!(Instant::now() < deadline, "seed never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown(client, daemon).await;

    let (mut client, daemon) = boot(opts(state, None)).await;
    let id = start(&mut client, json!({"model_path": "generic://gen-a"})).await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["params"]["knobs"]["seed"], "7", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    assert!(argv[0].contains("--seed 7"), "{argv:?}");
    shutdown(client, daemon).await;
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn the_proxy_routes_by_entry_name_and_auto_starts_it() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("proxy"), Some(proxy_port))).await;

    // Wait for the catalog row before asking the proxy for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !client
      .call("list_models", None)
      .await
      .unwrap()
      .to_string()
      .contains("generic://gen-a")
    {
      assert!(Instant::now() < deadline, "entry never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let http = reqwest::Client::new();
    let resp = http
      .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
      .json(&json!({
        "model": "gen-a",
        "messages": [{"role": "user", "content": "hi"}],
      }))
      .timeout(Duration::from_secs(60))
      .send()
      .await
      .expect("proxy request");
    assert!(resp.status().is_success(), "status {}", resp.status());

    let status = client.call("status", None).await.unwrap();
    let running = status["models"].as_array().unwrap();
    assert!(
      running
        .iter()
        .any(|m| m["id"]["path"] == "generic://gen-a" && state_of(m) == "ready"),
      "auto-started the entry: {status}"
    );
    shutdown(client, daemon).await;
  }

  /// An entry that declares `modes: [chat]` is listed as chat on `/v1/models`,
  /// and an embeddings request for it is refused before any launch instead of
  /// failing inside the engine.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_chat_only_entry_refuses_embeddings_and_lists_its_mode() {
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")
      .unwrap()
      .local_addr()
      .unwrap()
      .port();
    let (mut client, daemon) = boot(opts(unique_temp("modes"), Some(proxy_port))).await;
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let row = loop {
      let models: Value = http
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("models")
        .json()
        .await
        .unwrap();
      if let Some(r) = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "gen-chat")
      {
        break r.clone();
      }
      assert!(Instant::now() < deadline, "entry never listed: {models}");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(row["mode"], "chat", "{row}");

    let resp = http
      .post(format!("{base}/v1/embeddings"))
      .json(&json!({"model": "gen-chat", "input": "hi"}))
      .timeout(Duration::from_secs(30))
      .send()
      .await
      .expect("proxy request");
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "unsupported_endpoint", "{body}");

    let status = client.call("status", None).await.unwrap();
    assert!(
      !status["models"].to_string().contains("generic://gen-chat"),
      "refused before launch: {status}"
    );
    shutdown(client, daemon).await;
  }

  /// An entry with `model` is a server option on the catalog rows it matches,
  /// not a row of its own; picking it runs the GGUF through the entry.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_model_entry_runs_a_matching_catalog_gguf() {
    let models = unique_temp("gguf-models");
    for f in ["a-served.gguf", "b-other.gguf"] {
      std::fs::write(
        models.join(f),
        llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
      )
      .unwrap();
    }
    let mut o = opts(unique_temp("gguf"), None);
    o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: models.clone(),
      source: llamastash::discovery::ModelSource::UserPath,
    }];
    let (mut client, daemon) = boot(o).await;

    let served = models.join("a-served.gguf").display().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let rows = loop {
      let rows = client.call("list_models", None).await.unwrap()["models"].clone();
      if rows.to_string().contains("a-served") {
        break rows;
      }
      assert!(Instant::now() < deadline, "models never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let row = |needle: &str| {
      rows
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["path"].as_str().unwrap_or("").contains(needle))
        .cloned()
        .unwrap()
    };
    assert_eq!(
      row("a-served")["supported_backends"],
      json!(["llamacpp", "generic"]),
      "offered after the default"
    );
    assert_eq!(row("b-other")["supported_backends"], json!(["llamacpp"]));
    assert!(
      !rows.to_string().contains("generic://gen-gguf"),
      "no row of its own"
    );
    wait_server(&mut client, "generic-gen-gguf").await;

    let id = start(
      &mut client,
      // The TUI sends the row's default backend beside the server pick; the
      // server decides.
      json!({
        "model_path": served,
        "backend": "llamacpp",
        "server": "generic-gen-gguf",
        "knobs": {"gguf-knob": "v"},
      }),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      argv.contains("-m a-served "),
      "{{name}} is the model id: {argv}"
    );
    assert!(argv.contains(&format!("--model-file {served}")), "{argv}");
    assert!(argv.contains("--gguf-knob v"), "{argv}");
    shutdown(client, daemon).await;
  }

  /// `{name}` is the id `/v1/models` publishes: two same-stem GGUFs publish
  /// qualified ids, and the proxy forwards `body.model` unchanged, so the bare
  /// stem would 404 on an engine that checks it.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn name_placeholder_is_the_published_id_when_stems_collide() {
    let roots = [unique_temp("dup-a"), unique_temp("dup-b")];
    for root in &roots {
      std::fs::write(
        root.join("dup-served.gguf"),
        llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
      )
      .unwrap();
    }
    let mut o = opts(unique_temp("dup"), None);
    o.discovery.scan_roots = roots
      .iter()
      .map(|r| llamastash::discovery::scanner::ScanRoot {
        path: r.clone(),
        source: llamastash::discovery::ModelSource::UserPath,
      })
      .collect();
    let (mut client, daemon) = boot(o).await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
      let rows = client.call("list_models", None).await.unwrap()["models"].clone();
      if rows.to_string().matches("dup-served").count() >= 2 {
        break;
      }
      assert!(Instant::now() < deadline, "models never listed");
      tokio::time::sleep(Duration::from_millis(50)).await;
    }

    wait_server(&mut client, "generic-gen-gguf").await;

    // Sent through a symlink, as macOS's `/tmp` is: the catalog keys the
    // canonical path, and the lookup must still find the published id.
    #[cfg(unix)]
    let sent_root = {
      let link = unique_temp("dup-link").join("root");
      std::os::unix::fs::symlink(&roots[0], &link).unwrap();
      link
    };
    #[cfg(not(unix))]
    let sent_root = roots[0].clone();
    let served = sent_root.join("dup-served.gguf").display().to_string();
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let argv = log_lines(&mut client, &id, "argv ").await;
    let argv = argv.first().expect("argv line");
    assert!(
      !argv.contains("-m dup-served "),
      "the ambiguous stem must not be sent: {argv}"
    );
    assert!(argv.contains("/dup-served "), "qualified id: {argv}");
    shutdown(client, daemon).await;
  }

  /// A server inherited from the last launch must not override an explicit
  /// backend: `start --backend llamacpp` after a generic launch ran generic.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn an_explicit_backend_beats_an_inherited_server() {
    let models = unique_temp("explicit-models");
    std::fs::write(
      models.join("c-served.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .unwrap();
    let mut o = opts(unique_temp("explicit"), None);
    o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
      path: models.clone(),
      source: llamastash::discovery::ModelSource::UserPath,
    }];
    let (mut client, daemon) = boot(o).await;
    wait_server(&mut client, "generic-gen-gguf").await;

    let served = models.join("c-served.gguf").display().to_string();
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "generic", "{r}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("generic-gen-gguf") {
        break;
      }
      assert!(Instant::now() < deadline, "server never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_timed(&mut client, &id, 5).await;

    let id = start(
      &mut client,
      json!({"model_path": served, "backend": "llamacpp"}),
    )
    .await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "llamacpp", "{r}");
    assert!(r["params"]["server"].is_null(), "{r}");
    shutdown(client, daemon).await;
  }

  /// A remembered server that is gone from config must not leave its backend
  /// behind. The launch dropped the missing server but kept the remembered
  /// backend, so a plain `start` ran that backend with no server to pick,
  /// instead of the model's default.
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn a_removed_remembered_server_falls_back_to_the_default_backend() {
    let models = unique_temp("gone-models");
    std::fs::write(
      models.join("d-served.gguf"),
      llamastash::gguf::test_fixtures::build_minimal_gguf("llama"),
    )
    .unwrap();
    let state = unique_temp("gone");
    let opts_at = |state: &PathBuf| {
      let mut o = opts(state.clone(), None);
      o.discovery.scan_roots = vec![llamastash::discovery::scanner::ScanRoot {
        path: models.clone(),
        source: llamastash::discovery::ModelSource::UserPath,
      }];
      o
    };
    let served = models.join("d-served.gguf").display().to_string();

    let (mut client, daemon) = boot(opts_at(&state)).await;
    wait_server(&mut client, "generic-gen-gguf").await;
    let id = start(
      &mut client,
      json!({"model_path": served, "server": "generic-gen-gguf"}),
    )
    .await;
    wait_state(&mut client, &id, "ready").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
      let lp = client.call("last_params_list", None).await.unwrap();
      if lp.to_string().contains("generic-gen-gguf") {
        break;
      }
      assert!(Instant::now() < deadline, "server never persisted: {lp}");
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop_timed(&mut client, &id, 5).await;
    shutdown(client, daemon).await;

    // The entry is removed from config between runs. Entries are installed
    // process-wide, so rename the remembered id instead of dropping the entry.
    let file = state.join("state.json");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
      &file,
      text.replace("generic-gen-gguf", "generic-gen-removed"),
    )
    .unwrap();

    let (mut client, daemon) = boot(opts_at(&state)).await;
    wait_server(&mut client, "generic-gen-gguf").await;
    let id = start(&mut client, json!({"model_path": served})).await;
    let r = wait_state(&mut client, &id, "ready").await;
    assert_eq!(r["backend"], "llamacpp", "{r}");
    assert!(r["params"]["server"].is_null(), "{r}");
    shutdown(client, daemon).await;
  }
}
