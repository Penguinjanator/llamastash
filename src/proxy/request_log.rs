//! In-memory log of the model requests the proxy handled.
//!
//! One row per request on a forwarded `/v1/*` route: clock fields, the
//! routing outcome, and the token counts read from the end of the response
//! (see [`super::usage_tap`]). No prompt or response text is stored.
//!
//! Rows live in a ring of [`CAPACITY`]. Totals per model and per launch are
//! kept apart from the ring, so a summary still counts requests whose rows
//! have been pushed out. The ring and the totals are lost when the daemon
//! stops. [`RequestLog::write_to`] also appends each finished row to a file.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::usage_tap::Usage;

/// Rows kept. At roughly 400 bytes a row this is well under 1 MiB.
pub const CAPACITY: usize = 1000;

/// Rows one `requests_tail` call returns when it names no `limit`.
pub const DEFAULT_TAIL: usize = 100;

/// A caller's row limit: the default when it named none, never more than the
/// ring holds, so one query cannot ask for more rows than exist.
pub(crate) fn clamp_limit(raw: Option<usize>) -> usize {
  raw.unwrap_or(DEFAULT_TAIL).min(CAPACITY)
}

/// Name of the request log file inside the daemon's log directory.
pub const FILE_NAME: &str = "requests.jsonl";

/// How many stopped launches keep their totals. A launch id is never reused,
/// so totals deleted the moment a launch stops read as a counter reset to
/// anything that keeps reading them.
pub const RETIRED_LAUNCH_HISTORY: usize = 16;

/// Finished rows that may wait for the file writer. Past this the line is
/// dropped from the file (the in-memory log still has the row) rather
/// than holding up the request that produced it.
const FILE_QUEUE: usize = 1024;

/// The file the log is also written to.
struct FileSink {
  path: PathBuf,
  queue: mpsc::Sender<String>,
  /// Finished rows that did not reach the file: the queue was full, or
  /// the file could not be opened or written.
  dropped: AtomicU64,
}

/// What `status` reports about the request log file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileStatus {
  pub path: PathBuf,
  /// Finished rows that were not written to the file.
  pub dropped: u64,
}

/// Append every line from `rx` to the rotating file at `path`, until the
/// file cannot be opened or written. A line taken off the queue and not
/// written is added to `lost`.
async fn write_lines(
  path: &Path,
  rx: &mut mpsc::Receiver<String>,
  lost: &mut u64,
) -> std::io::Result<()> {
  let mut writer = crate::daemon::supervisor::LogWriter::open(path.to_path_buf()).await?;
  while let Some(line) = rx.recv().await {
    if let Err(e) = writer.write_line(line.as_bytes()).await {
      *lost += 1;
      return Err(e);
    }
  }
  Ok(())
}

/// Strings that come from a client or a child process are cut to these
/// lengths, and stripped of control characters, before they are stored.
const MAX_MODEL_CHARS: usize = 200;
const MAX_CAUSE_CHARS: usize = 300;

/// How a request ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
  /// No response yet, or the response body is still streaming.
  #[default]
  InFlight,
  /// The whole response was sent.
  Done,
  /// The client went away before the response was complete.
  ClientClosed,
  /// The upstream connection failed part-way through the response body.
  UpstreamError,
}

/// One logged request. Every key is always present on the wire; a value
/// the proxy does not know is `null`. Reading tolerates a missing key, so
/// a client and a daemon one field apart still understand each other.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRow {
  /// Counts up from 1 for the life of the daemon.
  pub seq: u64,
  /// Unix time in milliseconds when the proxy received the request.
  pub started_at_ms: u64,
  /// Peer address of the client connection (`ip:port`).
  pub client: Option<String>,
  /// Request path, e.g. `/v1/chat/completions`.
  pub route: String,
  /// The `model` string the client sent.
  pub requested_model: Option<String>,
  /// Name of the model the row belongs to: the one that served the request,
  /// or the one asked for when nothing served it.
  pub model: Option<String>,
  /// Catalog path of that model. `requests_tail` filters on it.
  pub model_path: Option<String>,
  /// Launch that served the request.
  pub launch_id: Option<String>,
  pub state: RequestState,
  /// HTTP status sent to the client.
  pub status: Option<u16>,
  /// The proxy's error `code`, or its `type` when it has no code, when the
  /// proxy answered the request itself.
  pub error: Option<String>,
  /// That error's message.
  pub cause: Option<String>,
  /// This request spawned the model's launch. Not set when the spawn was
  /// refused, or when the request waited on a launch something else started.
  pub auto_start: bool,
  /// Launches unloaded to make room for this request's auto-start.
  pub evicted: Vec<String>,
  /// Why another model answered (`launch_failed` / `family_mismatch`).
  pub fallback: Option<String>,
  /// Milliseconds from receipt to the first response body byte.
  pub ttfb_ms: Option<u64>,
  /// Milliseconds from receipt to the end of the response.
  pub duration_ms: Option<u64>,
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  /// Generation speed: what the server reported, or the proxy's estimate.
  pub tokens_per_second: Option<f64>,
  /// The server reported no speed, so the proxy worked it out from the
  /// token count and its own clock.
  pub tokens_per_second_estimated: bool,
}

/// A row as table-cell text. The CLI table and the TUI tab both render
/// from it, so one field cannot be formatted two ways.
pub struct RequestCells {
  /// `HH:MM:SS` the request arrived, in local time.
  pub time: String,
  pub status: String,
  pub total: String,
  pub ttfb: String,
  /// Prefixed with `~` when the speed is the proxy's estimate.
  pub speed: String,
  pub tokens_in: String,
  pub tokens_out: String,
  /// The route without its `/v1/` prefix, which every logged route has.
  pub route: String,
  pub model: String,
  pub launch: String,
  pub client: String,
  /// See [`RequestRow::note`]. Empty for a plain served request.
  pub note: String,
}

impl RequestRow {
  /// Counted as an error in a summary: a 4xx / 5xx answer, or a response
  /// the upstream cut short. A client that hung up is not an error.
  pub fn is_error(&self) -> bool {
    self.state == RequestState::UpstreamError || self.status.is_some_and(|s| s >= 400)
  }

  fn is_success(&self) -> bool {
    self.state == RequestState::Done && self.status.is_some_and(|s| s < 400)
  }

  /// The row's cells. `none` stands in for a value the log does not have.
  pub fn cells(&self, none: &str) -> RequestCells {
    let or_none = |v: Option<String>| v.unwrap_or_else(|| none.to_string());
    let (hour, min, sec) = crate::util::datetime::local_hms(self.started_at_ms / 1000);
    RequestCells {
      time: format!("{hour:02}:{min:02}:{sec:02}"),
      status: or_none(self.status.map(|s| s.to_string())),
      total: or_none(self.duration_ms.map(fmt_ms)),
      ttfb: or_none(self.ttfb_ms.map(fmt_ms)),
      speed: or_none(self.tokens_per_second.map(|t| {
        let mark = if self.tokens_per_second_estimated {
          "~"
        } else {
          ""
        };
        format!("{mark}{t:.1}")
      })),
      tokens_in: or_none(self.prompt_tokens.map(fmt_count)),
      tokens_out: or_none(self.completion_tokens.map(fmt_count)),
      route: self
        .route
        .strip_prefix("/v1/")
        .unwrap_or(&self.route)
        .to_string(),
      model: or_none(self.model.clone().or_else(|| self.requested_model.clone())),
      launch: or_none(self.launch_id.clone()),
      client: or_none(self.client.clone()),
      note: self.note(),
    }
  }

  /// What happened beyond the status code, on one line: the auto-start,
  /// what was unloaded for it, a fallback, the proxy's own error, or how
  /// the response was cut short. Empty for a plain served request.
  pub fn note(&self) -> String {
    let mut parts: Vec<String> = Vec::new();
    match self.state {
      // The model is known and no launch has taken the request yet.
      RequestState::InFlight if self.model_path.is_some() && self.launch_id.is_none() => {
        parts.push("loading model".to_string());
      }
      RequestState::InFlight => parts.push("in flight".to_string()),
      RequestState::ClientClosed => parts.push("client closed".to_string()),
      RequestState::UpstreamError => parts.push("upstream error".to_string()),
      RequestState::Done => {}
    }
    if self.auto_start && self.state != RequestState::InFlight {
      parts.push("auto-start".to_string());
    }
    if !self.evicted.is_empty() {
      parts.push(format!("unloaded {}", self.evicted.join(" ")));
    }
    if let Some(reason) = &self.fallback {
      parts.push(match &self.requested_model {
        Some(asked) => format!("fallback ({reason}) for {asked}"),
        None => format!("fallback ({reason})"),
      });
    }
    if let Some(error) = &self.error {
      parts.push(match &self.cause {
        Some(cause) => format!("{error}: {cause}"),
        None => error.clone(),
      });
    }
    parts.join(", ")
  }
}

