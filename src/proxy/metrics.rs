//! `GET /metrics`: the proxy's own request numbers, as Prometheus text or as
//! JSON.
//!
//! Every number comes from the in-memory request log
//! ([`super::request_log`]), so counters count since daemon start and nothing
//! is persisted across a restart — `requests.jsonl` stays the durable record.
//! The route never contacts a backend; the engines' own numbers are the
//! separate [`super::metrics_backend`] route.
//!
//! Metric names are `llamastash:*`, one prefix of our own beside whatever the
//! engines behind us export; theirs come through the backend route untouched.
//! There are no `gen_ai.*` aliases, no histograms, and one label set per metric name. `model` and
//! `route` are the only labels a default scrape carries; the per-launch series
//! are opt-in with `?launch=1` because launch ids are never reused.
//!
//! Plan: docs/plans/2026-10-11-001-feat-proxy-metrics-plan.md.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Request, Response, StatusCode};
use serde_json::{Map, Value};

use super::request_log::{
  LogScope, LogSnapshot, RequestLog, RequestSummary, ScopeTotals, CAPACITY, DEFAULT_TAIL,
};
use super::route;
use super::router::{self, ProxyResponse};
use super::state::ProxyState;
use crate::daemon::supervisor::ManagedState;

/// The media type a Prometheus scrape expects for text format 0.0.4.
pub(crate) const TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Which body a metrics route answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
  Text,
  Json,
}

/// A metrics request the proxy refuses, in the shape each route turns into its
/// own OpenAI-shaped error envelope.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reject {
  /// `?format=` named something other than the two accepted values.
  BadFormat(String),
  /// The `Accept` header matched neither candidate type.
  NotAcceptable(String),
  /// `?model=` / `?launch=` named no model and no launch.
  UnknownRef(String),
  /// `?model=` and `?launch=` name a model and a launch that does not serve it.
  ConflictingScope { model: String, launch: String },
  /// The `Accept` header matched both candidates equally and cannot be settled.
  Ambiguous(String),
}

impl Reject {
  pub(crate) fn into_response(self) -> ProxyResponse {
    match self {
      Reject::BadFormat(v) => router::error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("unknown format `{v}`; use `prometheus` or `json`"),
      ),
      Reject::NotAcceptable(v) => router::error_response(
        StatusCode::NOT_ACCEPTABLE,
        "not_acceptable",
        &format!(
          "Accept `{v}` matches neither text/plain nor application/json; \
           send `Accept: application/json` for JSON or `?format=json`"
        ),
      ),
      Reject::UnknownRef(v) => router::error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        &format!("no model or launch matches `{v}`"),
      ),
      Reject::ConflictingScope { model, launch } => router::error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("launch `{launch}` does not serve model `{model}`"),
      ),
      Reject::Ambiguous(v) => router::error_response(
        StatusCode::BAD_REQUEST,
        "ambiguous_model",
        &format!("`{v}` matches more than one model; scope it with a launch or a full path"),
      ),
    }
  }
}

/// Query parameters both metrics routes read.
#[derive(Debug, Default)]
pub(crate) struct Query {
  /// `?format=json` / `?format=prometheus`.
  pub(crate) format: Option<String>,
  /// `?limit=` for the JSON row list.
  pub(crate) limit: Option<usize>,
  /// `?model=<ref>`.
  pub(crate) model: Option<String>,
  /// `?launch=<ref>`, or `?launch=1` for the per-launch series.
  pub(crate) launch: Option<String>,
}

/// Values of `launch=` that mean "include the per-launch series" rather than
/// "scope to this launch". A launch id is `L<n>`, so `1` can never name one.
const PER_LAUNCH_FLAGS: [&str; 3] = ["1", "true", "yes"];

fn is_per_launch_flag(value: &str) -> bool {
  PER_LAUNCH_FLAGS.contains(&value)
}

impl Query {
  pub(crate) fn parse(query: Option<&str>) -> Self {
    let mut out = Query::default();
    for (key, value) in pairs(query) {
      match key.as_str() {
        "format" => out.format = Some(value),
        "limit" => out.limit = value.parse().ok(),
        "model" => out.model = Some(value),
        "launch" => out.launch = Some(value),
        _ => {}
      }
    }
    out
  }

  /// `?launch=1`: the caller asked for the per-launch series.
  pub(crate) fn per_launch(&self) -> bool {
    self.launch.as_deref().is_some_and(is_per_launch_flag)
  }

  /// The `launch=` value read as a launch reference, `None` for the flag form.
  fn launch_ref(&self) -> Option<&str> {
    self.launch.as_deref().filter(|v| !is_per_launch_flag(v))
  }

  /// Row limit for the JSON shape, clamped to what the log can hold.
  pub(crate) fn limit(&self) -> usize {
    self.limit.unwrap_or(DEFAULT_TAIL).min(CAPACITY)
  }
}

/// Query string as `(key, value)` pairs with percent-escapes and `+` undone. A
/// pair with no `=` has an empty value.
fn pairs(query: Option<&str>) -> Vec<(String, String)> {
  query
    .unwrap_or_default()
    .split('&')
    .filter(|pair| !pair.is_empty())
    .map(|pair| {
      let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
      (decode(key), decode(value))
    })
    .collect()
}

/// Percent-decode one query token: `%XX` runs (a multi-byte character is a run
/// of them), `+` for a space, `%2B` for a literal plus. A stray `%` that does
/// not start a hex pair is kept.
fn decode(raw: &str) -> String {
  let bytes = raw.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    let escaped = if bytes[i] == b'%' && i + 2 < bytes.len() {
      std::str::from_utf8(&bytes[i + 1..i + 3])
        .ok()
        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
    } else {
      None
    };
    match escaped {
      Some(byte) => {
        out.push(byte);
        i += 3;
      }
      None => {
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
      }
    }
  }
  // A decoded value can be arbitrary bytes; lossy keeps the route answering
  // rather than failing on a malformed escape.
  String::from_utf8_lossy(&out).into_owned()
}

/// Best `q` the two candidate types get from an `Accept` header, as
/// `(json, text)` where `0.0` means not accepted.
///
/// Only an exact `application/json` or `text/plain` selects its own format. A
/// `;` parameter that is not `q` (`version=0.0.4`, `charset=…`) does not affect
/// the match and a missing `q` is `1.0`. A wildcard — `*/*` or `application/*`
/// — means "whatever you have", which is the text default; only an exact type
/// asks for JSON, so a scraper can never receive JSON by accident.
fn accept_q(header: &str) -> (f32, f32) {
  let mut best = (0.0f32, 0.0f32);
  for entry in header.split(',') {
    let mut parts = entry.trim().split(';');
    let Some(syntax) = parts.next().map(|s| s.trim().to_ascii_lowercase()) else {
      continue;
    };
    let mut q = 1.0f32;
    for param in parts {
      let Some((name, value)) = param.split_once('=') else {
        continue;
      };
      if name.trim().eq_ignore_ascii_case("q") {
        q = value.trim().parse().unwrap_or(0.0);
      }
    }
    if q <= 0.0 {
      continue;
    }
    if syntax == "application/json" {
      best.0 = best.0.max(q);
    }
    if syntax == "text/plain" || syntax.ends_with("/*") {
      best.1 = best.1.max(q);
    }
  }
  best
}

