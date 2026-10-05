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

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::usage_tap::Usage;

/// Rows kept. At roughly 400 bytes a row this is well under 1 MiB.
pub const CAPACITY: usize = 1000;

/// Rows one `requests_tail` call returns when it names no `limit`.
pub const DEFAULT_TAIL: usize = 100;

/// Name of the request log file inside the daemon's log directory.
pub const FILE_NAME: &str = "requests.jsonl";

/// Finished rows that may wait for the file writer. Past this the line is
/// dropped from the file (the in-memory log still has the row) rather
/// than holding up the request that produced it.
const FILE_QUEUE: usize = 1024;

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
  /// This request started the model: nothing was serving or loading it.
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
  if n < 10_000 {
    n.to_string()
  } else if n < 1_000_000 {
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
  /// The most recent speed a request reported.
  pub tokens_per_second_last: Option<f64>,
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
    vec![
      ("Requests", self.requests.to_string()),
      ("Errors", self.errors.to_string()),
      ("Avg total", ms(self.avg_duration_ms)),
      ("Avg TTFB", ms(self.avg_ttfb_ms)),
      ("Tok/s avg", tps(self.tokens_per_second_avg)),
      ("Tok/s last", tps(self.tokens_per_second_last)),
      ("Tokens in", fmt_count(self.prompt_tokens)),
      ("Tokens out", fmt_count(self.completion_tokens)),
      ("Auto-starts", self.auto_starts.to_string()),
      ("Unloaded", self.evictions.to_string()),
    ]
  }
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
}

impl Totals {
  fn add(&mut self, row: &RequestRow) {
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
      tokens_per_second_last: self.tps_last,
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
  // One entry per model that got a request and per launch that served
  // one. Neither is ever removed: a model path comes from the catalog and
  // a launch takes seconds to create, so the maps stay small.
  by_model: HashMap<String, Totals>,
  by_launch: HashMap<String, Totals>,
}

impl Inner {
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

/// What `requests_tail` returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
  pub summary: RequestSummary,
  /// Newest first.
  pub rows: Vec<RequestRow>,
}

/// Shared handle to the log. Cheap to clone.
#[derive(Clone, Default)]
pub struct RequestLog {
  inner: Arc<Mutex<Inner>>,
  /// Queue to the file writer, once [`Self::write_to`] has started one.
  file: Arc<OnceLock<mpsc::Sender<String>>>,
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
  pub fn write_to(&self, path: PathBuf) {
    let (tx, mut rx) = mpsc::channel::<String>(FILE_QUEUE);
    if self.file.set(tx).is_err() {
      return;
    }
    tokio::spawn(async move {
      let mut writer = match crate::daemon::supervisor::LogWriter::open(path.clone()).await {
        Ok(writer) => writer,
        Err(e) => {
          log::warn!("proxy: cannot open request log {}: {e}", path.display());
          return;
        }
      };
      while let Some(line) = rx.recv().await {
        if let Err(e) = writer.write_line(line.as_bytes()).await {
          log::warn!("proxy: request log write to {} failed: {e}", path.display());
        }
      }
    });
  }

  /// Start a row for a request the proxy just received. The returned
  /// record finishes the row when it is dropped, so every exit path is
  /// logged, including a client that hangs up while a model loads.
  pub fn begin(&self, route: &str, client: Option<SocketAddr>) -> RequestRecord {
    let started_at_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_millis() as u64)
      .unwrap_or(0);
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

  /// The newest `limit` rows, newest first, and the summary for the same
  /// scope: one model when `model_path` is given, every request otherwise.
  pub fn tail(&self, model_path: Option<&str>, limit: usize) -> Tail {
    let inner = self.lock();
    let totals = match model_path {
      Some(path) => inner.by_model.get(path),
      None => Some(&inner.all),
    };
    Tail {
      summary: totals.map(Totals::summary).unwrap_or_default(),
      rows: inner
        .rows
        .iter()
        .rev()
        .filter(|r| model_path.is_none_or(|p| r.model_path.as_deref() == Some(p)))
        .take(limit)
        .cloned()
        .collect(),
    }
  }

