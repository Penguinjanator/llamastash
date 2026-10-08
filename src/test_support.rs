//! Shared helpers for tests: the integration suites under `tests/`
//! and the inline `#[cfg(test)]` modules that need the same isolation.
//!
//! Gated behind the `test-fixtures` feature so consumer builds of the
//! library don't carry test-only utilities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Unique temp directory for an integration test.
///
/// macOS `sun_path` is 104 bytes; the default `temp_dir()` already
/// eats ~50 of those, so we trim the time-based suffix and add a
/// process-local atomic counter. Two tests running on the same
/// millisecond used to share a directory (and a daemon, and a
/// runtime.json), which surfaced as periodic Connect-error flakes in
/// the chat smoke tests. `prefix` should be 2-5 chars.
pub fn unique_temp_dir(prefix: &str, label: &str) -> PathBuf {
  static SEQ: AtomicU64 = AtomicU64::new(0);
  let seq = SEQ.fetch_add(1, Ordering::Relaxed);
  let suffix = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .expect("clock")
    .as_millis()
    % 0xFFFF_FFFF;
  let dir = std::env::temp_dir().join(format!(
    "{prefix}-{label}-{}-{suffix:x}-{seq:x}",
    std::process::id()
  ));
  std::fs::create_dir_all(&dir).expect("temp dir creation");
  dir
}

/// A launch-pool port range for a test daemon.
///
/// Probes a batch of ephemeral ports at once and spans the lowest to the
/// highest. The batch is the point: an ephemeral port is only ours until the
/// probe listener drops, and the daemon does not bind it until several
/// milliseconds later, so under a 40-way parallel test run another process
/// routinely takes it in between. A range sized to exactly one port has
/// nowhere to fall back and the launch dies with "no free port in N-N";
/// a spread gives `ports::allocate` (which walks the range linearly) somewhere
/// to land.
pub fn allocate_port_range(probes: usize) -> crate::config::loader::PortRange {
  let listeners: Vec<_> = (0..probes.max(1))
    .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral"))
    .collect();
  let mut ports: Vec<u16> = listeners
    .iter()
    .map(|l| l.local_addr().expect("local_addr").port())
    .collect();
  ports.sort_unstable();
  drop(listeners);
  crate::config::loader::PortRange {
    start: ports[0],
    end: ports[ports.len() - 1],
  }
}

/// Best-effort **synchronous** daemon shutdown, for test `Drop` guards (Drop
/// runs during unwind and can't drive an async client). Hand-rolls an
/// HTTP/1.0 `POST /rpc` carrying the JSON-RPC `shutdown` envelope against the
/// URL + token recorded in `runtime.json`. That trips the daemon's shutdown
/// token, so `run_foreground` runs its `stop_all_managed` step — which is
/// where every `setsid`-detached supervised child (`fake_llama_server`) gets
/// SIGTERM/SIGKILLed. Without it those children become init-owned orphans, the
/// historical source of leaked test fixtures. No-op when `runtime.json` is
/// absent (daemon already gone).
pub fn sync_shutdown_daemon(state_dir: &std::path::Path) -> std::io::Result<()> {
  use std::io::{Read, Write};
  use std::net::TcpStream;
  use std::time::Duration;
  let info = match crate::daemon::runtime_file::load(state_dir) {
    Ok(Some(i)) => i,
    _ => return Ok(()),
  };
  // The daemon binds loopback only, so the URL is always `http://127.0.0.1:<port>`.
  let host_port = info
    .ipc_url
    .strip_prefix("http://")
    .unwrap_or(info.ipc_url.as_str());
  let mut stream = TcpStream::connect(host_port)?;
  stream.set_write_timeout(Some(Duration::from_secs(1)))?;
  stream.set_read_timeout(Some(Duration::from_secs(1)))?;
  let body = br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#;
  let req = format!(
    "POST /rpc HTTP/1.0\r\n\
     Host: {host_port}\r\n\
     Authorization: Bearer {token}\r\n\
     Content-Type: application/json\r\n\
     Content-Length: {len}\r\n\
     Connection: close\r\n\r\n",
    token = info.ipc_token,
    len = body.len(),
  );
  stream.write_all(req.as_bytes())?;
  stream.write_all(body)?;
  // Drain the response so the daemon's writer doesn't block on a full peer
  // buffer; the content doesn't matter — only that the token was tripped.
  let mut sink = [0u8; 512];
  let _ = stream.read(&mut sink);
  Ok(())
}