/// The response format: `?format=` beats `Accept`, which beats the default of
/// Prometheus text. A wildcard must never select JSON, and a tie goes to text.
pub(crate) fn choose_format(query: Option<&str>, headers: &HeaderMap) -> Result<Format, Reject> {
  let q = Query::parse(query);
  if let Some(format) = &q.format {
    return match format.as_str() {
      "json" => Ok(Format::Json),
      "prometheus" => Ok(Format::Text),
      other => Err(Reject::BadFormat(other.to_string())),
    };
  }
  let Some(accept) = headers
    .get(hyper::header::ACCEPT)
    .and_then(|v| v.to_str().ok())
  else {
    return Ok(Format::Text);
  };
  let (json, text) = accept_q(accept);
  if json == 0.0 && text == 0.0 {
    return Err(Reject::NotAcceptable(accept.to_string()));
  }
  Ok(if json > text {
    Format::Json
  } else {
    Format::Text
  })
}

/// What one scrape covers.
#[derive(Debug)]
pub(crate) struct Scope {
  /// The request-log scope to read the summary and rows from.
  pub(crate) log: LogScope,
  /// Restrict the per-model series to this catalog path.
  pub(crate) path: Option<String>,
  /// Restrict the per-launch series to this launch id.
  pub(crate) launch: Option<String>,
  /// Display name of the model the scope names, for the JSON `scope` block.
  pub(crate) name: Option<String>,
  /// The per-launch series are in the response.
  pub(crate) per_launch: bool,
}

impl Default for Scope {
  fn default() -> Self {
    Scope {
      log: LogScope::All,
      path: None,
      launch: None,
      name: None,
      per_launch: false,
    }
  }
}

impl Scope {
  /// Whether a per-model entry is in the view.
  pub(crate) fn keeps_model(&self, path: &str) -> bool {
    self.path.as_deref().is_none_or(|want| want == path)
  }

  /// Whether a launch is in the view, given the path it served (`None` when the
  /// scrape cannot tell).
  pub(crate) fn keeps_launch(&self, launch: &str, path: Option<&str>) -> bool {
    if let Some(want) = &self.launch {
      if want != launch {
        return false;
      }
    }
    match (&self.path, path) {
      (Some(want), Some(path)) => want == path,
      // A retired launch with no rows left in the ring cannot be matched to the
      // model, so a model-scoped scrape leaves it out rather than guessing.
      (Some(_), None) => false,
      (None, _) => true,
    }
  }
}

/// One live launch as a metrics scrape sees it.
#[derive(Clone)]
pub(crate) struct LiveLaunch {
  pub(crate) id: String,
  /// Display name of the model it serves.
  pub(crate) model: String,
  pub(crate) path: String,
  pub(crate) port: u16,
  pub(crate) ready: bool,
}

/// Every live launch that is a model, with its display name and port. The same
/// registry walk `/ui`'s chooser makes; infrastructure launches (a managed
/// multiplexer's umbrella) serve no model of their own and are left out.
pub(crate) async fn live_launches(state: &ProxyState) -> Vec<LiveLaunch> {
  let sup_snap = state.ctx.supervisors.snapshot().await;
  let cat_snap = state.ctx.catalog.shared_view().await;
  let by_path = route::index_catalog_by_path(&cat_snap);
  let mut out = Vec::new();
  for (launch_id, model) in sup_snap {
    if crate::backend::is_infra_launch(&launch_id) {
      continue;
    }
    let path = model.id().path.to_string_lossy().into_owned();
    let name = by_path
      .get(&path)
      .map(|m| {
        m.display_label
          .clone()
          .unwrap_or_else(|| crate::util::paths::model_display_name(&m.path))
      })
      .unwrap_or_else(|| crate::util::paths::model_display_name(&model.id().path));
    let ready = matches!(model.state().await, ManagedState::Ready);
    out.push(LiveLaunch {
      id: launch_id.as_str().to_string(),
      model: name,
      path,
      port: model.port(),
      ready,
    });
  }
  out.sort_by(|a, b| launch_order(&a.id).cmp(&launch_order(&b.id)));
  out
}

/// Sort key for a launch id: `L10` after `L2`.
pub(crate) fn launch_order(id: &str) -> (u8, u64, &str) {
  match id.strip_prefix('L').and_then(|n| n.parse::<u64>().ok()) {
    Some(n) => (0, n, id),
    None => (1, 0, id),
  }
}

/// Resolve `?model=` / `?launch=` into a [`Scope`], in the order the plan sets:
/// launch id, port, exact catalog path, then a case-insensitive display-name
/// match through the proxy's own reference resolver. `live` is the caller's
/// [`live_launches`] snapshot, so one request asks the registry once.
pub(crate) async fn resolve_scope(
  state: &ProxyState,
  query: Option<&str>,
  live: &[LiveLaunch],
) -> Result<Scope, Reject> {
  let q = Query::parse(query);
  let mut scope = Scope {
    log: LogScope::All,
    per_launch: q.per_launch(),
    ..Scope::default()
  };

  if let Some(value) = q.launch_ref() {
    let snapshot = state.ctx.requests.snapshot();
    let (id, path) = resolve_launch(state, &snapshot, live, value).await?;
    scope.launch = Some(id);
    scope.path = path;
    // Naming one launch is also asking for its series.
    scope.per_launch = true;
    scope.log = LogScope::Launch {
      id: scope.launch.clone().unwrap_or_default(),
    };
    scope.name = scope.path.as_deref().and_then(|wanted| {
      live
        .iter()
        .find(|l| l.path == wanted)
        .map(|l| l.model.clone())
        .or_else(|| {
          snapshot
            .models
            .iter()
            .find(|m| m.key == wanted)
            .and_then(|m| m.label.clone())
        })
    });
  }

  if let Some(value) = &q.model {
    let (path, name) = resolve_model(state, value).await.map_err(|_| {
      // A model reference that names nothing is a 404 naming the value; a
      // substring that matches several models is the proxy's usual ambiguity.
      Reject::UnknownRef(value.clone())
    })?;
    // A launch scope and a model scope have to agree. A `?launch=` whose model
    // is not the one `?model=` names has no honest answer, and a half-empty body
    // would read as idle traffic rather than as the contradiction it is.
    if let (Some(launch_value), Some(served)) = (q.launch_ref(), scope.path.as_ref()) {
      if served != &path {
        return Err(Reject::ConflictingScope {
          model: value.clone(),
          launch: launch_value.to_string(),
        });
      }
    }
    if scope.launch.is_none() {
      scope.log = LogScope::Model { path: path.clone() };
    }
    scope.name = Some(name);
    scope.path = Some(path);
  }
  Ok(scope)
}