/// A token count for a table cell: `9999`, `12.3k`, `1.2M`.
fn fmt_count(n: u64) -> String {
  // One decimal of thousands rounds to `1000.0k` from here up.
  const MILLIONS_FROM: u64 = 999_950;
  if n < 10_000 {
    n.to_string()
  } else if n < MILLIONS_FROM {
    format!("{:.1}k", n as f64 / 1_000.0)
  } else {
    format!("{:.1}M", n as f64 / 1_000_000.0)
  }
}

/// A duration for a table cell: `850ms`, `1.2s`, `2m05s`, `1h02m`.
fn fmt_ms(ms: u64) -> String {
  let secs = ms / 1000;
  if ms < 1000 {
    format!("{ms}ms")
  } else if secs < 60 {
    format!("{:.1}s", ms as f64 / 1000.0)
  } else if secs < 3600 {
    format!("{}m{:02}s", secs / 60, secs % 60)
  } else {
    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
  }
}

/// Totals of the finished requests of one model, one launch, or the whole
/// log, since the daemon started. Averages cover completed 2xx / 3xx
/// requests only, so a fast 404 does not pull the latency down.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestSummary {
  pub requests: u64,
  pub errors: u64,
  pub avg_duration_ms: Option<u64>,
  pub avg_ttfb_ms: Option<u64>,
  /// Mean of the per-request speeds, estimated ones included.
  pub tokens_per_second_avg: Option<f64>,
  /// Mean speed of the newest launch of the scoped model only: what this
  /// session is running at, with a reloaded model no older launch pulling
  /// the average. Set for a model scope only; `None` for the whole log and
  /// for a scope no request has reported a speed for yet.
  pub tokens_per_second_session: Option<f64>,
  /// The most recent speed a request reported. Kept in the JSON and in the
  /// row table, but no longer on the summary strips: the session figure says
  /// the same thing about what is running now.
  pub tokens_per_second_last: Option<f64>,
  /// Distinct client IPs among the rows the scope retains. Loopback tools
  /// all share `127.0.0.1`, so this counts machines and reads 1 until a LAN
  /// bind; [`Self::connections`] is the figure that counts tools.
  pub hosts: Option<u64>,
  /// Distinct client peers on `ip:port` among the retained rows — what one
  /// tool's live connection looks like to the proxy.
  pub connections: Option<u64>,
  /// [`Self::hosts`] over the requests in flight right now.
  pub live_hosts: Option<u64>,
  /// [`Self::connections`] over the requests in flight right now.
  pub live_connections: Option<u64>,
  pub prompt_tokens: u64,
  pub completion_tokens: u64,
  /// Requests that started the model.
  pub auto_starts: u64,
  /// Launches unloaded to make room.
  pub evictions: u64,
}

impl RequestSummary {
  /// Label and text of every figure, in display order. `none` stands in
  /// for a figure no request has supplied yet.
  pub fn cells(&self, none: &str) -> Vec<(&'static str, String)> {
    let ms = |v: Option<u64>| v.map(fmt_ms).unwrap_or_else(|| none.to_string());
    let tps = |v: Option<f64>| {
      v.map(|t| format!("{t:.1}"))
        .unwrap_or_else(|| none.to_string())
    };
    let num = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| none.to_string());
    vec![
      ("Requests", self.requests.to_string()),
      ("Errors", self.errors.to_string()),
    ]
    .into_iter()
    .chain(self.live_cell())
    .chain([
      ("Hosts", num(self.hosts)),
      ("Connections", num(self.connections)),
      ("Avg total", ms(self.avg_duration_ms)),
      ("Avg TTFB", ms(self.avg_ttfb_ms)),
      ("Avg tok/s", tps(self.tokens_per_second_avg)),
    ])
    .chain(
      self
        .tokens_per_second_session
        .map(|t| ("Tok/s session", format!("{t:.1}"))),
    )
    .chain([
      ("Tokens in", fmt_count(self.prompt_tokens)),
      ("Tokens out", fmt_count(self.completion_tokens)),
      ("Auto-starts", self.auto_starts.to_string()),
      ("Unloaded", self.evictions.to_string()),
    ])
    .collect()
  }

  /// The in-flight pair as one cell: `Live N` when every host in flight is on
  /// one connection and `Live N/M` when they differ (an agent runner with two
  /// tools open reads `1/2`). Absent when nothing is in flight, so an idle
  /// launch's strip looks the way it did before the figure existed, and
  /// absent from a daemon that never computed it.
  fn live_cell(&self) -> Option<(&'static str, String)> {
    let (hosts, connections) = (self.live_hosts?, self.live_connections?);
    (hosts > 0 || connections > 0).then(|| {
      let text = if hosts == connections {
        hosts.to_string()
      } else {
        format!("{hosts}/{connections}")
      };
      ("Live", text)
    })
  }
}

/// How many hosts and `ip:port` peers a set of rows came from. Both figures
/// count rows a scope still retains rather than the since-daemon-start
/// totals, so they describe the table on screen and do not grow as the daemon
/// ages.
#[derive(Debug, Clone, Default)]
pub struct Clients {
  requests: u64,
  hosts: HashSet<String>,
  connections: HashSet<String>,
}

impl Clients {
  fn add(&mut self, row: &RequestRow) {
    self.requests += 1;
    let Some(client) = &row.client else {
      return;
    };
    self.connections.insert(client.clone());
    self.hosts.insert(client_host(client).to_string());
  }

  /// Rows counted, in flight ones included.
  pub fn requests(&self) -> u64 {
    self.requests
  }

  /// Distinct client IPs.
  pub fn hosts(&self) -> u64 {
    self.hosts.len() as u64
  }

  /// Distinct `ip:port` peers.
  pub fn connections(&self) -> u64 {
    self.connections.len() as u64
  }
}

/// [`Clients`] of a scope and the same over its in-flight rows.
#[derive(Default)]
struct ClientPair {
  all: Clients,
  live: Clients,
}

impl ClientPair {
  fn add(&mut self, row: &RequestRow) {
    self.all.add(row);
    if row.state == RequestState::InFlight {
      self.live.add(row);
    }
  }

  /// Stamp the four client figures onto `summary`. A daemon that computes
  /// them always answers, so an empty scope reads `Some(0)`; only an older
  /// daemon leaves them `None`.
  fn stamp(&self, summary: &mut RequestSummary) {
    summary.hosts = Some(self.all.hosts());
    summary.connections = Some(self.all.connections());
    summary.live_hosts = Some(self.live.hosts());
    summary.live_connections = Some(self.live.connections());
  }
}

/// The address part of a client string, which is always a `SocketAddr`'s text
/// (`127.0.0.1:5000`, `[::1]:5000`). Anything that does not carry a port is
/// its own host.
fn client_host(client: &str) -> &str {
  let host = match client.rsplit_once(':') {
    Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => host,
    _ => client,
  };
  host.trim_matches(&['[', ']'][..])
}

#[derive(Debug, Clone, Default)]
struct Totals {
  requests: u64,
  errors: u64,
  duration_ms_sum: u64,
  ttfb_ms_sum: u64,
  /// Requests in the two sums above.
  timed: u64,
  tps_sum: f64,
  tps_n: u64,
  tps_last: Option<f64>,
  prompt_tokens: u64,
  completion_tokens: u64,
  auto_starts: u64,
  evictions: u64,
  /// Display name of the model these rows belong to, taken from them, so a
  /// launch's figures and the model's print the same name.
  label: Option<String>,
}