  /// Summary of the requests one launch served.
  pub fn launch_summary(&self, launch_id: &str) -> RequestSummary {
    self
      .lock()
      .by_launch
      .get(launch_id)
      .map(Totals::summary)
      .unwrap_or_default()
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

  pub fn set_launch(&mut self, launch_id: &str) {
    self.row.launch_id = Some(launch_id.to_string());
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
  pub fn publish(&self) {
    self.log.lock().store(&self.row);
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
      if let Some(path) = &self.row.model_path {
        inner
          .by_model
          .entry(path.clone())
          .or_default()
          .add(&self.row);
      }
      if let Some(launch) = &self.row.launch_id {
        inner
          .by_launch
          .entry(launch.clone())
          .or_default()
          .add(&self.row);
      }
    }
    if let Some(file) = self.log.file.get() {
      if let Ok(line) = serde_json::to_string(&self.row) {
        let _ = file.try_send(line);
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

  #[test]
  fn begin_shows_an_in_flight_row_and_finish_completes_it() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, Some("127.0.0.1:5000".parse().unwrap()));
    let tail = log.tail(None, 10);
    assert_eq!(tail.rows.len(), 1);
    assert_eq!(tail.rows[0].state, RequestState::InFlight);
    assert_eq!(tail.rows[0].client.as_deref(), Some("127.0.0.1:5000"));
    assert_eq!(tail.summary.requests, 0, "the summary counts finished rows");

    rec.set_status(200);
    rec.finish(RequestState::Done, Usage::default());
    let done = log.tail(None, 10);
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
    assert_eq!(log.tail(None, 10).rows[0].model_path, None);
    rec.publish();
    assert_eq!(
      log.tail(None, 10).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
  }

  #[test]
  fn a_dropped_record_is_logged_as_client_closed() {
    let log = RequestLog::new();
    drop(log.begin(ROUTE, None));
    let tail = log.tail(None, 10);
    assert_eq!(tail.rows[0].state, RequestState::ClientClosed);
    assert_eq!(tail.rows[0].status, None);
    assert_eq!(tail.summary.errors, 0);
  }

  #[test]
  fn respond_records_the_proxy_error() {
    let log = RequestLog::new();
    let rec = log.begin(ROUTE, None);
    rec.respond(503, Some("launch_failed"), Some("out of memory"));
    let tail = log.tail(None, 10);
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

    let tail = log.tail(Some("/m/a.gguf"), 10);
    assert_eq!(
      tail.rows.iter().map(|r| r.seq).collect::<Vec<_>>(),
      vec![a2, a1]
    );
    assert_eq!(tail.summary.requests, 2);
    assert_eq!(log.tail(None, 10).summary.requests, 3);
    assert_eq!(log.tail(Some("/m/a.gguf"), 1).rows.len(), 1);
    assert_eq!(log.tail(Some("/m/none.gguf"), 10), Tail::default());
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

    let s = log.tail(Some("/m/a.gguf"), 0).summary;
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
    assert_eq!(log.launch_summary("L9"), RequestSummary::default());
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
    let s = log.tail(Some("/m/a.gguf"), 0).summary;
    assert_eq!((s.auto_starts, s.evictions), (1, 2));
  }

  #[test]
  fn ring_is_bounded_and_totals_outlive_the_rows() {
    let log = RequestLog::new();
    for _ in 0..CAPACITY + 5 {
      served(&log, "/m/a.gguf", "L1", Usage::default());
    }
    let tail = log.tail(None, usize::MAX);
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
      log.tail(None, 1).rows[0].model_path.as_deref(),
      Some("/m/a.gguf")
    );
    assert_eq!(log.tail(Some("/m/slow.gguf"), 0).summary.requests, 1);
  }

  #[test]
  fn client_strings_are_cut_and_cleaned_before_they_are_stored() {
    let log = RequestLog::new();
    let mut rec = log.begin(ROUTE, None);
    rec.set_requested_model(Some(&"é".repeat(MAX_MODEL_CHARS + 50)));
    let long_cause = "x".repeat(MAX_CAUSE_CHARS + 50);
    rec.respond(503, Some("launch_failed"), Some(&long_cause));
    let row = log.tail(None, 1).rows.remove(0);
    assert_eq!(
      row.requested_model.unwrap().chars().count(),
      MAX_MODEL_CHARS
    );
    assert_eq!(row.cause.unwrap().len(), MAX_CAUSE_CHARS);

    let mut hostile = log.begin(ROUTE, None);
    hostile.set_requested_model(Some("evil\x1b[2Jmodel\nname\ttab"));
    hostile.respond(503, Some("launch_failed"), Some("line one\nline two"));
    let cleaned = log.tail(None, 1).rows.remove(0);
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
    assert_eq!(fmt_count(1_234_567), "1.2M");
  }

  #[test]
  fn summary_cells_use_the_placeholder_for_missing_figures() {
    let cells = RequestSummary::default().cells("—");
    assert_eq!(cells[0], ("Requests", "0".to_string()));
    assert_eq!(cells[2], ("Avg total", "—".to_string()));
    assert_eq!(cells[4], ("Tok/s avg", "—".to_string()));
    let filled = RequestSummary {
      avg_duration_ms: Some(1_240),
      tokens_per_second_avg: Some(39.64),
      prompt_tokens: 12_345,
      ..RequestSummary::default()
    };
    let filled_cells = filled.cells("—");
    assert_eq!(filled_cells[2].1, "1.2s");
    assert_eq!(filled_cells[4].1, "39.6");
    assert_eq!(filled_cells[6].1, "12.3k");
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
    assert_eq!(log.tail(None, 10).rows.len(), 3);
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
    let v = serde_json::to_value(&log.tail(None, 1).rows[0]).unwrap();
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