/// `(launch id, catalog path)` for a `?launch=` value: a live or retired launch
/// id first, then a live launch's port. The path is `None` when the launch has
/// stopped and no row of it is left in the ring.
async fn resolve_launch(
  state: &ProxyState,
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  value: &str,
) -> Result<(String, Option<String>), Reject> {
  let by_id = |id: &str| {
    live
      .iter()
      .find(|l| l.id.eq_ignore_ascii_case(id))
      .map(|l| (l.id.clone(), Some(l.path.clone())))
      .or_else(|| {
        snapshot
          .launches
          .iter()
          .find(|l| l.totals.key.eq_ignore_ascii_case(id))
          .map(|l| {
            (
              l.totals.key.clone(),
              snapshot.launch_path(&l.totals.key).map(str::to_string),
            )
          })
      })
  };
  if let Some(found) = by_id(value) {
    return Ok(found);
  }
  if let Ok(port) = value.parse::<u16>() {
    if let Some(found) = live.iter().find(|l| l.port == port) {
      return Ok((found.id.clone(), Some(found.path.clone())));
    }
  }
  // A full catalog path is also a launch reference in practice: it names the
  // one live launch of that model.
  if let Ok((path, _)) = resolve_model(state, value).await {
    if let Some(found) = live.iter().find(|l| l.path == path) {
      return Ok((found.id.clone(), Some(path)));
    }
  }
  Err(Reject::UnknownRef(value.to_string()))
}

/// `(catalog path, display name)` for a `?model=` value: an exact catalog path,
/// then the same reference rules `/v1/chat/completions` uses (an outright name,
/// a `proxy.aliases` entry, a partial match).
pub(crate) async fn resolve_model(
  state: &ProxyState,
  value: &str,
) -> Result<(String, String), Reject> {
  let (views, rows) = state.ctx.catalog.shared_pair().await;
  if let Some(model) = route::index_catalog_by_path(&views).get(value).copied() {
    return Ok((
      value.to_string(),
      model
        .display_label
        .clone()
        .unwrap_or_else(|| crate::util::paths::model_display_name(&model.path)),
    ));
  }
  match route::resolve_client_reference(state, rows.as_ref(), value) {
    // The launch half of a `name@launch` reference is the caller's business,
    // not the scope's: the scope is the model it names.
    Ok((_, row)) => Ok((row.path.clone(), route::served_name_for_row(&row))),
    Err(route::ClientRefMiss::NotFound) => Err(Reject::UnknownRef(value.to_string())),
    Err(route::ClientRefMiss::Ambiguous(_)) => Err(Reject::Ambiguous(value.to_string())),
  }
}

/// What both routes parse out of a request before they differ.
pub(crate) struct Parsed {
  pub(crate) format: Format,
  pub(crate) scope: Scope,
  pub(crate) limit: usize,
}

/// Negotiate the format and resolve the scope of a metrics request.
pub(crate) async fn parse_request(
  state: &ProxyState,
  req: &Request<Incoming>,
  live: &[LiveLaunch],
) -> Result<Parsed, Reject> {
  let query = req.uri().query();
  let format = choose_format(query, req.headers())?;
  let scope = resolve_scope(state, query, live).await?;
  let limit = Query::parse(query).limit();
  Ok(Parsed {
    format,
    scope,
    limit,
  })
}

/// `GET /metrics`.
pub(crate) async fn serve(state: Arc<ProxyState>, req: Request<Incoming>) -> ProxyResponse {
  let live = live_launches(&state).await;
  let parsed = match parse_request(&state, &req, &live).await {
    Ok(parsed) => parsed,
    Err(reject) => return reject.into_response(),
  };
  let snapshot = state.ctx.requests.snapshot();
  let loaded = live.iter().filter(|l| l.ready).count();
  let discovered = state.ctx.catalog.len().await;
  if parsed.format == Format::Json {
    return json_response(json_body(
      &state.ctx.requests,
      &snapshot,
      &live,
      &parsed,
      loaded,
      discovered,
    ));
  }
  let mut doc = Document::new(true);
  render(
    &mut doc,
    &snapshot,
    &live,
    &parsed.scope,
    loaded,
    discovered,
  );
  if let Some(status) = state.ctx.requests.file_status() {
    doc.sample(
      "llamastash:request_log_file_dropped_total",
      "Finished requests that did not reach the request log file",
      "counter",
      &[],
      status.dropped as f64,
    );
  }
  text_response(doc.render())
}