impl Totals {
  fn add(&mut self, row: &RequestRow) {
    if row.model.is_some() {
      self.label = row.model.clone();
    }
    self.requests += 1;
    if row.is_error() {
      self.errors += 1;
    }
    if let (true, Some(total), Some(ttfb)) = (row.is_success(), row.duration_ms, row.ttfb_ms) {
      self.duration_ms_sum = self.duration_ms_sum.saturating_add(total);
      self.ttfb_ms_sum = self.ttfb_ms_sum.saturating_add(ttfb);
      self.timed += 1;
    }
    if let Some(tps) = row.tokens_per_second {
      self.tps_sum += tps;
      self.tps_n += 1;
      self.tps_last = Some(tps);
    }
    self.prompt_tokens = self
      .prompt_tokens
      .saturating_add(row.prompt_tokens.unwrap_or(0));
    self.completion_tokens = self
      .completion_tokens
      .saturating_add(row.completion_tokens.unwrap_or(0));
    self.auto_starts += u64::from(row.auto_start);
    self.evictions += row.evicted.len() as u64;
  }

  fn summary(&self) -> RequestSummary {
    RequestSummary {
      requests: self.requests,
      errors: self.errors,
      avg_duration_ms: self.duration_ms_sum.checked_div(self.timed),
      avg_ttfb_ms: self.ttfb_ms_sum.checked_div(self.timed),
      tokens_per_second_avg: (self.tps_n > 0).then(|| self.tps_sum / self.tps_n as f64),
      // `RequestLog::tail` fills this for a model scope; a single set of
      // totals has no notion of "the newest launch".
      tokens_per_second_session: None,
      tokens_per_second_last: self.tps_last,
      // The client figures count rows, which a set of totals no longer holds.
      // `RequestLog::tail` and `snapshot` fill them in.
      hosts: None,
      connections: None,
      live_hosts: None,
      live_connections: None,
      prompt_tokens: self.prompt_tokens,
      completion_tokens: self.completion_tokens,
      auto_starts: self.auto_starts,
      evictions: self.evictions,
    }
  }
}

#[derive(Default)]
struct Inner {
  rows: VecDeque<RequestRow>,
  next_seq: u64,
  all: Totals,
  // Finished requests per request path. The router logs only the fixed paths
  // it matches exactly, so this cannot grow past the route table.
  by_route: HashMap<String, u64>,
  // One entry per model that got a request. Never removed: the keys are
  // catalog paths, so the map is as large as the catalog at most.
  by_model: HashMap<String, Totals>,
  // One entry per launch a request was handed to. A launch id is never
  // reused, so [`RequestLog::forget_launch`] moves the entry out when the
  // launch stops, or every auto-start would leave one behind.
  by_launch: HashMap<String, Totals>,
  // The entries `forget_launch` moved out, oldest first, capped at
  // [`RETIRED_LAUNCH_HISTORY`].
  retired: VecDeque<(String, Totals)>,
}

impl Inner {
  /// Totals for `scope`, or `None` for a scope nothing has contributed to.
  fn totals(&self, scope: &LogScope) -> Option<&Totals> {
    match scope {
      LogScope::All => Some(&self.all),
      LogScope::Model { path } => self.by_model.get(path),
      LogScope::Launch { id } => RequestLog::launch_totals(self, id),
    }
  }

  /// Overwrite the ring's copy of `row`, if the ring still holds it.
  fn store(&mut self, row: &RequestRow) {
    let Some(front) = self.rows.front().map(|r| r.seq) else {
      return;
    };
    // Rows are pushed in `seq` order and only ever popped from the front,
    // so a row's slot is its distance from the front.
    let Some(slot) = row
      .seq
      .checked_sub(front)
      .and_then(|i| self.rows.get_mut(i as usize))
    else {
      return;
    };
    *slot = row.clone();
  }
}

/// What `requests_tail` returns. It is the method's JSON body, so the
/// daemon writes it and the CLI and the TUI read it through this type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tail {
  pub summary: RequestSummary,
  /// Newest first.
  #[serde(rename = "requests")]
  pub rows: Vec<RequestRow>,
}

/// Shared handle to the log. Cheap to clone.
#[derive(Clone, Default)]
pub struct RequestLog {
  inner: Arc<Mutex<Inner>>,
  /// Set once [`Self::write_to`] has started a file writer.
  file: Arc<OnceLock<FileSink>>,
}

impl RequestLog {
  pub fn new() -> Self {
    Self::default()
  }

  // The lock is never held across an await and every critical section is a
  // few field writes, so a poisoned lock still guards consistent data.
  fn lock(&self) -> MutexGuard<'_, Inner> {
    self.inner.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Also append every finished row to `path`, one JSON object per line
  /// with the keys `requests_tail` returns. The file rotates like a
  /// launch log. Call once, from inside the daemon's runtime.
  ///
  /// A failure to open or write the file is logged once and ends the
  /// writing, the way a launch's log capture pauses. Rows that do not
  /// reach the file are counted, and [`Self::file_status`] reports the
  /// count.
  pub fn write_to(&self, path: PathBuf) {
    let (queue, mut rx) = mpsc::channel::<String>(FILE_QUEUE);
    let sink = FileSink {
      path: path.clone(),
      queue,
      dropped: AtomicU64::new(0),
    };
    if self.file.set(sink).is_err() {
      return;
    }
    let file = Arc::clone(&self.file);
    tokio::spawn(async move {
      let mut lost = 0;
      if let Err(e) = write_lines(&path, &mut rx, &mut lost).await {
        log::warn!(
          "proxy: request log file {} failed: {e}; writing to it stopped (the in-memory log continues, `status` counts the rows dropped)",
          path.display()
        );
      }
      // Closing the queue makes every later row count as dropped at the
      // sender. The rows still in it will never be written either.
      rx.close();
      while rx.try_recv().is_ok() {
        lost += 1;
      }
      if let Some(installed) = file.get() {
        installed.dropped.fetch_add(lost, Ordering::Relaxed);
      }
    });
  }

  /// Where the log is also written and how many finished rows did not
  /// make it there. `None` when no file was asked for.
  pub fn file_status(&self) -> Option<FileStatus> {
    self.file.get().map(|sink| FileStatus {
      path: sink.path.clone(),
      dropped: sink.dropped.load(Ordering::Relaxed),
    })
  }

  /// Start a row for a request the proxy just received. The returned
  /// record finishes the row when it is dropped, so every exit path is
  /// logged, including a client that hangs up while a model loads.
  ///
  /// `route` is stored uncut: the router logs only the fixed paths it
  /// matches exactly, so a client cannot make it long.
  pub fn begin(&self, route: &str, client: Option<SocketAddr>) -> RequestRecord {
    let started_at_ms = crate::util::datetime::now_ms();
    let mut inner = self.lock();
    inner.next_seq += 1;
    let row = RequestRow {
      seq: inner.next_seq,
      started_at_ms,
      client: client.map(|c| c.to_string()),
      route: route.to_string(),
      ..RequestRow::default()
    };
    if inner.rows.len() >= CAPACITY {
      inner.rows.pop_front();
    }
    inner.rows.push_back(row.clone());
    RequestRecord {
      log: self.clone(),
      row,
      started: Instant::now(),
      finished: false,
    }
  }

  /// The newest `limit` rows of `scope`, newest first, and the summary for
  /// that same scope.
  pub fn tail(&self, scope: LogScope, limit: usize) -> Tail {
    let inner = self.lock();
    let totals = inner.totals(&scope);
    let mut summary = totals.map(Totals::summary).unwrap_or_default();
    let mut counts = ClientPair::default();
    for row in inner.rows.iter().filter(|row| scope.matches(row)) {
      counts.add(row);
    }
    counts.stamp(&mut summary);
    // The speed of the newest launch that served this model, so a model
    // reloaded at a different speed shows both numbers.
    if let LogScope::Model { .. } = scope {
      summary.tokens_per_second_session = inner
        .rows
        .iter()
        .rev()
        .find(|r| scope.matches(r))
        .and_then(|r| r.launch_id.as_deref())
        .and_then(|id| RequestLog::launch_totals(&inner, id))
        .and_then(|t| Totals::summary(t).tokens_per_second_avg);
    }
    Tail {
      summary,
      rows: inner
        .rows
        .iter()
        .rev()
        .filter(|r| scope.matches(r))
        .take(limit)
        .cloned()
        .collect(),
    }
  }