/// A [`RunningSnapshot`](crate::daemon::state_store::RunningSnapshot) for
/// tests, with every field pre-filled and one setter per field a test
/// actually varies.
///
/// `RunningSnapshot` is constructed in a dozen inline test modules and four
/// integration suites. Every field added to it used to mean editing each of
/// those literals; here it means one default in one place.
///
/// ```ignore
/// let row = running_row("/m/a.gguf").name("coder").launch_id("L2").build();
/// ```
pub struct RunningRow(crate::daemon::state_store::RunningSnapshot);

/// Start a [`RunningRow`] for a GGUF at `path`: launch `L1` on port 41100,
/// unnamed, chat mode, on the default backend.
pub fn running_row(path: &str) -> RunningRow {
  use crate::daemon::registry::LaunchId;
  use crate::daemon::state_store::RunningSnapshot;
  use crate::launch::mode::LaunchMode;
  use crate::launch::params::LaunchParams;
  RunningRow(RunningSnapshot {
    id: crate::backend::identity::ModelIdentity::Gguf(crate::gguf::identity::ModelId {
      path: PathBuf::from(path),
      header_blake3: [7u8; 32],
    }),
    pid: 1,
    port: 41100,
    started_at: 0,
    launch_id: Some(LaunchId("L1".to_string())),
    name: None,
    preset: None,
    params: LaunchParams::new(PathBuf::from(path), LaunchMode::Chat),
    actuals: Default::default(),
    resolved_backend: crate::backend::DEFAULT_BACKEND_ID.to_string(),
    projected_demand_bytes: None,
    origin: None,
  })
}

impl RunningRow {
  /// Replace the GGUF identity with a backend (delegated / registry) one.
  pub fn identity(mut self, id: crate::backend::identity::ModelIdentity) -> Self {
    self.0.id = id;
    self
  }

  pub fn name(mut self, name: &str) -> Self {
    self.0.name = Some(name.to_string());
    self
  }

  /// The launch name as an `Option`, for a test that parameterises over both.
  pub fn maybe_name(mut self, name: Option<&str>) -> Self {
    self.0.name = name.map(str::to_string);
    self
  }

  /// The preset the launch resolved, as on a preset-backed running row.
  pub fn preset(mut self, preset: &str) -> Self {
    self.0.preset = Some(preset.to_string());
    self
  }

  pub fn launch_id(mut self, id: &str) -> Self {
    self.0.launch_id = Some(crate::daemon::registry::LaunchId(id.to_string()));
    self
  }

  /// Drop the launch id, as on a row adopted from a `state.json` written
  /// before the stamp existed.
  pub fn unstamped(mut self) -> Self {
    self.0.launch_id = None;
    self
  }

  pub fn port(mut self, port: u16) -> Self {
    self.0.port = port;
    self
  }

  pub fn pid(mut self, pid: i32) -> Self {
    self.0.pid = pid;
    self
  }

  pub fn started_at(mut self, secs: u64) -> Self {
    self.0.started_at = secs;
    self
  }

  pub fn params(mut self, params: crate::launch::params::LaunchParams) -> Self {
    self.0.params = params;
    self
  }

  /// The demand the admission gate priced this launch at — what make-room
  /// credits it for when it is unloaded.
  pub fn projected_demand(mut self, bytes: u64) -> Self {
    self.0.projected_demand_bytes = Some(bytes);
    self
  }