/// Every `llamastash:*` series of a `/metrics` scrape into `doc`.
fn render(
  doc: &mut Document,
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  scope: &Scope,
  loaded: usize,
  discovered: usize,
) {
  // Daemon gauges: no labels, always answered.
  doc.sample(
    "llamastash:models_loaded",
    "Model launches currently Ready",
    "gauge",
    &[],
    loaded as f64,
  );
  doc.sample(
    "llamastash:models_discovered",
    "Models in the catalog",
    "gauge",
    &[],
    discovered as f64,
  );
  doc.sample(
    "llamastash:request_log_rows",
    "Requests held in the in-memory request log",
    "gauge",
    &[],
    snapshot.rows.len() as f64,
  );
  doc.sample(
    "llamastash:request_log_capacity",
    "How many requests the request log ring holds",
    "gauge",
    &[],
    CAPACITY as f64,
  );

  for (route, count) in &snapshot.routes {
    doc.sample(
      "llamastash:route_requests_total",
      "Requests the proxy finished, by request path",
      "counter",
      &[("route", route)],
      *count as f64,
    );
  }

  // The `model` label value of every model in the log, settled before the scope
  // cuts anything: the same model has to print the same value in a scoped and an
  // unscoped scrape, or the two series read as two models.
  let labels = model_labels(&snapshot.models);
  let models: Vec<&ScopeTotals> = snapshot
    .models
    .iter()
    .filter(|m| scope.keeps_model(&m.key))
    .collect();
  for totals in models {
    let label = &labels[&totals.key];
    let in_flight =
      snapshot.in_flight(|row| row.model_path.as_deref() == Some(totals.key.as_str()));
    model_series(doc, label, totals, in_flight);
  }

  // Requests the proxy answered without ever reaching a model (an unknown model
  // name, an unreadable body). They are in `summary` but under no model, so an
  // unscoped scrape gives them their own series instead of dropping them.
  if scope.path.is_none() && scope.launch.is_none() {
    unattributed_series(doc, snapshot);
  }

  if !scope.per_launch {
    return;
  }
  for entry in live {
    doc.sample(
      "llamastash:launch_info",
      "Live model launches, labelled by launch and model",
      "gauge",
      &[
        ("launch", &entry.id),
        ("model", &entry.model),
        ("port", &entry.port.to_string()),
      ],
      1.0,
    );
  }
  for totals in &snapshot.launches {
    let id = &totals.totals.key;
    let path = live
      .iter()
      .find(|l| &l.id == id)
      .map(|l| l.path.as_str())
      .or_else(|| snapshot.launch_path(id));
    if !scope.keeps_launch(id, path) {
      continue;
    }
    let label = totals
      .totals
      .label
      .clone()
      .or_else(|| live.iter().find(|l| &l.id == id).map(|l| l.model.clone()))
      .unwrap_or_else(|| "unknown".to_string());
    let per_launch_labels = [("launch", id.as_str()), ("model", label.as_str())];
    let s = &totals.totals.summary;
    doc.sample(
      "llamastash:launch_requests_total",
      "Requests one launch finished",
      "counter",
      &per_launch_labels,
      s.requests as f64,
    );
    doc.sample(
      "llamastash:launch_request_errors_total",
      "Requests one launch failed",
      "counter",
      &per_launch_labels,
      s.errors as f64,
    );
    doc.sample(
      "llamastash:launch_prompt_tokens_total",
      "Prompt tokens one launch served",
      "counter",
      &per_launch_labels,
      s.prompt_tokens as f64,
    );
    doc.sample(
      "llamastash:launch_completion_tokens_total",
      "Completion tokens one launch served",
      "counter",
      &per_launch_labels,
      s.completion_tokens as f64,
    );
    // In-flight is a live quantity: a stopped launch has none, and printing the
    // zero would read as a request that never finished.
    if totals.live {
      let in_flight = snapshot.in_flight(|row| row.launch_id.as_deref() == Some(id.as_str()));
      doc.sample(
        "llamastash:launch_requests_in_flight",
        "Requests one launch is still serving",
        "gauge",
        &per_launch_labels,
        in_flight as f64,
      );
    }
  }
}

/// The `model` label value for a request that never reached one.
const NO_MODEL: &str = "none";

/// Series for the requests no model answered, as `model="none"`. Everything
/// the log holds for a model is attributed to it, so the unattributed share is
/// the whole log minus the models; a request still in flight before any model
/// is picked is counted there too.
fn unattributed_series(doc: &mut Document, snapshot: &LogSnapshot) {
  let attributed_requests: u64 = snapshot.models.iter().map(|m| m.summary.requests).sum();
  let attributed_errors: u64 = snapshot.models.iter().map(|m| m.summary.errors).sum();
  let requests = snapshot
    .all
    .summary
    .requests
    .saturating_sub(attributed_requests);
  let in_flight = snapshot.in_flight(|row| row.model_path.is_none());
  if requests == 0 && in_flight == 0 {
    return;
  }
  let errors = snapshot
    .all
    .summary
    .errors
    .saturating_sub(attributed_errors);
  let labels = [("model", NO_MODEL)];
  doc.sample(
    "llamastash:requests_total",
    "Requests the proxy finished, since daemon start",
    "counter",
    &labels,
    requests as f64,
  );
  doc.sample(
    "llamastash:request_errors_total",
    "Finished requests that failed or were cut short",
    "counter",
    &labels,
    errors as f64,
  );
  doc.sample(
    "llamastash:requests_in_flight",
    "Requests the proxy is still serving",
    "gauge",
    &labels,
    in_flight as f64,
  );
}

/// The counters and gauges that carry one `model` label.
fn model_series(doc: &mut Document, label: &str, totals: &ScopeTotals, in_flight: u64) {
  let labels = [("model", label)];
  let s = &totals.summary;
  doc.sample(
    "llamastash:requests_total",
    "Requests the proxy finished, since daemon start",
    "counter",
    &labels,
    s.requests as f64,
  );
  doc.sample(
    "llamastash:request_errors_total",
    "Finished requests that failed or were cut short",
    "counter",
    &labels,
    s.errors as f64,
  );
  doc.sample(
    "llamastash:prompt_tokens_total",
    "Prompt tokens the finished requests reported",
    "counter",
    &labels,
    s.prompt_tokens as f64,
  );
  doc.sample(
    "llamastash:completion_tokens_total",
    "Completion tokens the finished requests reported",
    "counter",
    &labels,
    s.completion_tokens as f64,
  );
  doc.sample(
    "llamastash:request_duration_seconds_total",
    "Total duration of the finished requests",
    "counter",
    &labels,
    totals.duration_ms_sum as f64 / 1000.0,
  );
  doc.sample(
    "llamastash:request_ttft_seconds_total",
    "Total time to the first response byte of the finished requests",
    "counter",
    &labels,
    totals.ttfb_ms_sum as f64 / 1000.0,
  );
  doc.sample(
    "llamastash:auto_starts_total",
    "Requests that started a model launch",
    "counter",
    &labels,
    s.auto_starts as f64,
  );
  doc.sample(
    "llamastash:evictions_total",
    "Launches unloaded to make room for a request",
    "counter",
    &labels,
    s.evictions as f64,
  );
  doc.sample(
    "llamastash:requests_in_flight",
    "Requests the proxy is still serving",
    "gauge",
    &labels,
    in_flight as f64,
  );
  // A gauge with no figure yet has no sample; the metric is then omitted whole
  // rather than printed as a made-up zero.
  if let Some(ms) = s.avg_duration_ms {
    doc.sample(
      "llamastash:request_duration_seconds_avg",
      "Mean duration of the completed successful requests",
      "gauge",
      &labels,
      ms as f64 / 1000.0,
    );
  }
  if let Some(ms) = s.avg_ttfb_ms {
    doc.sample(
      "llamastash:request_ttft_seconds_avg",
      "Mean time to first byte of the completed successful requests",
      "gauge",
      &labels,
      ms as f64 / 1000.0,
    );
  }
  if let Some(tps) = s.tokens_per_second_last {
    doc.sample(
      "llamastash:request_tokens_per_second",
      "Generation speed the most recent request reported",
      "gauge",
      &labels,
      tps,
    );
  }
  if let Some(tps) = s.tokens_per_second_avg {
    doc.sample(
      "llamastash:request_tokens_per_second_avg",
      "Mean of the per-request generation speeds",
      "gauge",
      &labels,
      tps,
    );
  }
}