  /// Retire a stopped launch's totals. `status` only reads the totals of
  /// launches that are still registered; [`Self::launch_summary`] still
  /// answers for the last [`RETIRED_LAUNCH_HISTORY`] stopped launches.
  pub fn forget_launch(&self, launch_id: &str) {
    let mut inner = self.lock();
    let Some(totals) = inner.by_launch.remove(launch_id) else {
      return;
    };
    inner.retired.push_back((launch_id.to_string(), totals));
    while inner.retired.len() > RETIRED_LAUNCH_HISTORY {
      inner.retired.pop_front();
    }
  }

  /// Summary of the requests one launch served, live first then retired.
  ///
  /// This is [`Self::tail`] on a launch scope with no rows asked for, so the
  /// stamping `tail` does reaches the per-launch surfaces too.
  pub fn launch_summary(&self, launch_id: &str) -> RequestSummary {
    self
      .tail(
        LogScope::Launch {
          id: launch_id.to_string(),
        },
        0,
      )
      .summary
  }

  /// Display name of the model `launch_id` served, or `None` when no finished
  /// row of it carries one.
  pub fn launch_label(&self, launch_id: &str) -> Option<String> {
    Self::launch_totals(&self.lock(), launch_id).and_then(|totals| totals.label.clone())
  }

  fn launch_totals<'a>(inner: &'a Inner, launch_id: &str) -> Option<&'a Totals> {
    inner.by_launch.get(launch_id).or_else(|| {
      inner
        .retired
        .iter()
        .rev()
        .find(|(retired, _)| retired == launch_id)
        .map(|(_, totals)| totals)
    })
  }

  /// The whole log in owned data, taken under one lock. A `/metrics` scrape
  /// renders from this instead of holding the lock while it builds a body.
  pub fn snapshot(&self) -> LogSnapshot {
    let inner = self.lock();
    // The client figures of every scope, in one pass over the ring, so a
    // `/metrics` summary prints the same four numbers as `requests --json`
    // of the same scope: both count these rows and nothing else.
    let mut counts = ClientPair::default();
    let mut model_counts: HashMap<&str, ClientPair> = HashMap::new();
    let mut launch_counts: HashMap<&str, ClientPair> = HashMap::new();
    for row in inner.rows.iter() {
      counts.add(row);
      if let Some(path) = row.model_path.as_deref() {
        model_counts.entry(path).or_default().add(row);
      }
      if let Some(launch) = row.launch_id.as_deref() {
        launch_counts.entry(launch).or_default().add(row);
      }
    }
    let view = |key: String, totals: &Totals| ScopeTotals {
      key,
      label: totals.label.clone(),
      summary: totals.summary(),
      duration_ms_sum: totals.duration_ms_sum,
      ttfb_ms_sum: totals.ttfb_ms_sum,
      timed: totals.timed,
    };
    let mut models: Vec<ScopeTotals> = inner
      .by_model
      .iter()
      .map(|(path, totals)| view(path.clone(), totals))
      .collect();
    models.sort_by(|a, b| a.key.cmp(&b.key));
    // A scope whose rows all left the ring has no counts of its own; zero is
    // the answer, not "not computed".
    let empty = ClientPair::default();
    for model in &mut models {
      model_counts
        .get(model.key.as_str())
        .unwrap_or(&empty)
        .stamp(&mut model.summary);
    }
    let mut launches: Vec<LaunchTotals> = inner
      .by_launch
      .iter()
      .map(|(id, totals)| LaunchTotals {
        totals: view(id.clone(), totals),
        live: true,
      })
      .collect();
    launches.sort_by(|a, b| a.totals.key.cmp(&b.totals.key));
    launches.extend(inner.retired.iter().map(|(id, totals)| LaunchTotals {
      totals: view(id.clone(), totals),
      live: false,
    }));
    for launch in &mut launches {
      launch_counts
        .get(launch.totals.key.as_str())
        .unwrap_or(&empty)
        .stamp(&mut launch.totals.summary);
    }
    let mut all = view(String::new(), &inner.all);
    counts.stamp(&mut all.summary);
    let mut routes: Vec<(String, u64)> = inner
      .by_route
      .iter()
      .map(|(route, n)| (route.clone(), *n))
      .collect();
    routes.sort();
    LogSnapshot {
      rows: inner.rows.iter().cloned().collect(),
      all,
      models,
      launches,
      routes,
    }
  }
}

/// One scope's totals as a `/metrics` scrape reads them: the summary
/// `requests --json` returns, plus the sums behind its averages. A counter
/// needs the total; a mean cannot be re-expanded into one.
#[derive(Debug, Clone)]
pub struct ScopeTotals {
  /// Catalog path for a model, launch id for a launch, empty for the whole log.
  pub key: String,
  /// Display name of the model these totals belong to.
  pub label: Option<String>,
  pub summary: RequestSummary,
  pub duration_ms_sum: u64,
  pub ttfb_ms_sum: u64,
  /// How many requests the two sums above cover. Not `summary.requests`: a sum
  /// only holds a request the proxy could time, so a mean has to divide by this,
  /// and an error or a cancellation moves the request count but neither sum.
  pub timed: u64,
}

/// A launch's totals, and whether the launch is still running.
pub struct LaunchTotals {
  pub totals: ScopeTotals,
  pub live: bool,
}

/// Owned view of the log for one scrape.
pub struct LogSnapshot {
  /// The ring's rows, oldest first.
  pub rows: Vec<RequestRow>,
  /// Totals of every finished request.
  pub all: ScopeTotals,
  /// One entry per model a request reached, sorted by catalog path.
  pub models: Vec<ScopeTotals>,
  /// Live launches sorted by id, then the retired ones, oldest first.
  pub launches: Vec<LaunchTotals>,
  /// Finished requests per request path, sorted by path.
  pub routes: Vec<(String, u64)>,
}

impl LogSnapshot {
  /// The in-flight rows one scope holds: how many there are, and how many
  /// distinct hosts and peers they came from.
  pub fn in_flight(&self, mut pred: impl FnMut(&RequestRow) -> bool) -> Clients {
    let mut clients = Clients::default();
    for row in self
      .rows
      .iter()
      .filter(|row| row.state == RequestState::InFlight)
      .filter(|row| pred(row))
    {
      clients.add(row);
    }
    clients
  }

  /// Catalog path a launch served, from the rows that carry both. A retired
  /// launch whose rows left the ring has no entry.
  pub fn launch_path(&self, launch_id: &str) -> Option<&str> {
    self
      .rows
      .iter()
      .rev()
      .find(|row| row.launch_id.as_deref() == Some(launch_id))
      .and_then(|row| row.model_path.as_deref())
  }
}

/// Which requests a read covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogScope {
  /// Every request the log holds.
  All,
  /// One model's, across every launch that served it.
  Model { path: String },
  /// One launch's, live or retired. Strictly the launch's own rows: a request
  /// filed under the model that never reached this launch belongs to the
  /// model's figures, not here.
  Launch { id: String },
}

impl LogScope {
  fn matches(&self, row: &RequestRow) -> bool {
    match self {
      LogScope::All => true,
      LogScope::Model { path } => row.model_path.as_deref() == Some(path.as_str()),
      LogScope::Launch { id } => row.launch_id.as_deref() == Some(id.as_str()),
    }
  }
}

/// Write handle for one request's row. Setters change the record's own
/// copy; [`Self::publish`] and the finishing calls copy it into the log.
pub struct RequestRecord {
  log: RequestLog,
  row: RequestRow,
  started: Instant,
  finished: bool,
}

impl RequestRecord {
  pub fn set_requested_model(&mut self, model: Option<&str>) {
    self.row.requested_model = model.map(|m| clean(m, MAX_MODEL_CHARS));
  }

  /// Attribute the row to a model: its display name and catalog path.
  pub fn set_model(&mut self, name: &str, path: &str) {
    self.row.model = Some(clean(name, MAX_MODEL_CHARS));
    self.row.model_path = Some(path.to_string());
  }

  /// The request was handed to `launch_id`. This is also where the
  /// launch's totals entry is created; see [`Self::finish`] for why not
  /// there.
  pub fn set_launch(&mut self, launch_id: &str) {
    self.row.launch_id = Some(launch_id.to_string());
    self
      .log
      .lock()
      .by_launch
      .entry(launch_id.to_string())
      .or_default();
  }