  /// How the launch came to be running — the sweep and make-room only ever
  /// consider an `AutoStart` row.
  pub fn origin(mut self, origin: crate::daemon::supervisor::LaunchOrigin) -> Self {
    self.0.origin = Some(origin);
    self
  }

  pub fn resolved_backend(mut self, backend: &str) -> Self {
    self.0.resolved_backend = backend.to_string();
    self
  }

  pub fn build(self) -> crate::daemon::state_store::RunningSnapshot {
    self.0
  }
}

/// The id of the registered backend that declares knob `knob_id`, for tests
/// that need a real non-default backend without naming one.
pub fn backend_declaring(knob_id: &str) -> &'static str {
  use crate::backend::Backend;
  crate::backend::Backends::all()
    .into_iter()
    .find(|b| b.knobs().iter().any(|k| k.id == knob_id))
    .map(|b| b.id())
    .unwrap_or_else(|| panic!("no backend declares knob `{knob_id}`"))
}

/// The newest request-log row once it satisfies `done`. The proxy finishes
/// a row when hyper drops the response body, which can trail the client's
/// read by a scheduler tick.
pub async fn newest_request_when(
  log: &crate::proxy::request_log::RequestLog,
  done: impl Fn(&crate::proxy::request_log::RequestRow) -> bool,
) -> crate::proxy::request_log::RequestRow {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  loop {
    match log.tail(None, 1).rows.into_iter().next() {
      Some(row) if done(&row) => return row,
      other => {
        assert!(
          std::time::Instant::now() < deadline,
          "log row never reached the expected state; last seen: {other:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      }
    }
  }
}

/// The newest request-log row once its response has ended.
pub async fn finished_request(
  log: &crate::proxy::request_log::RequestLog,
) -> crate::proxy::request_log::RequestRow {
  newest_request_when(log, |r| {
    r.state != crate::proxy::request_log::RequestState::InFlight
  })
  .await
}

/// A `ModelMetadata` for a 7B chat GGUF of architecture `arch`, for proxy and
/// supervisor tests that need a row rather than a parsed file.
pub fn fake_metadata(arch: &str) -> crate::gguf::metadata::ModelMetadata {
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};
  ModelMetadata {
    arch: Some(arch.to_string()),
    total_parameters: Some(7_000_000_000),
    parameter_label: Some("7B".to_string()),
    quant: Quant::Q4_K,
    quant_label: None,
    native_ctx: Some(8192),
    chat_template: None,
    tokenizer_kind: Some("llama".to_string()),
    reasoning_hint: false,
    mode_hint: ModeHint::Chat,
    weights_bytes: Some(4_000_000_000),
    lazy_tensor_bytes: Vec::new(),
    mtp: None,
  }
}

/// Write a minimal parseable GGUF of `arch` into `dir` under `name` and return
/// its canonical path, the shape a discovery row expects.
pub fn write_gguf(dir: &std::path::Path, name: &str, arch: &str) -> PathBuf {
  let path = dir.join(name);
  std::fs::write(&path, crate::gguf::test_fixtures::build_minimal_gguf(arch)).expect("write gguf");
  crate::util::paths::canonicalize(&path).expect("canonicalize")
}

/// Start the proxy serve loop on an ephemeral loopback port, wait until it
/// reports `Listening`, and hand back the bound address with the pieces the
/// caller needs to stop it ([`shutdown_listener`]).
pub async fn spawn_listener(
  state: std::sync::Arc<crate::proxy::state::ProxyState>,
) -> (
  std::net::SocketAddr,
  crate::daemon::shutdown::ShutdownToken,
  tokio::task::JoinHandle<()>,
) {
  use crate::proxy::server::{loopback_addr, new_status_cell, serve};
  let token = crate::daemon::shutdown::ShutdownToken::new();
  let status = new_status_cell();
  let bind_addr = loopback_addr(0);
  let token_for_task = token.clone();
  let status_for_task = std::sync::Arc::clone(&status);
  let handle = tokio::spawn(async move {
    serve(state, bind_addr, token_for_task, status_for_task)
      .await
      .expect("proxy serve returns Ok");
  });
  let bound = wait_for_listening(&status, std::time::Duration::from_secs(2))
    .await
    .expect("listener reaches Listening");
  (bound, token, handle)
}

/// Trigger shutdown and join the serve task with a generous budget. Catches a
/// hung serve loop instead of leaving a detached task that would otherwise be
/// silently torn down when the runtime exits.
pub async fn shutdown_listener(
  shutdown: crate::daemon::shutdown::ShutdownToken,
  handle: tokio::task::JoinHandle<()>,
) {
  shutdown.trigger();
  tokio::time::timeout(std::time::Duration::from_secs(5), handle)
    .await
    .expect("proxy serve loop must exit after shutdown.trigger()")
    .expect("proxy serve task must not panic");
}

/// Poll `status` for up to `budget` for the `Listening` address, or `None`.
pub async fn wait_for_listening(
  status: &crate::proxy::server::StatusCell,
  budget: std::time::Duration,
) -> Option<std::net::SocketAddr> {
  use crate::proxy::server::ProxyStatus;
  let deadline = std::time::Instant::now() + budget;
  while std::time::Instant::now() < deadline {
    if let ProxyStatus::Listening { addr, .. } = status.read().unwrap().clone() {
      return Some(addr);
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
  }
  None
}

/// One raw HTTP/1.1 exchange: connect, send `request`, read to EOF, split the
/// reply into `(status, headers, body)` with header names lowercased.
/// `Connection: close` keeps it one-shot, so a caller never has to frame a body
/// to know the response ended.
async fn http_round_trip(
  addr: std::net::SocketAddr,
  request: String,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
  sock.write_all(request.as_bytes()).await.expect("write");
  let mut buf = Vec::new();
  sock.read_to_end(&mut buf).await.expect("read");
  let needle = b"\r\n\r\n";
  let split = buf
    .windows(needle.len())
    .position(|w| w == needle)
    .expect("CRLFCRLF terminator");
  let head = std::str::from_utf8(&buf[..split]).expect("utf8 headers");
  let mut lines = head.split("\r\n");
  let status: u16 = lines
    .next()
    .expect("status line")
    .split_whitespace()
    .nth(1)
    .expect("status code")
    .parse()
    .expect("parse status");
  let headers = lines
    .filter_map(|line| line.split_once(':'))
    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
    .collect();
  (status, headers, buf[split + needle.len()..].to_vec())
}

fn head_lines(host: std::net::SocketAddr, extra_headers: &[(&str, &str)]) -> String {
  let mut req = format!("Host: {host}\r\nConnection: close\r\n");
  for (k, v) in extra_headers {
    req.push_str(&format!("{k}: {v}\r\n"));
  }
  req
}

/// A bare-socket `GET` with optional extra headers: `(status, headers, body)`.
pub async fn http_get(
  addr: std::net::SocketAddr,
  path: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "GET {path} HTTP/1.1\r\n{}{}",
    head_lines(addr, extra_headers),
    "\r\n"
  );
  http_round_trip(addr, request).await
}

/// A bare-socket `HEAD` with optional extra headers: `(status, headers, body)`.
pub async fn http_head(
  addr: std::net::SocketAddr,
  path: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "HEAD {path} HTTP/1.1\r\n{}{}",
    head_lines(addr, extra_headers),
    "\r\n"
  );
  http_round_trip(addr, request).await
}

/// A bare-socket JSON `POST` with optional extra headers:
/// `(status, headers, body)`.
pub async fn http_post(
  addr: std::net::SocketAddr,
  path: &str,
  body: &str,
  extra_headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
  let request = format!(
    "POST {path} HTTP/1.1\r\n{}Content-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
    head_lines(addr, extra_headers),
    body.len()
  );
  http_round_trip(addr, request).await
}

/// One header value from an [`http_get`] / [`http_post`] reply; `name` is
/// matched case-insensitively.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
  headers
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}
