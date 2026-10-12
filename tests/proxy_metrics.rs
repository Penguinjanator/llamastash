//! Integration tests for the Prometheus surface: `GET /metrics` (the proxy's own
//! numbers) and `GET /metrics/backend` (the engines' numbers, merged).
//!
//! Same fixture pattern as `tests/proxy_ui.rs`: `fake_llama_server` children
//! behind the supervisor, a real proxy listener, a hand-rolled HTTP client so the
//! assertions can look at status lines, `Content-Type`, and raw bodies. One fake
//! is started with `--metrics` and one without, which is how a real llama.cpp
//! answers with and without the knob, so the case where an engine refuses to
//! serve metrics is the real thing rather than a stub.
//!
//! Plan: docs/plans/2026-10-11-001-feat-proxy-metrics-plan.md.

#![cfg(feature = "test-fixtures")]

use std::{
  ffi::OsString,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use llamastash::backend::llama_cpp::LlamaCppBackend;
use llamastash::daemon::context::MethodContext;
use llamastash::daemon::probe::ProbeOptions;
use llamastash::daemon::registry::SupervisorRegistry;
use llamastash::daemon::shutdown::ShutdownToken;
use llamastash::daemon::supervisor::{spawn as supervisor_spawn, ManagedSpawn, ManagedState};
use llamastash::discovery::{DiscoveredModel, ModelCatalog, ModelSource};
use llamastash::gguf::identity::ModelId;
use llamastash::launch::mode::LaunchMode;
use llamastash::launch::params::LaunchParams;
use llamastash::proxy::state::ProxyState;
use llamastash::proxy::DEFAULT_BODY_LIMIT_BYTES;
use llamastash::test_support::{
  fake_metadata, header_value, http_get, http_post, shutdown_listener, spawn_listener,
};
use serde_json::Value;
use tokio::time::sleep;

// --- helpers -------------------------------------------------------------

fn unique_temp(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-metrics", label)
}

fn allocate_port() -> u16 {
  llamastash::test_support::allocate_port_range(8).start
}

fn fake_binary() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

fn fast_probe() -> ProbeOptions {
  ProbeOptions {
    interval: Duration::from_millis(30),
    timeout: Duration::from_secs(5),
  }
}

fn discovered(path: &str, display_label: Option<&str>) -> DiscoveredModel {
  let p = PathBuf::from(path);
  DiscoveredModel {
    parent: p.parent().unwrap().to_path_buf(),
    path: p,
    source: ModelSource::UserPath,
    metadata: Some(fake_metadata("llama")),
    parse_error: None,
    split_siblings: Vec::new(),
    display_label: display_label.map(str::to_string),
    multimodal: None,
    supported_backends: Vec::new(),
    mtp_head: None,
  }
}

/// Spawn a `fake_llama_server` through the supervisor and wait for Ready. With
/// `metrics` it passes `--metrics`, the flag real llama.cpp needs to serve its own
/// `/metrics`.
async fn spawn_fake(
  catalog_path: &str,
  log_dir: &Path,
  metrics: bool,
) -> llamastash::daemon::supervisor::ManagedModel {
  spawn_fake_with(
    catalog_path,
    log_dir,
    if metrics { &["--metrics"] } else { &[] },
  )
  .await
}

/// As [`spawn_fake`], with the fixture's own extra flags passed through.
async fn spawn_fake_with(
  catalog_path: &str,
  log_dir: &Path,
  extras: &[&str],
) -> llamastash::daemon::supervisor::ManagedModel {
  let port = allocate_port();
  let id = ModelId {
    path: PathBuf::from(catalog_path),
    header_blake3: [0u8; 32],
  };
  let mut params = LaunchParams::new(PathBuf::from(catalog_path), LaunchMode::Chat);
  if !extras.is_empty() {
    params.extras = extras.iter().map(OsString::from).collect();
  }
  let plan = LlamaCppBackend::new().process_spec(&params, port, fake_binary(), fast_probe());
  let model = supervisor_spawn(ManagedSpawn {
    id,
    params,
    port,
    mode: LaunchMode::Chat,
    log_path: log_dir.join("fake.log"),
    plan,
    origin: llamastash::daemon::supervisor::LaunchOrigin::Manual,
    fit_gate: None,
    resolved_backend: "llamacpp".to_string(),
  })
  .await
  .expect("spawn fake_llama_server");
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  loop {
    let state = model.state().await;
    if matches!(state, ManagedState::Ready) {
      return model;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "fake_llama_server never became Ready: {state:?}"
    );
    sleep(Duration::from_millis(20)).await;
  }
}

async fn build_state(
  models: Vec<DiscoveredModel>,
  supervisors: SupervisorRegistry,
) -> Arc<ProxyState> {
  let catalog = ModelCatalog::new();
  for m in models {
    catalog.upsert(m).await;
  }
  let ctx =
    MethodContext::with_catalog(ShutdownToken::new(), catalog).with_supervisors(supervisors);
  ProxyState::from_context_with_auth(
    &ctx,
    false,
    true,
    None,
    DEFAULT_BODY_LIMIT_BYTES,
    &Default::default(),
  )
}

/// One forwarded chat request, so the request log has a real row behind the
/// numbers a scrape reads.
async fn drive_request(addr: std::net::SocketAddr, model: &str) {
  let body = serde_json::json!({
    "model": model,
    "messages": [{ "role": "user", "content": "hello" }],
    "stream": true,
  })
  .to_string();
  let (status, _headers, response) = http_post(addr, "/v1/chat/completions", &body, &[]).await;
  assert_eq!(status, 200, "request should forward, got: {response:?}");
}

fn text(body: Vec<u8>) -> String {
  String::from_utf8(body).expect("utf8 body")
}

/// The port the JSON view reported for the first live launch.
async fn live_port(addr: std::net::SocketAddr) -> u16 {
  let (_status, _headers, body) = http_get(addr, "/metrics?format=json&launch=1", &[]).await;
  let v: Value = serde_json::from_slice(&body).expect("json");
  v["launches"][0]["port"].as_u64().expect("port") as u16
}

// --- /metrics ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_default_is_prometheus_text_from_the_request_log() {
  let dir = unique_temp("text");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir, true).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  // An empty log still answers: a scraper's first scrape must not be an error.
  let (status, headers, body) = http_get(addr, "/metrics", &[]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("text/plain; version=0.0.4; charset=utf-8"),
    "Prometheus scrapes key off the 0.0.4 media type"
  );
  let empty = text(body);
  assert!(empty.contains("llamastash:models_loaded 1\n"));
  assert!(empty.contains("llamastash:models_discovered 1\n"));
  assert!(empty.contains("llamastash:request_log_rows 0\n"));
  assert!(empty.contains("llamastash:request_log_capacity 1000\n"));
  // Nothing recorded, so no per-model series at all.
  assert!(!empty.contains("llamastash:requests_total"));

  drive_request(addr, "solo").await;
  let (status, _headers, body) = http_get(addr, "/metrics", &[]).await;
  assert_eq!(status, 200);
  let after = text(body);
  for line in [
    "# TYPE llamastash:requests_total counter\n",
    "llamastash:requests_total{model=\"solo\"} 1\n",
    "llamastash:route_requests_total{route=\"/v1/chat/completions\"} 1\n",
    "llamastash:request_errors_total{model=\"solo\"} 0\n",
    "llamastash:requests_in_flight{model=\"solo\"} 0\n",
    "llamastash:auto_starts_total{model=\"solo\"} 0\n",
    "llamastash:evictions_total{model=\"solo\"} 0\n",
    "llamastash:prompt_tokens_total{model=\"solo\"} 0\n",
  ] {
    assert!(after.contains(line), "missing {line:?} in:\n{after}");
  }
  assert!(
    after.contains("# HELP llamastash:requests_total"),
    "each name gets a HELP line:\n{after}"
  );
  // One label set per name: the counter appears exactly once.
  assert_eq!(after.matches("llamastash:requests_total{").count(), 1);
  assert!(after.contains("llamastash:request_duration_seconds_total{model=\"solo\"} "));
  // No `?launch=1`, so the per-launch series stay out.
  assert!(!after.contains("llamastash:launch_"));

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_launch_scrape_adds_the_per_launch_series() {
  let dir = unique_temp("launch");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir, true).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id.clone(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;
  drive_request(addr, "solo").await;

  let (status, _headers, body) = http_get(addr, "/metrics?launch=1", &[]).await;
  assert_eq!(status, 200);
  let out = text(body);
  let id = launch_id.as_str();
  assert!(
    out.contains(&format!(
      "llamastash:launch_info{{launch=\"{id}\",model=\"solo\",port="
    )),
    "launch_info names the launch, its model and its port:\n{out}"
  );
  assert!(out.contains(&format!(
    "llamastash:launch_requests_total{{launch=\"{id}\",model=\"solo\"}} 1"
  )));
  assert!(out.contains(&format!(
    "llamastash:launch_requests_in_flight{{launch=\"{id}\",model=\"solo\"}} 0"
  )));

  // The same launch by port, and the same model by display name, both scope to
  // what was asked for.
  let port = live_port(addr).await;
  for scope in [
    format!("launch={id}"),
    format!("launch={port}"),
    "model=solo".to_string(),
  ] {
    let (status, hdrs, body) = http_get(addr, &format!("/metrics?{scope}"), &[]).await;
    assert_eq!(
      status,
      200,
      "{scope} must be a valid scope, headers {hdrs:?} body {}",
      String::from_utf8_lossy(&body)
    );
    let out = text(body);
    assert!(out.contains("llamastash:requests_total{model=\"solo\"} 1"));
    if scope.starts_with("model=") {
      assert!(
        !out.contains("llamastash:launch_"),
        "a plain model scope adds no per-launch series:\n{out}"
      );
    } else {
      assert!(
        out.contains(&format!("launch_requests_total{{launch=\"{id}\"")),
        "{scope} must carry its own launch series:\n{out}"
      );
    }
  }

  // A reference that names nothing is a 404 that says so.
  let (status, _h, body) = http_get(addr, "/metrics?model=definitely-absent", &[]).await;
  assert_eq!(status, 404);
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["error"]["type"], "not_found");
  assert!(
    v["error"]["message"]
      .as_str()
      .unwrap_or_default()
      .contains("definitely-absent"),
    "the 404 names the value that matched nothing: {v}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_json_shape_follows_the_requests_contract() {
  let dir = unique_temp("json");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir, true).await;
  let launch_id = registry.next_id();
  registry.insert(launch_id.clone(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;
  drive_request(addr, "solo").await;

  let (status, headers, body) = http_get(addr, "/metrics", &[("Accept", "application/json")]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("application/json")
  );
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert!(v["generated_at_ms"].as_u64().unwrap_or(0) > 0);
  assert!(v["scope"]["model"].is_null() && v["scope"]["launch"].is_null());
  assert_eq!(v["summary"]["requests"], 1);
  assert_eq!(v["summary"]["errors"], 0);
  assert_eq!(v["models_loaded"], 1);
  assert_eq!(v["models_discovered"], 1);
  assert_eq!(v["models"].as_array().unwrap().len(), 1);
  assert_eq!(v["models"][0]["model"], "solo");
  assert_eq!(v["models"][0]["path"], "/fixture/solo.gguf");
  assert_eq!(v["models"][0]["launches"], 1);
  assert_eq!(v["models"][0]["requests"], 1);
  assert_eq!(v["requests"].as_array().unwrap().len(), 1);
  // The row keys are the ones `llamastash requests --json` publishes.
  assert_eq!(v["requests"][0]["route"], "/v1/chat/completions");
  assert_eq!(v["requests"][0]["model"], "solo");
  assert_eq!(v["requests"][0]["launch_id"], launch_id.as_str());
  assert_eq!(v["launches"][0]["launch"], launch_id.as_str());
  assert_eq!(v["launches"][0]["model"], "solo");
  assert_eq!(v["launches"][0]["live"], true);
  assert!(v["launches"][0]["port"].as_u64().unwrap() > 0);

  // A model scope names itself in `scope`, and `?limit=` bounds the rows but not
  // the totals.
  let (_status, _h, body) = http_get(addr, "/metrics?format=json&model=solo&limit=0", &[]).await;
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["scope"]["model"], "solo");
  assert_eq!(v["requests"].as_array().unwrap().len(), 0);
  assert_eq!(
    v["summary"]["requests"], 1,
    "totals are not limited by `limit`"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_content_negotiation_matches_the_plan() {
  let dir = unique_temp("negotiate");
  let registry = SupervisorRegistry::new();
  let state = build_state(Vec::new(), registry).await;
  let (addr, shutdown, handle) = spawn_listener(state).await;
  let text_ct = "text/plain; version=0.0.4; charset=utf-8";

  // The Accept table, in both directions.
  let cases: &[(&str, &str)] = &[
    ("", text_ct),
    ("*/*", text_ct),
    ("text/plain", text_ct),
    ("text/plain; version=0.0.4; charset=utf-8", text_ct),
    (
      "application/openmetrics-text; version=1.0.0,text/plain",
      text_ct,
    ),
    ("application/json", "application/json"),
    (
      "application/json;q=0.9, text/plain;q=0.5",
      "application/json",
    ),
    // A tie goes to text, so a scrape can never get JSON by accident.
    ("application/json;q=0.9, text/plain;q=0.9", text_ct),
    // A wildcard of either type is "whatever you have", which is the text
    // default: only an exact `application/json` asks for JSON.
    ("application/*", text_ct),
    ("text/*", text_ct),
  ];
  for (accept, want) in cases {
    let headers: Vec<(&str, &str)> = if accept.is_empty() {
      Vec::new()
    } else {
      vec![("Accept", *accept)]
    };
    let (status, got, body) = http_get(addr, "/metrics", &headers).await;
    assert_eq!(status, 200, "Accept: {accept}");
    assert_eq!(
      header_value(&got, "content-type"),
      Some(*want),
      "Accept: {accept}"
    );
    assert!(!body.is_empty());
  }

  // `?format=` beats the header, in both directions.
  let (status, headers, _) = http_get(addr, "/metrics?format=json", &[("Accept", "*/*")]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("application/json")
  );
  let (status, headers, _) = http_get(
    addr,
    "/metrics?format=prometheus",
    &[("Accept", "application/json")],
  )
  .await;
  assert_eq!(status, 200);
  assert!(header_value(&headers, "content-type")
    .unwrap_or_default()
    .starts_with("text/plain"));

  // An unknown format is a 400 naming the accepted values.
  let (status, _h, body) = http_get(addr, "/metrics?format=yaml", &[]).await;
  assert_eq!(status, 400);
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["error"]["type"], "invalid_request");
  assert!(v["error"]["message"]
    .as_str()
    .unwrap_or_default()
    .contains("prometheus"));

  // A header that matches neither candidate is a 406, in the same envelope.
  let (status, _h, body) = http_get(addr, "/metrics", &[("Accept", "application/xml")]).await;
  assert_eq!(status, 406);
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["error"]["type"], "not_acceptable");
  // ...and `?format=json` overrides it, because a query is unambiguous.
  let (status, _, _) = http_get(
    addr,
    "/metrics?format=json",
    &[("Accept", "application/xml")],
  )
  .await;
  assert_eq!(status, 200);

  // Only GET is routed; a POST is the proxy's usual no-such-route 404.
  let (status, _h, _) = http_post(addr, "/metrics", "{}", &[]).await;
  assert_eq!(status, 404);

  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- /metrics/backend ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_backend_merges_targets_and_isolates_one_that_refuses() {
  let dir = unique_temp("backend");
  let registry = SupervisorRegistry::new();
  let alpha = spawn_fake("/fixture/alpha.gguf", &dir, true).await;
  let alpha_id = registry.next_id();
  registry.insert(alpha_id.clone(), alpha.clone()).await;
  let beta = spawn_fake("/fixture/beta.gguf", &dir, false).await;
  let beta_id = registry.next_id();
  registry.insert(beta_id.clone(), beta.clone()).await;
  let state = build_state(
    vec![
      discovered("/fixture/alpha.gguf", Some("alpha")),
      discovered("/fixture/beta.gguf", Some("beta")),
    ],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;
  let (alpha_id, beta_id) = (alpha_id.as_str(), beta_id.as_str());

  let (status, headers, body) = http_get(addr, "/metrics/backend", &[]).await;
  assert_eq!(
    status, 200,
    "one refusing engine must not fail the whole scrape"
  );
  assert!(header_value(&headers, "content-type")
    .unwrap_or_default()
    .starts_with("text/plain"));
  let out = text(body);
  // The engine's own names come through with `launch`/`model` added and its own
  // labels untouched.
  assert!(
    out.contains(&format!(
      "llamacpp:prompt_tokens_total{{launch=\"{alpha_id}\",model=\"alpha\"}} 21"
    )),
    "engine series must be labelled by launch and model:\n{out}"
  );
  assert!(
    out.contains(&format!(
      "llamacpp:predicted_tokens_total{{model_name=\"fake\",launch=\"{alpha_id}\",model=\"alpha\"}} 7"
    )),
    "the engine's own label stays in front of ours:\n{out}"
  );
  assert_eq!(
    out
      .matches("# TYPE llamacpp:prompt_tokens_total counter")
      .count(),
    1,
    "one HELP/TYPE block per metric name across both targets:\n{out}"
  );
  // Our own per-target gauges tell a scraper which engine answered.
  assert!(out.contains(&format!(
    "llamastash:backend_metrics_available{{launch=\"{alpha_id}\",model=\"alpha\"}} 1"
  )));
  assert!(out.contains(&format!(
    "llamastash:backend_metrics_available{{launch=\"{beta_id}\",model=\"beta\"}} 0"
  )));
  assert!(out.contains(&format!(
    "llamastash:backend_metrics_series{{launch=\"{alpha_id}\",model=\"alpha\"}} 3"
  )));
  assert!(out.contains(&format!(
    "llamastash:backend_metrics_series{{launch=\"{beta_id}\",model=\"beta\"}} 0"
  )));
  assert!(out.contains("llamastash:backend_metrics_scrape_duration_seconds{"));
  assert!(out.contains("llamastash:backend_metrics_skipped_total{"));
  // The proxy's own request numbers are not in this response.
  assert!(!out.contains("llamastash:requests_total"));

  // JSON form: per-target status, so a scraper can tell a refused engine from a
  // missing one.
  let (status, headers, body) =
    http_get(addr, "/metrics/backend?format=json", &[("Accept", "*/*")]).await;
  assert_eq!(status, 200);
  assert_eq!(
    header_value(&headers, "content-type"),
    Some("application/json")
  );
  let v: Value = serde_json::from_slice(&body).expect("json");
  let backends = v["backends"].as_array().unwrap();
  assert_eq!(backends.len(), 2);
  let entry = |want: &str| {
    backends
      .iter()
      .find(|row| row["launch"] == want)
      .cloned()
      .expect("entry for the launch")
  };
  assert_eq!(entry(alpha_id)["status"], 200);
  assert_eq!(entry(alpha_id)["ok"], true);
  assert_eq!(entry(alpha_id)["series"], 3);
  assert_eq!(entry(alpha_id)["model"], "alpha");
  assert_eq!(entry(beta_id)["status"], 501);
  assert_eq!(entry(beta_id)["ok"], false);
  assert!(entry(beta_id)["error"]
    .as_str()
    .unwrap_or_default()
    .contains("501"));
  // The JSON form carries no metric text, only the counts.
  assert!(entry(beta_id).get("text").is_none());
  assert_eq!(entry(beta_id)["series"], 0);

  // Scoping to one model fans out to one target only, and `?launch=` by id does
  // the same.
  for scope in ["model=alpha", &format!("launch={alpha_id}")] {
    let (_status, _h, body) =
      http_get(addr, &format!("/metrics/backend?format=json&{scope}"), &[]).await;
    let v: Value = serde_json::from_slice(&body).expect("json");
    let backends = v["backends"].as_array().unwrap();
    assert_eq!(backends.len(), 1, "{scope} must fan out to one target");
    assert_eq!(backends[0]["launch"], alpha_id);
  }

  // A reference that names nothing is the same 404 as on `/metrics`.
  let (status, _h, body) = http_get(addr, "/metrics/backend?model=definitely-absent", &[]).await;
  assert_eq!(status, 404);
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["error"]["type"], "not_found");

  let _ = alpha.stop(Duration::from_secs(3)).await;
  let _ = beta.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_backend_with_nothing_running_is_an_empty_success() {
  let dir = unique_temp("backend-empty");
  let registry = SupervisorRegistry::new();
  let state = build_state(Vec::new(), registry).await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  // No engines to ask, which is a clean empty scrape rather than an error.
  let (status, _headers, body) = http_get(addr, "/metrics/backend", &[]).await;
  assert_eq!(status, 200);
  assert_eq!(text(body), "");

  let (status, _h, body) = http_get(addr, "/metrics/backend?format=json", &[]).await;
  assert_eq!(status, 200);
  let v: Value = serde_json::from_slice(&body).expect("json");
  assert_eq!(v["backends"].as_array().unwrap().len(), 0);

  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- what a scrape is, and what a scope means -----------------------------

/// The `llamastash:request_log_rows` gauge from a text scrape.
async fn logged_rows(addr: std::net::SocketAddr) -> u64 {
  let (_status, _headers, body) = http_get(addr, "/metrics", &[]).await;
  text(body)
    .lines()
    .find(|line| line.starts_with("llamastash:request_log_rows "))
    .and_then(|line| line.rsplit(' ').next())
    .and_then(|value| value.parse().ok())
    .expect("the row-count gauge is always emitted")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_scrapes_are_not_logged_as_requests() {
  let dir = unique_temp("scrape-unlogged");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/solo.gguf", &dir, true).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/solo.gguf", Some("solo"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  drive_request(addr, "solo").await;
  assert_eq!(logged_rows(addr).await, 1);

  for path in [
    "/metrics",
    "/metrics?format=json",
    "/metrics/backend",
    "/metrics/backend?format=json",
  ] {
    let (status, _headers, _body) = http_get(addr, path, &[]).await;
    assert_eq!(status, 200, "{path}");
  }

  // A scrape reads the numbers; it is not traffic. Otherwise every Prometheus
  // poll would land in the log it reports and bury the real requests.
  assert_eq!(
    logged_rows(addr).await,
    1,
    "four scrapes must not add a row each"
  );
  let (_status, _headers, body) = http_get(addr, "/metrics", &[]).await;
  let out = text(body);
  assert!(
    !out.contains("route=\"/metrics"),
    "the metrics routes are not a forwarded route:\n{out}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_contradictory_scope_is_a_400_naming_both() {
  let dir = unique_temp("scope-conflict");
  let registry = SupervisorRegistry::new();
  let alpha = spawn_fake("/fixture/alpha.gguf", &dir, true).await;
  let alpha_id = registry.next_id();
  registry.insert(alpha_id.clone(), alpha.clone()).await;
  let beta = spawn_fake("/fixture/beta.gguf", &dir, true).await;
  let beta_id = registry.next_id();
  registry.insert(beta_id.clone(), beta.clone()).await;
  let state = build_state(
    vec![
      discovered("/fixture/alpha.gguf", Some("alpha")),
      discovered("/fixture/beta.gguf", Some("beta")),
    ],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  // A `?model=` with a `?launch=` of a different model has no honest answer, and
  // an empty body would read as idle traffic instead of as the mistake it is.
  for route in ["/metrics", "/metrics/backend"] {
    for (model, launch) in [("alpha", beta_id.as_str()), ("beta", alpha_id.as_str())] {
      let scope = format!("model={model}&launch={launch}");
      let (status, _headers, body) = http_get(addr, &format!("{route}?{scope}"), &[]).await;
      assert_eq!(status, 400, "{route}?{scope}: {}", text(body));
      let v: Value = serde_json::from_slice(&body).expect("an error body");
      let message = v["error"]["message"].as_str().unwrap_or_default();
      assert_eq!(v["error"]["type"], "invalid_request");
      assert!(
        message.contains(model) && message.contains(launch),
        "the error has to name both values, got: {message}"
      );
    }
  }

  // A pair that does belong together is still just a scope.
  drive_request(addr, "alpha").await;
  for scope in [
    format!("model=alpha&launch={}", alpha_id.as_str()),
    format!("model=alpha&launch={}", live_port_of(addr, "alpha").await),
  ] {
    let (status, _headers, _body) = http_get(addr, &format!("/metrics?{scope}"), &[]).await;
    assert_eq!(status, 200, "{scope}");
  }

  let _ = alpha.stop(Duration::from_secs(3)).await;
  let _ = beta.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

/// The port the JSON view reported for one launch.
async fn live_port_of(addr: std::net::SocketAddr, model: &str) -> u16 {
  let (_status, _headers, body) = http_get(addr, "/metrics?format=json&launch=1", &[]).await;
  let v: Value = serde_json::from_slice(&body).expect("json");
  v["launches"]
    .as_array()
    .unwrap()
    .iter()
    .find(|row| row["model"] == model)
    .and_then(|row| row["port"].as_u64())
    .expect("a live launch for that model") as u16
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_scope_values_are_percent_decoded() {
  let dir = unique_temp("query-decode");
  let registry = SupervisorRegistry::new();
  let model = spawn_fake("/fixture/cafe.gguf", &dir, true).await;
  registry.insert(registry.next_id(), model.clone()).await;
  let label = "caf\u{e9} +x";
  let state = build_state(
    vec![discovered("/fixture/cafe.gguf", Some(label))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;
  drive_request(addr, label).await;

  // The same display name as a scraper would send it: the accented character as
  // two escapes, the space as `%20`, the literal plus as `%2B`.
  let (status, _headers, body) = http_get(addr, "/metrics?model=caf%C3%A9%20%2Bx", &[]).await;
  assert_eq!(
    status,
    200,
    "an escaped reference has to resolve: {}",
    text(body)
  );
  let out = text(body);
  assert!(
    out.contains("llamastash:requests_total{model=\"caf\u{e9} +x\"} 1"),
    "the label is the decoded name:\n{out}"
  );

  // A bare `+` in a query is a space, so that one names `caf\u{e9} x` and matches
  // nothing: the decoded value is what gets matched, not the raw text.
  let (status, _headers, _body) = http_get(addr, "/metrics?model=caf%C3%A9+x", &[]).await;
  assert_eq!(status, 404);

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

// --- an engine that will not answer ---------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_backend_skips_a_target_that_stops_answering_in_time() {
  let dir = unique_temp("backend-slow");
  let registry = SupervisorRegistry::new();
  // The fan-out gives each engine 2 s; this one answers at 3 s, which is what a
  // wedged or still-loading engine looks like from outside.
  let model = spawn_fake_with(
    "/fixture/slow.gguf",
    &dir,
    &["--metrics", "--metrics-delay-ms", "3000"],
  )
  .await;
  let id = registry.next_id();
  registry.insert(id.clone(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/slow.gguf", Some("slow"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let started = std::time::Instant::now();
  let (status, _headers, body) = http_get(addr, "/metrics/backend?format=json", &[]).await;
  let took = started.elapsed();
  assert_eq!(status, 200, "one wedged engine is not a failed scrape");
  assert!(
    took >= Duration::from_millis(1_500) && took < Duration::from_secs(10),
    "the scrape waits its own patience and then gives up: {took:?}"
  );
  let v: Value = serde_json::from_slice(&body).expect("json");
  let entry = &v["backends"][0];
  assert_eq!(entry["ok"], false);
  assert!(
    entry["error"]
      .as_str()
      .unwrap_or_default()
      .contains("timed out after 2 s"),
    "{entry}"
  );
  assert!(
    entry["status"].is_null(),
    "nothing came back, so there is no status: {entry}"
  );
  assert_eq!(entry["series"], 0);

  let (_status, _headers, body) = http_get(addr, "/metrics/backend", &[]).await;
  let out = text(body);
  assert!(
    !out.contains("llamacpp:"),
    "a target that never answered contributes no series:\n{out}"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_backend_caps_a_target_that_sends_too_much() {
  let dir = unique_temp("backend-huge");
  let registry = SupervisorRegistry::new();
  // 5 MiB of body against a 4 MiB cap: the series are in the kept prefix, so the
  // target still counts, but the read stops at the cap.
  let model = spawn_fake_with(
    "/fixture/huge.gguf",
    &dir,
    &["--metrics", "--metrics-pad", "5242880"],
  )
  .await;
  let id = registry.next_id();
  registry.insert(id.clone(), model.clone()).await;
  let state = build_state(
    vec![discovered("/fixture/huge.gguf", Some("huge"))],
    registry,
  )
  .await;
  let (addr, shutdown, handle) = spawn_listener(state).await;

  let (_status, _headers, body) = http_get(addr, "/metrics/backend?format=json", &[]).await;
  let v: Value = serde_json::from_slice(&body).expect("json");
  let entry = &v["backends"][0];
  assert_eq!(entry["ok"], true, "the prefix it sent is still a scrape");
  assert_eq!(entry["truncated"], true, "{entry}");
  assert_eq!(entry["series"], 3, "{entry}");

  let (_status, _headers, body) = http_get(addr, "/metrics/backend", &[]).await;
  assert!(
    text(body).contains("llamacpp:prompt_tokens_total"),
    "the kept prefix is merged"
  );

  let _ = model.stop(Duration::from_secs(3)).await;
  shutdown_listener(shutdown, handle).await;
  std::fs::remove_dir_all(&dir).ok();
}