/// The `model` label value of each entry in `models`. Two models can share a
/// display name (same-named GGUFs in different roots), and one metric name may
/// not carry two samples with the same label set, so a shared name falls back to
/// the catalog path for the models that collide. Callers pass the whole log, so
/// the pick does not depend on what a scope happens to include.
fn model_labels(models: &[ScopeTotals]) -> HashMap<String, String> {
  let mut counts: HashMap<String, usize> = HashMap::new();
  for totals in models {
    *counts
      .entry(totals.label.clone().unwrap_or_default())
      .or_default() += 1;
  }
  models
    .iter()
    .map(|totals| {
      let label = totals.label.clone().unwrap_or_else(|| totals.key.clone());
      let out = if counts.get(&label).copied().unwrap_or(0) > 1 {
        totals.key.clone()
      } else {
        label
      };
      (totals.key.clone(), out)
    })
    .collect()
}

/// The JSON body: the same `summary` and `requests` objects
/// `llamastash requests --json` returns, plus the per-model and per-launch
/// totals the text format carries as samples.
fn json_body(
  log: &RequestLog,
  snapshot: &LogSnapshot,
  live: &[LiveLaunch],
  parsed: &Parsed,
  loaded: usize,
  discovered: usize,
) -> Value {
  let scope = &parsed.scope;
  let tail = log.tail(scope.log.clone(), parsed.limit);
  let mut scope_block = Map::new();
  scope_block.insert("model".into(), value_opt(scope.name.clone()));
  scope_block.insert("launch".into(), value_opt(scope.launch.clone()));

  let mut models = Vec::new();
  for totals in snapshot.models.iter().filter(|m| scope.keeps_model(&m.key)) {
    let launches = snapshot
      .launches
      .iter()
      .filter(|l| {
        live
          .iter()
          .find(|entry| entry.id == l.totals.key)
          .map(|entry| entry.path == totals.key)
          .or_else(|| snapshot.launch_path(&l.totals.key).map(|p| p == totals.key))
          .unwrap_or(false)
      })
      .count();
    models.push(merged(
      &[
        ("model", value_opt(totals.label.clone())),
        ("path", Value::String(totals.key.clone())),
        ("launches", Value::from(launches)),
      ],
      &totals.summary,
    ));
  }

  let mut launches = Vec::new();
  for entry in &snapshot.launches {
    let id = &entry.totals.key;
    let live_entry = live.iter().find(|l| &l.id == id);
    let path = live_entry
      .map(|l| l.path.clone())
      .or_else(|| snapshot.launch_path(id).map(str::to_string));
    if !scope.keeps_launch(id, path.as_deref()) {
      continue;
    }
    let port = live_entry.map(|l| l.port);
    let model = entry
      .totals
      .label
      .clone()
      .or_else(|| live_entry.map(|l| l.model.clone()));
    let body = merged(
      &[
        ("launch", Value::String(id.clone())),
        ("model", value_opt(model)),
        (
          "port",
          match port {
            Some(p) => Value::from(p),
            None => Value::Null,
          },
        ),
        ("live", Value::Bool(live_entry.is_some())),
      ],
      &entry.totals.summary,
    );
    launches.push(body);
  }

  json_object(|map| {
    map.insert(
      "generated_at_ms".into(),
      Value::from(
        SystemTime::now()
          .duration_since(UNIX_EPOCH)
          .map(|d| d.as_millis() as u64)
          .unwrap_or(0),
      ),
    );
    map.insert("scope".into(), Value::Object(scope_block));
    map.insert(
      "summary".into(),
      serde_json::to_value(&tail.summary).unwrap_or(Value::Null),
    );
    map.insert("models".into(), Value::Array(models));
    map.insert("launches".into(), Value::Array(launches));
    map.insert(
      "requests".into(),
      Value::Array(
        tail
          .rows
          .iter()
          .map(|row| serde_json::to_value(row).unwrap_or(Value::Null))
          .collect(),
      ),
    );
    map.insert("models_loaded".into(), Value::from(loaded));
    map.insert("models_discovered".into(), Value::from(discovered));
  })
}

/// A [`RequestSummary`] with extra keys in front of its own, so a client that
/// parses `requests --json` parses this too.
fn merged(extra: &[(&str, Value)], summary: &RequestSummary) -> Value {
  json_object(|map| {
    for (key, value) in extra {
      map.insert((*key).into(), value.clone());
    }
    if let Value::Object(fields) = serde_json::to_value(summary).unwrap_or(Value::Null) {
      for (key, value) in fields {
        map.insert(key, value);
      }
    }
  })
}

/// A JSON object built in insertion order.
pub(crate) fn json_object(build: impl FnOnce(&mut Map<String, Value>)) -> Value {
  let mut map = Map::new();
  build(&mut map);
  Value::Object(map)
}

fn value_opt(value: Option<String>) -> Value {
  match value {
    Some(v) => Value::String(v),
    None => Value::Null,
  }
}

/// One metric name's HELP/TYPE text and its samples, in print order.
struct Block {
  help: String,
  ty: String,
  samples: Vec<(String, f64)>,
}

/// A Prometheus text document under construction.
///
/// `# HELP` and `# TYPE` are written once per metric name, first writer wins.
/// A metric whose only samples were refused (non-finite) prints nothing at all,
/// which is what a scraper expects: no empty `# TYPE` block.
pub(crate) struct Document {
  blocks: Vec<(String, Block)>,
  index: HashMap<String, usize>,
  /// Our own series sort by name and by label text; a merged engine scrape
  /// keeps the order the targets printed.
  sorted: bool,
}

impl Document {
  pub(crate) fn new(sorted: bool) -> Self {
    Document {
      blocks: Vec::new(),
      index: HashMap::new(),
      sorted,
    }
  }

  fn block(&mut self, name: &str) -> &mut (String, Block) {
    if let Some(i) = self.index.get(name) {
      return &mut self.blocks[*i];
    }
    self.index.insert(name.to_string(), self.blocks.len());
    self.blocks.push((
      name.to_string(),
      Block {
        help: String::new(),
        ty: "untyped".to_string(),
        samples: Vec::new(),
      },
    ));
    self.blocks.last_mut().expect("just pushed")
  }

  /// Record a `# HELP` / `# TYPE` line read from a scraped engine. The first
  /// one for a name wins, so merging N targets prints one block per name.
  pub(crate) fn declare(&mut self, name: &str, help: Option<&str>, ty: Option<&str>) {
    let block = self.block(name);
    if block.1.help.is_empty() {
      if let Some(help) = help {
        block.1.help = help.to_string();
      }
    }
    if block.1.ty == "untyped" {
      if let Some(ty) = ty {
        block.1.ty = ty.to_string();
      }
    }
  }