  pub fn set_auto_start(&mut self) {
    self.row.auto_start = true;
  }

  pub fn set_evicted(&mut self, launches: Vec<String>) {
    self.row.evicted = launches;
  }

  pub fn set_fallback(&mut self, reason: &str) {
    self.row.fallback = Some(reason.to_string());
  }

  /// The upstream answered with `status`; the body is about to stream.
  pub fn set_status(&mut self, status: u16) {
    self.row.status = Some(status);
  }

  /// The first response body byte arrived. Later calls change nothing.
  pub fn mark_first_byte(&mut self) {
    if self.row.ttfb_ms.is_none() {
      self.row.ttfb_ms = Some(self.elapsed_ms());
    }
  }

  /// Copy the row as it stands into the log, so a reader sees a request
  /// that is still waiting on a load or still streaming.
  ///
  /// A row that already names its model opens that model's totals entry here
  /// rather than leaving it to [`Self::finish`]: the first request to a model is
  /// the one that carries the whole load, and the per-model in-flight gauge
  /// needs an entry to sit on while that request is still running.
  pub fn publish(&self) {
    let mut inner = self.log.lock();
    inner.store(&self.row);
    if let Some(path) = &self.row.model_path {
      let totals = inner.by_model.entry(path.clone()).or_default();
      // Only the label, not `Totals::add`: the request has not finished. The
      // label has to be the display name now, or the first scrape of a model
      // prints a different `model` value than every scrape after it.
      if totals.label.is_none() {
        totals.label = self.row.model.clone();
      }
    }
  }

  /// The proxy answered the request itself, with `error` (its error code
  /// or type) and `cause` (the error message). Finishes the row.
  pub fn respond(mut self, status: u16, error: Option<&str>, cause: Option<&str>) {
    self.row.status = Some(status);
    self.row.error = error.map(str::to_string);
    self.row.cause = cause.map(|c| clean(c, MAX_CAUSE_CHARS));
    self.finish(RequestState::Done, Usage::default());
  }

  /// The response body ended. Stores the final row and adds it to the totals.
  pub fn finish(&mut self, state: RequestState, usage: Usage) {
    if self.finished {
      return;
    }
    self.finished = true;
    self.row.state = state;
    self.row.duration_ms = Some(self.elapsed_ms());
    self.row.prompt_tokens = usage.prompt_tokens;
    self.row.completion_tokens = usage.completion_tokens;
    self.row.tokens_per_second = usage.tokens_per_second;
    self.row.tokens_per_second_estimated = usage.tokens_per_second_estimated;
    {
      let mut inner = self.log.lock();
      let inner = &mut *inner;
      inner.store(&self.row);
      inner.all.add(&self.row);
      *inner.by_route.entry(self.row.route.clone()).or_default() += 1;
      if let Some(path) = &self.row.model_path {
        inner
          .by_model
          .entry(path.clone())
          .or_default()
          .add(&self.row);
      }
      // No `or_default` here: a response can end after its launch was
      // stopped and forgotten, and must not bring the entry back.
      if let Some(totals) = self
        .row
        .launch_id
        .as_ref()
        .and_then(|launch| inner.by_launch.get_mut(launch))
      {
        totals.add(&self.row);
      }
    }
    if let Some(sink) = self.log.file.get() {
      let queued = serde_json::to_string(&self.row)
        .ok()
        .is_some_and(|line| sink.queue.try_send(line).is_ok());
      if !queued {
        sink.dropped.fetch_add(1, Ordering::Relaxed);
      }
    }
  }

  fn elapsed_ms(&self) -> u64 {
    self.started.elapsed().as_millis() as u64
  }
}

impl Drop for RequestRecord {
  fn drop(&mut self) {
    // Reached without a finishing call only when hyper dropped the request
    // future, which it does when the client disconnects.
    self.finish(RequestState::ClientClosed, Usage::default());
  }
}