  /// One sample of a `llamastash:*` metric. `labels` print as `key="value"`
  /// pairs in the order given.
  pub(crate) fn sample(
    &mut self,
    name: &str,
    help: &str,
    ty: &str,
    labels: &[(&str, &str)],
    value: f64,
  ) {
    if !value.is_finite() {
      return;
    }
    let rendered = labels
      .iter()
      .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
      .collect::<Vec<_>>()
      .join(",");
    let block = self.block(name);
    if block.1.help.is_empty() {
      block.1.help = help.to_string();
    }
    if block.1.ty == "untyped" {
      block.1.ty = ty.to_string();
    }
    block.1.samples.push((rendered, value));
  }

  /// One sample taken verbatim from a scraped engine: `labels` is the text
  /// between the braces, kept as the engine printed it.
  pub(crate) fn sample_raw(&mut self, name: &str, labels: String, value: f64) {
    if !value.is_finite() {
      return;
    }
    self.block(name).1.samples.push((labels, value));
  }

  /// How many samples landed in the document.
  #[cfg(test)]
  pub(crate) fn samples(&self) -> usize {
    self.blocks.iter().map(|(_, b)| b.samples.len()).sum()
  }

  pub(crate) fn render(&self) -> String {
    let mut order: Vec<&(String, Block)> = self.blocks.iter().collect();
    if self.sorted {
      order.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let mut out = String::new();
    for (name, block) in order {
      if block.samples.is_empty() {
        continue;
      }
      let mut samples: Vec<&(String, f64)> = block.samples.iter().collect();
      if self.sorted {
        samples.sort_by(|a, b| a.0.cmp(&b.0));
      }
      if !block.help.is_empty() {
        out.push_str(&format!("# HELP {name} {}\n", escape(&block.help)));
      }
      out.push_str(&format!("# TYPE {name} {}\n", block.ty));
      for (labels, value) in samples {
        if labels.is_empty() {
          out.push_str(&format!("{name} {}\n", fmt_value(*value)));
        } else {
          out.push_str(&format!("{name}{{{labels}}} {}\n", fmt_value(*value)));
        }
      }
    }
    out
  }
}

/// A Prometheus label value: `\`, `"`, and the control characters that have an
/// escape are replaced.
pub(crate) fn escape(value: &str) -> String {
  let mut out = String::with_capacity(value.len());
  for c in value.chars() {
    match c {
      '\\' => out.push_str(r"\\"),
      '"' => out.push_str(r#"\""#),
      '\n' => out.push_str(r"\n"),
      '\r' => out.push_str(r"\r"),
      '\t' => out.push_str(r"\t"),
      _ => out.push(c),
    }
  }
  out
}

/// A sample value. Whole numbers print without a decimal point; Prometheus
/// reads either form and an integer counter reads cleaner.
fn fmt_value(value: f64) -> String {
  if value.fract() == 0.0 && value.abs() < 1e15 {
    format!("{}", value as i64)
  } else {
    format!("{value}")
  }
}

/// 200 OK with a Prometheus text body.
pub(crate) fn text_response(body: String) -> ProxyResponse {
  Ok(
    Response::builder()
      .status(StatusCode::OK)
      .header(CONTENT_TYPE, TEXT_CONTENT_TYPE)
      .body(
        Full::new(Bytes::from(body))
          .map_err(|never| match never {})
          .boxed(),
      )
      .expect("static text response must build"),
  )
}

/// 200 OK with a JSON body built by one of these routes.
pub(crate) fn json_response(body: Value) -> ProxyResponse {
  let bytes = serde_json::to_vec(&body).expect("json encoding of a built value cannot fail");
  Ok(
    Response::builder()
      .status(StatusCode::OK)
      .header(CONTENT_TYPE, "application/json")
      .body(
        Full::new(Bytes::from(bytes))
          .map_err(|never| match never {})
          .boxed(),
      )
      .expect("static headers always parse"),
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::proxy::request_log::RequestState;
  use crate::proxy::usage_tap::Usage;
  use std::collections::BTreeSet;

  fn format_of(accept: Option<&str>) -> Result<Format, Reject> {
    let mut headers = HeaderMap::new();
    if let Some(a) = accept {
      headers.insert(hyper::header::ACCEPT, a.parse().unwrap());
    }
    choose_format(None, &headers)
  }

  #[test]
  fn metrics_accept_contract_matches_the_plan() {
    assert_eq!(format_of(None).unwrap(), Format::Text);
    assert_eq!(format_of(Some("*/*")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("text/plain")).unwrap(), Format::Text);
    assert_eq!(
      format_of(Some("text/plain; version=0.0.4; charset=utf-8")).unwrap(),
      Format::Text
    );
    assert_eq!(
      format_of(Some(
        "application/openmetrics-text; version=1.0.0,text/plain"
      ))
      .unwrap(),
      Format::Text
    );
    // A wildcard never selects JSON, on either side.
    assert_eq!(format_of(Some("application/*")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("text/*")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("*/*;q=0.5")).unwrap(), Format::Text);
    assert_eq!(format_of(Some("application/json")).unwrap(), Format::Json);
    assert_eq!(
      format_of(Some("application/json;q=0.9, text/plain;q=0.5")).unwrap(),
      Format::Json
    );
    assert_eq!(
      format_of(Some("application/json;q=0.9, text/plain;q=0.9")).unwrap(),
      Format::Text
    );
    assert!(matches!(
      format_of(Some("application/xml")),
      Err(Reject::NotAcceptable(_))
    ));
  }

  #[test]
  fn metrics_format_query_beats_the_accept_header() {
    let json = HeaderMap::from_iter([(hyper::header::ACCEPT, "*/*".parse().unwrap())]);
    assert_eq!(
      choose_format(Some("format=json"), &json).unwrap(),
      Format::Json
    );
    assert_eq!(
      choose_format(Some("format=prometheus"), &json).unwrap(),
      Format::Text
    );
    assert!(matches!(
      choose_format(Some("format=yaml"), &json),
      Err(Reject::BadFormat(v)) if v == "yaml"
    ));
    // A `?format=` beats a header that would otherwise be refused outright.
    assert_eq!(
      choose_format(
        Some("format=prometheus"),
        &HeaderMap::from_iter([(hyper::header::ACCEPT, "application/xml".parse().unwrap())])
      )
      .unwrap(),
      Format::Text
    );
  }

  #[test]
  fn metrics_query_parses_every_parameter() {
    let q = Query::parse(Some("format=json&limit=5&model=llama3&launch=1"));
    assert_eq!(q.format.as_deref(), Some("json"));
    assert_eq!(q.limit, Some(5));
    assert_eq!(q.model.as_deref(), Some("llama3"));
    assert!(q.per_launch());
    assert_eq!(q.launch_ref(), None);
    // A percent-escaped space and `+` come back decoded.
    let escaped = Query::parse(Some("model=qwen3%204b%2Fq4"));
    assert_eq!(escaped.model.as_deref(), Some("qwen3 4b/q4"));
    // A multi-byte character is a run of escapes and has to survive whole, and
    // `%2B` stays a literal plus while a bare `+` is a space.
    let unicode = Query::parse(Some("model=caf%C3%A9+%2Bx&launch=L2%20b"));
    assert_eq!(unicode.model.as_deref(), Some("caf\u{e9} +x"));
    assert_eq!(unicode.launch.as_deref(), Some("L2 b"));
    // A `%` that starts no hex pair is itself.
    assert_eq!(
      Query::parse(Some("model=100%off")).model.as_deref(),
      Some("100%off")
    );
    assert_eq!(Query::parse(None).limit(), DEFAULT_TAIL);
    assert_eq!(Query::parse(Some("limit=99999")).limit(), CAPACITY);
  }

  /// One finished request, straight into a fresh log.
  fn request(log: &RequestLog, path: &str, launch: &str, prompt: u64, completion: u64) {
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model("m1", path);
    rec.set_launch(launch);
    rec.set_status(200);
    rec.mark_first_byte();
    rec.finish(
      RequestState::Done,
      Usage {
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        tokens_per_second: Some(40.0),
        tokens_per_second_estimated: false,
      },
    );
  }

  #[test]
  fn metrics_snapshot_counts_routes_and_retired_launches() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    log.forget_launch("L1");
    let snap = log.snapshot();
    assert_eq!(snap.routes, vec![("/v1/chat/completions".to_string(), 1)]);
    assert_eq!(snap.models.len(), 1);
    assert_eq!(snap.launches.len(), 1);
    assert!(!snap.launches[0].live);
    assert_eq!(snap.models[0].summary.requests, 1);
    assert_eq!(snap.all.duration_ms_sum, snap.models[0].duration_ms_sum);
  }

  #[test]
  fn metrics_text_renders_counters_and_gauges() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let snap = log.snapshot();
    let scope = Scope::default();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &scope, 1, 2);
    let text = doc.render();
    assert!(text.contains("llamastash:requests_total{model=\"m1\"} 1\n"));
    assert!(text.contains("llamastash:prompt_tokens_total{model=\"m1\"} 10\n"));
    assert!(text.contains("llamastash:route_requests_total{route=\"/v1/chat/completions\"} 1\n"));
    assert!(text.contains("llamastash:requests_in_flight{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:models_loaded 1\n"));
    assert!(text.contains("llamastash:request_log_capacity 1000\n"));
    // A finished successful request has a duration and a first byte, so both
    // averages are there; they are sub-millisecond in a unit test.
    assert!(text.contains("llamastash:request_duration_seconds_avg{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_ttft_seconds_avg{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_duration_seconds_total{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_ttft_seconds_total{model=\"m1\"} 0\n"));
    assert!(text.contains("llamastash:request_tokens_per_second{model=\"m1\"} 40\n"));
    // No `?launch=1`, so no per-launch series.
    assert!(!text.contains("llamastash:launch_"));
  }