/// `s` cut to `max_chars`, with every control character turned into a
/// space. The model name is whatever a client sent and a cause can quote a
/// child's output, and both are later printed to a terminal, where a
/// newline breaks a table row and an escape sequence could do worse.
fn clean(s: &str, max_chars: usize) -> String {
  s.chars()
    .take(max_chars)
    .map(|c| if c.is_control() { ' ' } else { c })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  const ROUTE: &str = "/v1/chat/completions";

  fn served(log: &RequestLog, path: &str, launch: &str, usage: Usage) -> u64 {
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    let seq = rec.row.seq;
    rec.finish(RequestState::Done, usage);
    seq
  }

  /// One served request from one client peer.
  fn served_from(log: &RequestLog, path: &str, launch: &str, client: &str) {
    let mut rec = log.begin(ROUTE, Some(client.parse().unwrap()));
    rec.set_model("m", path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(RequestState::Done, Usage::default());
  }

  /// A current daemon always answers the client questions, so an empty scope
  /// reads computed zeros. `None` belongs to a daemon that never had them.
  fn zeros() -> RequestSummary {
    RequestSummary {
      hosts: Some(0),
      connections: Some(0),
      live_hosts: Some(0),
      live_connections: Some(0),
      ..RequestSummary::default()
    }
  }

  #[test]
  fn begin_shows_an_in_flight_row_and_finish_completes_it() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, Some("127.0.0.1:5000".parse().unwrap()));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows.len(), 1);
    assert_eq!(tail.rows[0].state, RequestState::InFlight);
    assert_eq!(tail.rows[0].client.as_deref(), Some("127.0.0.1:5000"));
    assert_eq!(tail.summary.requests, 0, "the summary counts finished rows");

    rec.set_status(200);
    rec.finish(RequestState::Done, Usage::default());
    let done = log.tail(LogScope::All, 10);
    assert_eq!(done.rows[0].state, RequestState::Done);
    assert_eq!(done.rows[0].status, Some(200));
    assert!(done.rows[0].duration_ms.is_some());
    assert_eq!(done.summary.requests, 1);
  }

  #[test]
  fn setters_stay_private_until_published() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    assert_eq!(log.tail(LogScope::All, 10).rows[0].model_path, None);
    rec.publish();
    assert_eq!(
      log.tail(LogScope::All, 10).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
  }

  #[test]
  fn publishing_a_model_row_opens_its_totals_entry() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    rec.publish();
    let snap = log.snapshot();
    // The in-flight gauge only has a series to sit on once the model has an
    // entry, and the entry has to carry the display name from the start or the
    // first scrape of a model prints a different `model` than later ones.
    assert_eq!(snap.models.len(), 1);
    assert_eq!(snap.models[0].key, "/m/a.gguf");
    assert_eq!(snap.models[0].label.as_deref(), Some("m"));
    assert_eq!(snap.models[0].summary.requests, 0);
    assert_eq!(snap.in_flight(|row| row.model_path.is_some()).requests(), 1);
  }

  #[test]
  fn hosts_and_connections_count_the_clients_of_the_rows_a_scope_keeps() {
    let log = RequestLog::new();
    // Three finished requests over three peers from two hosts, then two
    // requests still streaming from one host over two peers.
    served_from(&log, "/m/a.gguf", "L1", "127.0.0.1:5001");
    served_from(&log, "/m/a.gguf", "L1", "127.0.0.1:5002");
    served_from(&log, "/m/a.gguf", "L2", "192.168.1.7:6001");
    // The two live requests share a host but not a peer, which is the case
    // the pair exists for: one machine, two tools talking. The records have
    // to stay alive — dropping one finishes the row.
    let streaming: Vec<RequestRecord> = [6002u16, 6003]
      .into_iter()
      .map(|port| {
        let mut rec = log.begin(ROUTE, Some(format!("192.168.1.7:{port}").parse().unwrap()));
        rec.set_model("m", "/m/a.gguf");
        rec.set_launch("L2");
        // Setters stay private to the record until the row is published.
        rec.publish();
        rec
      })
      .collect();

    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!((s.requests, s.hosts, s.connections), (3, Some(2), Some(5)));
    assert_eq!((s.live_hosts, s.live_connections), (Some(1), Some(2)));

    // A launch scope sees its own clients only: L1 never met the LAN host.
    let l1 = log.launch_summary("L1");
    assert_eq!(
      (l1.hosts, l1.connections, l1.live_hosts, l1.live_connections),
      (Some(1), Some(2), Some(0), Some(0))
    );

    // An IPv6 loopback is its own host, brackets and all.
    served_from(&log, "/m/b.gguf", "L3", "[::1]:5001");
    let all = log.tail(LogScope::All, 0).summary;
    assert_eq!((all.hosts, all.connections), (Some(3), Some(6)));
    drop(streaming);
  }

  #[test]
  fn an_empty_scope_answers_zero_where_an_old_daemon_answers_nothing() {
    let log = RequestLog::new();
    let idle = log.tail(
      LogScope::Model {
        path: "/m/none.gguf".to_string(),
      },
      10,
    );
    assert_eq!(idle.summary, zeros());
    assert_eq!(log.launch_summary("L9"), zeros());
    // With nothing in flight the live pair is not displayed at all.
    assert!(!idle
      .summary
      .cells("—")
      .iter()
      .any(|(label, _)| *label == "Live"));
  }

  #[test]
  fn a_summary_from_an_older_daemon_leaves_the_client_figures_out() {
    // The wire shape grew; a summary written before it grew must not read as
    // "zero hosts", and must not gain a Live cell.
    let old: RequestSummary = serde_json::from_str(r#"{"requests": 2}"#).unwrap();
    assert_eq!((old.hosts, old.connections), (None, None));
    assert_eq!((old.live_hosts, old.live_connections), (None, None));
    let cells = old.cells("—");
    assert_eq!(cells[2], ("Hosts", "—".to_string()));
    assert!(!cells.iter().any(|(label, _)| *label == "Live"));
  }

  #[test]
  fn the_live_pair_reads_as_one_number_when_hosts_and_peers_agree() {
    let cells = |hosts, connections| {
      RequestSummary {
        live_hosts: Some(hosts),
        live_connections: Some(connections),
        ..RequestSummary::default()
      }
      .cells("—")
    };
    assert_eq!(
      cells(2, 2).iter().find(|c| c.0 == "Live").unwrap().1,
      "2".to_string()
    );
    assert_eq!(
      cells(1, 2).iter().find(|c| c.0 == "Live").unwrap().1,
      "1/2".to_string()
    );
    assert!(!cells(0, 0).iter().any(|c| c.0 == "Live"));
  }

  #[test]
  fn a_dropped_record_is_logged_as_client_closed() {
    let log = RequestLog::new();
    drop(log.begin(ROUTE, None));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows[0].state, RequestState::ClientClosed);
    assert_eq!(tail.rows[0].status, None);
    assert_eq!(tail.summary.errors, 0);
  }

  #[test]
  fn respond_records_the_proxy_error() {
    let log = RequestLog::new();
    let rec = log.begin(ROUTE, None);
    rec.respond(503, Some("launch_failed"), Some("out of memory"));
    let tail = log.tail(LogScope::All, 10);
    assert_eq!(tail.rows[0].status, Some(503));
    assert_eq!(tail.rows[0].error.as_deref(), Some("launch_failed"));
    assert_eq!(tail.rows[0].cause.as_deref(), Some("out of memory"));
    assert_eq!(tail.summary.errors, 1);
  }

  #[test]
  fn tail_filters_by_model_path_and_returns_newest_first() {
    let log = RequestLog::new();
    let a1 = served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/b.gguf", "L2", Usage::default());
    let a2 = served(&log, "/m/a.gguf", "L1", Usage::default());

    let tail = log.tail(
      LogScope::Model {
        path: "/m/a.gguf".to_string(),
      },
      10,
    );
    assert_eq!(
      tail.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![a2, a1]
    );
    assert_eq!(tail.summary.requests, 2);
    assert_eq!(log.tail(LogScope::All, 10).summary.requests, 3);
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          1
        )
        .rows
        .len(),
      1
    );
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/none.gguf".to_string()
          },
          10
        )
        .summary,
      zeros(),
      "a scope with no rows has no clients, not unknown ones"
    );
  }

  #[test]
  fn summary_averages_only_successful_requests() {
    let log = RequestLog::new();
    let usage = |tps| Usage {
      prompt_tokens: Some(10),
      completion_tokens: Some(20),
      tokens_per_second: Some(tps),
      ..Usage::default()
    };
    served(&log, "/m/a.gguf", "L1", usage(40.0));
    served(&log, "/m/a.gguf", "L1", usage(60.0));
    let mut failed = log.begin(ROUTE, None);
    failed.set_model("m", "/m/a.gguf");
    failed.respond(503, Some("launch_failed"), None);

    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!(s.requests, 3);
    assert_eq!(s.errors, 1);
    assert_eq!(s.prompt_tokens, 20);
    assert_eq!(s.completion_tokens, 40);
    assert_eq!(s.tokens_per_second_avg, Some(50.0));
    assert_eq!(s.tokens_per_second_last, Some(60.0));
    assert!(s.avg_duration_ms.is_some());
    assert!(s.avg_ttfb_ms.is_some());

    // The 503 never reached a launch, so the launch's totals leave it out.
    let l = log.launch_summary("L1");
    assert_eq!((l.requests, l.errors), (2, 0));
    assert_eq!(log.launch_summary("L9"), zeros());
  }

  #[test]
  fn summary_counts_auto_starts_and_evictions() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_model("m", "/m/a.gguf");
    rec.set_auto_start();
    rec.set_evicted(vec!["L3".to_string(), "L4".to_string()]);
    rec.set_status(200);
    rec.finish(RequestState::Done, Usage::default());
    let s = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        0,
      )
      .summary;
    assert_eq!((s.auto_starts, s.evictions), (1, 2));
  }

  #[test]
  fn ring_is_bounded_and_totals_outlive_the_rows() {
    let log = RequestLog::new();
    for _ in 0..CAPACITY + 5 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    let tail = log.tail(LogScope::All, usize::MAX);
    assert_eq!(tail.rows.len(), CAPACITY);
    assert_eq!(tail.rows[0].seq, (CAPACITY + 5) as u64);
    assert_eq!(tail.summary.requests, (CAPACITY + 5) as u64);
  }

  #[test]
  fn finishing_a_row_pushed_out_of_the_ring_still_counts() {
    let log = RequestLog::new();
    let mut slow = log.begin(ROUTE, None);
    slow.set_model("m", "/m/slow.gguf");
    for _ in 0..CAPACITY {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    slow.set_status(200);
    slow.finish(RequestState::Done, Usage::default());
    // The newest row is untouched by the late finish.
    assert_eq!(
      log.tail(LogScope::All, 1).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/slow.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      1
    );
  }

  #[test]
  fn client_strings_are_cut_and_cleaned_before_they_are_stored() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_requested_model(Some(&"é".repeat(MAX_MODEL_CHARS + 50)));
    let long_cause = "x".repeat(MAX_CAUSE_CHARS + 50);
    rec.respond(503, Some("launch_failed"), Some(&long_cause));
    let row = log.tail(LogScope::All, 1).rows.remove(0);
    assert_eq!(
      row.requested_model.unwrap().chars().count(),
      MAX_MODEL_CHARS
    );
    assert_eq!(row.cause.unwrap().len(), MAX_CAUSE_CHARS);

    let mut hostile = log.begin(ROUTE, None);
    hostile.set_requested_model(Some("evil\x1b[2Jmodel\nname\ttab"));
    hostile.respond(503, Some("launch_failed"), Some("line one\nline two"));
    let cleaned = log.tail(LogScope::All, 1).rows.remove(0);
    assert_eq!(
      cleaned.requested_model.as_deref(),
      Some("evil [2Jmodel name tab")
    );
    assert_eq!(cleaned.cause.as_deref(), Some("line one line two"));
  }

  #[test]
  fn note_describes_what_happened_beyond_the_status() {
    let base = RequestRow {
      state: RequestState::Done,
      status: Some(200),
      ..RequestRow::default()
    };
    assert_eq!(base.note(), "");
    // Waiting on a load, whoever started it: the model is known and no
    // launch has the request yet.
    let loading = RequestRow {
      state: RequestState::InFlight,
      model_path: Some("/m/a.gguf".to_string()),
      ..RequestRow::default()
    };
    assert_eq!(loading.note(), "loading model");
    // A launch has it. No status yet on a non-streamed request, which
    // only arrives with the whole response.
    let generating = RequestRow {
      launch_id: Some("L1".to_string()),
      ..loading.clone()
    };
    assert_eq!(generating.note(), "in flight");
    // Still reading the request body: no model yet.
    assert_eq!(RequestRow::default().note(), "in flight");
    let made_room = RequestRow {
      auto_start: true,
      evicted: vec!["L3".to_string(), "L4".to_string()],
      ..base.clone()
    };
    assert_eq!(made_room.note(), "auto-start, unloaded L3 L4");
    let fallback = RequestRow {
      auto_start: true,
      fallback: Some("launch_failed".to_string()),
      requested_model: Some("big".to_string()),
      ..base.clone()
    };
    assert_eq!(
      fallback.note(),
      "auto-start, fallback (launch_failed) for big"
    );
    let failed = RequestRow {
      status: Some(503),
      auto_start: true,
      error: Some("launch_failed".to_string()),
      cause: Some("out of memory".to_string()),
      ..base.clone()
    };
    assert_eq!(failed.note(), "auto-start, launch_failed: out of memory");
    let closed = RequestRow {
      state: RequestState::ClientClosed,
      ..RequestRow::default()
    };
    assert_eq!(closed.note(), "client closed");
  }

  #[test]
  fn the_time_cell_is_the_local_clock_time_the_request_started() {
    // 22:13:20 UTC. A zone's offset is a whole number of quarter hours,
    // so in any zone the minute is 13, 28, 43 or 58 and the second is 20.
    let row = RequestRow {
      started_at_ms: 1_700_000_000_000,
      ..RequestRow::default()
    };
    let time = row.cells("-").time;
    let parts: Vec<u32> = time
      .split(':')
      .map(|part| part.parse().expect("digits"))
      .collect();
    assert_eq!(time.len(), 8, "{time}");
    assert_eq!(parts.len(), 3, "{time}");
    assert!(parts[0] < 24, "{time}");
    assert_eq!(parts[1] % 15, 13, "{time}");
    assert_eq!(parts[2], 20, "{time}");
  }

  #[test]
  fn cells_format_each_field_and_fall_back_to_the_placeholder() {
    let row = RequestRow {
      route: ROUTE.to_string(),
      requested_model: Some("qwen".to_string()),
      model: Some("Qwen3.8-27B".to_string()),
      launch_id: Some("L1".to_string()),
      client: Some("127.0.0.1:5000".to_string()),
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(180),
      duration_ms: Some(1_240),
      prompt_tokens: Some(41),
      completion_tokens: Some(12_345),
      tokens_per_second: Some(39.64),
      ..RequestRow::default()
    };
    let c = row.cells("-");
    assert_eq!(c.time.len(), 8);
    assert_eq!(
      [
        c.status,
        c.total,
        c.ttfb,
        c.speed,
        c.tokens_in,
        c.tokens_out,
        c.route,
        c.model,
        c.launch,
        c.client,
        c.note
      ],
      [
        "200",
        "1.2s",
        "180ms",
        "39.6",
        "41",
        "12.3k",
        "chat/completions",
        "Qwen3.8-27B",
        "L1",
        "127.0.0.1:5000",
        ""
      ]
    );
    let estimated = RequestRow {
      tokens_per_second_estimated: true,
      ..row.clone()
    };
    assert_eq!(estimated.cells("-").speed, "~39.6");
    // Nothing served it: the model asked for stands in, the rest is blank.
    let pending = RequestRow {
      requested_model: Some("qwen".to_string()),
      ..RequestRow::default()
    };
    let p = pending.cells("-");
    assert_eq!(p.model, "qwen");
    assert_eq!(
      [
        p.status,
        p.total,
        p.ttfb,
        p.speed,
        p.tokens_in,
        p.tokens_out,
        p.launch,
        p.client
      ],
      ["-"; 8]
    );
  }

  #[test]
  fn durations_and_counts_pick_a_unit_by_size() {
    assert_eq!(fmt_ms(0), "0ms");
    assert_eq!(fmt_ms(850), "850ms");
    assert_eq!(fmt_ms(1_240), "1.2s");
    assert_eq!(fmt_ms(59_900), "59.9s");
    assert_eq!(fmt_ms(125_000), "2m05s");
    assert_eq!(fmt_ms(3_720_000), "1h02m");
    assert_eq!(fmt_count(0), "0");
    assert_eq!(fmt_count(9_999), "9999");
    assert_eq!(fmt_count(12_345), "12.3k");
    assert_eq!(fmt_count(999_949), "999.9k");
    assert_eq!(fmt_count(999_950), "1.0M");
    assert_eq!(fmt_count(999_999), "1.0M");
    assert_eq!(fmt_count(1_234_567), "1.2M");
  }

  #[test]
  fn summary_cells_use_the_placeholder_for_missing_figures() {
    let cells = RequestSummary::default().cells("—");
    assert_eq!(cells[0], ("Requests", "0".to_string()));
    assert_eq!(cells[2], ("Hosts", "—".to_string()));
    assert_eq!(cells[4], ("Avg total", "—".to_string()));
    assert_eq!(cells[6], ("Avg tok/s", "—".to_string()));
    let filled = RequestSummary {
      avg_duration_ms: Some(1_240),
      tokens_per_second_avg: Some(39.64),
      prompt_tokens: 12_345,
      ..RequestSummary::default()
    };
    let filled_cells = filled.cells("—");
    assert_eq!(filled_cells[4].1, "1.2s");
    assert_eq!(filled_cells[6].1, "39.6");
    assert_eq!(filled_cells[7].1, "12.3k");
    // The session speed is the newest one a stream reported, and the only
    // speed on the strip besides the average.
    assert!(!filled_cells.iter().any(|c| c.0 == "Tok/s last"));
  }

  #[tokio::test]
  async fn finished_rows_are_appended_to_the_file_as_json_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("logs").join(FILE_NAME);
    let log = RequestLog::new();
    log.write_to(path.clone());

    let first = served(&log, "/m/a.gguf", "L1", Usage::default());
    let _in_flight = log.begin(ROUTE, None);
    let failed = log.begin(ROUTE, None);
    failed.respond(404, Some("model_not_found"), Some("nope not found"));

    // The writer runs on its own task; wait for both lines to land.
    let mut text = String::new();
    for _ in 0..200 {
      text = std::fs::read_to_string(&path).unwrap_or_default();
      if text.lines().count() >= 2 {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows: Vec<RequestRow> = text
      .lines()
      .map(|line| serde_json::from_str(line).expect("one JSON row per line"))
      .collect();
    // Finished rows only, in the order they finished. The in-flight row
    // (`seq` 2) is not written.
    assert_eq!(
      rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![first, 3]
    );
    assert_eq!(rows[0].model_path.as_deref(), Some("/m/a.gguf"));
    assert_eq!(rows[1].error.as_deref(), Some("model_not_found"));
    // The in-memory log is unchanged by the file.
    assert_eq!(log.tail(LogScope::All, 10).rows.len(), 3);
    assert_eq!(log.file_status(), Some(FileStatus { path, dropped: 0 }));
    assert_eq!(RequestLog::new().file_status(), None, "no file asked for");
  }

  #[tokio::test]
  async fn rows_that_cannot_be_written_are_counted_not_lost_silently() {
    // The path's parent is a regular file, so the log file cannot be
    // created.
    let dir = tempfile::tempdir().expect("tempdir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"").expect("write blocker");
    let log = RequestLog::new();
    log.write_to(blocker.join(FILE_NAME));

    for _ in 0..3 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    // A row finished before the writer gave up sits in the queue until
    // the writer drains it; one finished after is counted at once.
    let mut dropped = 0;
    for _ in 0..200 {
      dropped = log.file_status().expect("file status").dropped;
      if dropped == 3 {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(dropped, 3);
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 4);
    // The in-memory log has all four.
    assert_eq!(log.tail(LogScope::All, 10).summary.requests, 4);
  }

  /// `/dev/full` opens and then fails every write with `ENOSPC`.
  #[cfg(target_os = "linux")]
  #[tokio::test]
  async fn a_write_failure_stops_the_writer_and_later_rows_are_counted() {
    let log = RequestLog::new();
    log.write_to(PathBuf::from("/dev/full"));
    // The writer gives up on the first row it cannot write. Rows keep
    // being finished meanwhile; none of them reaches a file.
    let dropped = || log.file_status().expect("file status").dropped;
    let mut finished = 0;
    for _ in 0..200 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
      finished += 1;
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
      if dropped() > 0 {
        break;
      }
    }
    assert!(dropped() > 0, "the writer never stopped");
    served(&log, "/m/a.gguf", "L1", Usage::default());
    finished += 1;
    // Every row is counted: the one whose write failed, the ones queued
    // behind it, and the ones finished after the writer stopped.
    for _ in 0..200 {
      if dropped() == finished {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(dropped(), finished);
  }

  #[test]
  fn a_full_queue_drops_the_line_and_counts_it() {
    // No runtime here, so nothing drains the queue: fill it by hand.
    let log = RequestLog::new();
    let (queue, _rx) = mpsc::channel::<String>(1);
    let sink = FileSink {
      path: PathBuf::from("/unused"),
      queue,
      dropped: AtomicU64::new(0),
    };
    assert!(log.file.set(sink).is_ok());
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 0);
    served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/a.gguf", "L1", Usage::default());
    assert_eq!(log.file_status().expect("file status").dropped, 2);
  }

  #[test]
  fn a_stopped_launch_keeps_its_totals_and_a_late_finish_does_not_bring_it_back() {
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", Usage::default());
    // Still streaming when the launch is stopped.
    let mut late = log.begin(ROUTE, None);
    late.set_model("m", "/m/a.gguf");
    late.set_launch("L1");
    late.set_status(200);
    assert_eq!(log.launch_summary("L1").requests, 1);

    log.forget_launch("L1");
    assert_eq!(log.launch_summary("L1").requests, 1);
    assert_eq!(
      log.lock().by_launch.len(),
      0,
      "the live entry is retired, not left behind"
    );
    late.finish(RequestState::UpstreamError, Usage::default());
    assert_eq!(log.lock().by_launch.len(), 0);
    // The model's own totals keep both requests.
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      2
    );
  }

  /// `launch_summary` answers with `tail`'s summary for the same launch scope,
  /// so a figure `tail` stamps cannot skip the per-launch surfaces.
  #[test]
  fn a_launch_summary_equals_the_launch_scope_tail_summary() {
    let log = RequestLog::new();
    let scope = |id: &str| LogScope::Launch { id: id.to_string() };
    served_from(&log, "/m/a.gguf", "L1", "192.168.1.7:5001");
    served(&log, "/m/b.gguf", "L1", Usage::default());
    assert_eq!(log.launch_summary("L1"), log.tail(scope("L1"), 0).summary);

    log.forget_launch("L1");
    assert_eq!(log.launch_summary("L1"), log.tail(scope("L1"), 0).summary);
    // A launch the log never heard of answers the same way on both paths.
    assert_eq!(log.launch_summary("L9"), log.tail(scope("L9"), 0).summary);
  }

  #[test]
  fn retirement_keeps_the_last_sixteen_stopped_launches_oldest_first() {
    let log = RequestLog::new();
    let ids: Vec<String> = (1..=17).map(|n| format!("L{n}")).collect();
    for id in &ids {
      served(&log, "/m/a.gguf", id, Usage::default());
    }
    for id in &ids {
      log.forget_launch(id);
    }
    assert_eq!(
      log.launch_summary("L1"),
      zeros(),
      "the oldest of seventeen is pushed out"
    );
    for id in &ids[1..] {
      assert_eq!(log.launch_summary(id).requests, 1, "{id} is retired");
    }
  }

  #[test]
  fn a_model_scope_reports_the_newest_launch_speed_as_the_session() {
    let speed = |t: f64| Usage {
      completion_tokens: Some(10),
      tokens_per_second: Some(t),
      ..Usage::default()
    };
    let log = RequestLog::new();
    served(&log, "/m/a.gguf", "L1", speed(80.0));
    served(&log, "/m/a.gguf", "L1", speed(60.0));
    served(&log, "/m/a.gguf", "L2", speed(20.0));

    let model = log
      .tail(
        LogScope::Model {
          path: "/m/a.gguf".to_string(),
        },
        10,
      )
      .summary;
    assert_eq!(
      model.tokens_per_second_avg,
      Some((80.0 + 60.0 + 20.0) / 3.0)
    );
    // The newest launch of the model runs at 20, and that is the session.
    assert_eq!(model.tokens_per_second_session, Some(20.0));
    // The whole log has no single session to name, and a launch scope is
    // already one launch, so neither gets the extra figure.
    assert_eq!(
      log
        .tail(LogScope::All, 10)
        .summary
        .tokens_per_second_session,
      None
    );
    assert_eq!(
      log
        .tail(
          LogScope::Launch {
            id: "L1".to_string()
          },
          10,
        )
        .summary
        .tokens_per_second_session,
      None
    );
    // The strip shows the figure only when there is one, so an old daemon
    // (no key in the JSON) renders exactly as before.
    let with = model.cells("-");
    assert!(with.iter().any(|(k, _)| *k == "Tok/s session"));
    assert_eq!(
      with[with
        .iter()
        .position(|(k, _)| *k == "Tok/s session")
        .unwrap()]
      .1,
      "20.0"
    );
    assert!(!RequestSummary::default()
      .cells("-")
      .iter()
      .any(|(k, _)| *k == "Tok/s session"));
  }

  #[test]
  fn a_launch_scope_sees_one_launch_of_a_model() {
    let log = RequestLog::new();
    let a1 = served(&log, "/m/a.gguf", "L1", Usage::default());
    served(&log, "/m/a.gguf", "L2", Usage::default());
    let a3 = served(&log, "/m/a.gguf", "L1", Usage::default());

    let scoped = log.tail(
      LogScope::Launch {
        id: "L1".to_string(),
      },
      10,
    );
    assert_eq!(
      scoped.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![a3, a1]
    );
    assert_eq!(scoped.summary.requests, 2);
    // The same model under another launch is the wider scope.
    assert_eq!(
      log
        .tail(
          LogScope::Model {
            path: "/m/a.gguf".to_string()
          },
          0
        )
        .summary
        .requests,
      3
    );
    // A launch's series carries the model's own name, not the launch id.
    assert_eq!(log.launch_label("L1").as_deref(), Some("m"));
    assert_eq!(
      log
        .tail(
          LogScope::Launch {
            id: "L9".to_string()
          },
          10
        )
        .summary,
      zeros(),
      "a launch that served nothing is an empty scope"
    );
  }

  #[test]
  fn a_retired_launch_still_scopes_rows_and_totals() {
    let log = RequestLog::new();
    let seq = served(&log, "/m/a.gguf", "L1", Usage::default());
    log.forget_launch("L1");

    let scoped = log.tail(
      LogScope::Launch {
        id: "L1".to_string(),
      },
      10,
    );
    assert_eq!(
      scoped.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![seq]
    );
    assert_eq!(scoped.summary.requests, 1);
    assert_eq!(log.launch_label("L1").as_deref(), Some("m"));
  }

  #[test]
  fn reading_tolerates_missing_keys() {
    let row: RequestRow = serde_json::from_str(r#"{"seq": 4, "status": 200}"#).unwrap();
    assert_eq!((row.seq, row.status), (4, Some(200)));
    assert_eq!(row.state, RequestState::InFlight);
    let summary: RequestSummary = serde_json::from_str(r#"{"requests": 2}"#).unwrap();
    assert_eq!(summary.requests, 2);
    assert_eq!(summary.avg_duration_ms, None);
  }

  #[test]
  fn row_wire_shape_keeps_every_key() {
    let log = RequestLog::new();
    drop(log.begin(ROUTE, None));
    let v = serde_json::to_value(&log.tail(LogScope::All, 1).rows[0]).unwrap();
    for key in [
      "seq",
      "started_at_ms",
      "client",
      "route",
      "requested_model",
      "model",
      "model_path",
      "launch_id",
      "state",
      "status",
      "error",
      "cause",
      "auto_start",
      "evicted",
      "fallback",
      "ttfb_ms",
      "duration_ms",
      "prompt_tokens",
      "completion_tokens",
      "tokens_per_second",
      "tokens_per_second_estimated",
    ] {
      assert!(v.get(key).is_some(), "missing key {key}");
    }
    assert_eq!(v["state"], "client_closed");
  }
}