  #[test]
  fn metrics_text_omits_labels_the_scope_excludes() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    request(&log, "/models/b.gguf", "L2", 1, 1);
    let snap = log.snapshot();
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      name: Some("m1".into()),
      per_launch: true,
      launch: None,
    };
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &scope, 2, 2);
    let text = doc.render();
    assert_eq!(text.matches("llamastash:requests_total{model=").count(), 1);
    // Both launches served the same model name in the fixture, so the launch
    // series print it for whichever the scope keeps.
    assert!(text.contains("llamastash:launch_requests_total{launch=\"L1\",model=\"m1\"} 1"));
    assert!(!text.contains("launch=\"L2\""));
  }

  #[test]
  fn metrics_colliding_model_names_fall_back_to_paths() {
    let log = RequestLog::new();
    request(&log, "/models/a/x.gguf", "L1", 1, 1);
    request(&log, "/models/b/x.gguf", "L2", 2, 1);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 2, 2);
    let text = doc.render();
    assert!(text.contains("llamastash:requests_total{model=\"/models/a/x.gguf\"} 1\n"));
    assert!(text.contains("llamastash:requests_total{model=\"/models/b/x.gguf\"} 1\n"));
  }

  /// One finished request that reached no model: the proxy answered it itself.
  fn rejected(log: &RequestLog, route: &str, status: u16) {
    let mut rec = log.begin(route, None);
    rec.set_status(status);
    rec.finish(RequestState::Done, Usage::default());
  }

  #[test]
  fn metrics_unattributed_requests_get_a_model_none_series() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    rejected(&log, "/v1/chat/completions", 404);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    let text = doc.render();
    for line in [
      "llamastash:requests_total{model=\"none\"} 1\n",
      "llamastash:request_errors_total{model=\"none\"} 1\n",
      "llamastash:requests_in_flight{model=\"none\"} 0\n",
      "llamastash:requests_total{model=\"m1\"} 1\n",
    ] {
      assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }

    // A model scope answers for that model, so the unattributed share sits
    // outside it; the log-wide totals stay in `summary` either way.
    let scope = Scope {
      log: LogScope::Model {
        path: "/models/a.gguf".into(),
      },
      path: Some("/models/a.gguf".into()),
      name: Some("m1".into()),
      ..Scope::default()
    };
    let mut scoped = Document::new(true);
    render(&mut scoped, &snap, &[], &scope, 1, 1);
    let scoped_text = scoped.render();
    assert!(
      !scoped_text.contains("model=\"none\""),
      "a model scope is that model's numbers:\n{scoped_text}"
    );
    assert!(scoped_text.contains("llamastash:requests_total{model=\"m1\"} 1\n"));
  }

  #[test]
  fn metrics_no_model_none_series_when_everything_is_attributed() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 1, 1);
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 1, 1);
    // Nothing to say, so no zero-filled series beside the real ones.
    assert!(!doc.render().contains("model=\"none\""));
  }

  #[test]
  fn metrics_model_label_does_not_change_with_the_scope() {
    // Two models that share a display name, so both print as their catalog path.
    let log = RequestLog::new();
    request(&log, "/models/a/x.gguf", "L1", 1, 1);
    request(&log, "/models/b/x.gguf", "L2", 2, 1);
    let snap = log.snapshot();
    let render_with = |scope: Scope| {
      let mut doc = Document::new(true);
      render(&mut doc, &snap, &[], &scope, 2, 2);
      doc.render()
    };
    let unscoped = render_with(Scope::default());
    let scoped = render_with(Scope {
      log: LogScope::Model {
        path: "/models/a/x.gguf".into(),
      },
      path: Some("/models/a/x.gguf".into()),
      ..Scope::default()
    });
    let want = "llamastash:requests_total{model=\"/models/a/x.gguf\"} 1\n";
    assert!(unscoped.contains(want), "unscoped:\n{unscoped}");
    assert!(scoped.contains(want), "scoped:\n{scoped}");
    assert!(
      !scoped.contains("{model=\"m1\"}"),
      "scoping to one of two colliding names must not rename it back:\n{scoped}"
    );
  }

  #[test]
  fn metrics_retired_launches_keep_totals_but_not_the_in_flight_gauge() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 1, 1);
    log.forget_launch("L1");
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(
      &mut doc,
      &snap,
      &[],
      &Scope {
        per_launch: true,
        ..Scope::default()
      },
      0,
      1,
    );
    let text = doc.render();
    assert!(
      text.contains("llamastash:launch_requests_total{launch=\"L1\",model=\"m1\"} 1\n"),
      "a stopped launch keeps its totals:\n{text}"
    );
    // In-flight is a live quantity, so a stopped launch prints no zero for it.
    assert!(
      !text.contains("llamastash:launch_requests_in_flight"),
      "in-flight is live-only:\n{text}"
    );
    assert!(
      !text.contains("llamastash:launch_info"),
      "live-only:\n{text}"
    );
  }

  /// Every `llamastash:*` name the two metrics modules can emit, read from the
  /// module source ahead of its test module.
  fn emitted_series_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for module in ["metrics.rs", "metrics_backend.rs"] {
      let path = format!("{}/src/proxy/{module}", env!("CARGO_MANIFEST_DIR"));
      let src = std::fs::read_to_string(&path).expect("module source");
      let src = src.split("#[cfg(test)]").next().unwrap_or(&src);
      let mut rest = src;
      while let Some(at) = rest.find("\"llamastash:") {
        rest = &rest[at + 1..];
        names.insert(
          rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_'))
            .collect(),
        );
      }
    }
    names
  }

  /// Every `llamastash:*` name `docs/usage.md` lists in its two Metrics sections.
  fn documented_series_names() -> BTreeSet<String> {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/usage.md"))
      .expect("usage.md");
    let start = doc
      .find("### Metrics (`/metrics`)")
      .expect("a Metrics section");
    let rest = &doc[start..];
    let end = rest
      .find("### Ollama-compat surface")
      .expect("a section after the two Metrics ones");
    let section = &rest[..end];
    let mut names = BTreeSet::new();
    let mut at = 0;
    while let Some(found) = section[at..].find("llamastash:") {
      let from = at + found + "llamastash:".len();
      names.insert(format!(
        "llamastash:{}",
        section[from..]
          .chars()
          .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_'))
          .collect::<String>()
      ));
      at = from;
    }
    names
  }

  #[test]
  fn metrics_series_names_match_the_docs() {
    let emitted = emitted_series_names();
    let documented = documented_series_names();
    let undocumented: Vec<&String> = emitted.difference(&documented).collect();
    let never_emitted: Vec<&String> = documented.difference(&emitted).collect();
    assert!(
      undocumented.is_empty() && never_emitted.is_empty(),
      "docs/usage.md and the code disagree; undocumented {undocumented:?}, \
       listed but never emitted {never_emitted:?}"
    );
  }

  #[test]
  fn metrics_json_shape_carries_the_requests_contract_keys() {
    let log = RequestLog::new();
    request(&log, "/models/a.gguf", "L1", 10, 3);
    let snap = log.snapshot();
    let parsed = Parsed {
      format: Format::Json,
      scope: Scope::default(),
      limit: 10,
    };
    let body = json_body(&log, &snap, &[], &parsed, 1, 2);
    assert_eq!(body["models_loaded"], 1);
    assert_eq!(body["summary"]["requests"], 1);
    assert_eq!(body["requests"].as_array().unwrap().len(), 1);
    // The row keys are the ones `requests --json` returns.
    assert!(body["requests"][0].get("launch_id").is_some());
    assert_eq!(body["models"][0]["path"], "/models/a.gguf");
    assert_eq!(body["models"][0]["requests"], 1);
    assert_eq!(body["launches"][0]["launch"], "L1");
    assert_eq!(body["launches"][0]["live"], false);
    assert_eq!(body["scope"]["launch"], Value::Null);
  }

  #[test]
  fn metrics_text_omits_an_average_with_nothing_to_average() {
    // A failed request still counts towards the duration counter, but the mean is
    // only over successful requests. Printing a made-up zero would read as fast.
    let log = RequestLog::new();
    let mut rec = log.begin("/v1/chat/completions", None);
    rec.set_model("m1", "/models/a.gguf");
    rec.set_launch("L1");
    rec.set_status(500);
    rec.finish(RequestState::Done, Usage::default());
    let snap = log.snapshot();
    let mut doc = Document::new(true);
    render(&mut doc, &snap, &[], &Scope::default(), 0, 0);
    let text = doc.render();
    assert!(text.contains("llamastash:request_errors_total{model=\"m1\"} 1\n"));
    assert!(text.contains("llamastash:request_duration_seconds_total{model=\"m1\"} 0\n"));
    assert!(!text.contains("llamastash:request_duration_seconds_avg"));
    assert!(!text.contains("llamastash:request_ttft_seconds_avg"));
    assert!(!text.contains("llamastash:request_tokens_per_second"));
  }

  #[test]
  fn metrics_document_sorts_and_skips_non_finite_values() {
    let mut doc = Document::new(true);
    doc.sample("b:name", "second", "gauge", &[], 1.0);
    doc.sample("a:name", "first", "counter", &[("k", "z")], 2.0);
    doc.sample("a:name", "first", "counter", &[("k", "a")], 3.0);
    doc.sample("c:name", "nan", "gauge", &[], f64::NAN);
    let text = doc.render();
    assert_eq!(
      text,
      "# HELP a:name first\n# TYPE a:name counter\na:name{k=\"a\"} 3\na:name{k=\"z\"} 2\n\
       # HELP b:name second\n# TYPE b:name gauge\nb:name 1\n"
    );
  }

  #[test]
  fn metrics_document_keeps_the_first_help_and_type() {
    let mut doc = Document::new(false);
    doc.declare("llamacpp:x", Some("first help"), Some("counter"));
    doc.declare("llamacpp:x", Some("second help"), Some("gauge"));
    doc.sample_raw("llamacpp:x", String::new(), 5.0);
    assert_eq!(
      doc.render(),
      "# HELP llamacpp:x first help\n# TYPE llamacpp:x counter\nllamacpp:x 5\n"
    );
    assert_eq!(doc.samples(), 1);
  }

  #[test]
  fn metrics_label_values_are_escaped() {
    assert_eq!(escape("a\"b\\c\nd"), r#"a\"b\\c\nd"#);
    assert_eq!(fmt_value(2.0), "2");
    assert_eq!(fmt_value(0.25), "0.25");
  }
}
