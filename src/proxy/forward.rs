//! Byte-pipe forwarding of `/v1/...` requests to a Ready upstream
//! `llama-server`.
//!
//! Once [`super::route::decide`] produces a [`RouteDecision::ReadyAt`],
//! the router hands the buffered body + the target port off to
//! [`forward_to_upstream`]. The contract is intentionally minimal:
//!
//! - Mirror inbound method, path, query, and headers (stripping
//!   hop-by-hop entries per RFC 7230).
//! - Forward the **buffered body bytes** with at most two surgical edits:
//!   `body.model` when the launch pins a request-model name, and whatever the
//!   serving backend needs for its engine
//!   ([`crate::backend::Backend::rewrite_request_body`]). Both copy every
//!   untouched entry as the client's own bytes — the body is never re-encoded.
//!   The plan's Risks row "rewrites body.model" makes the case for this; the
//!   echo-verification test in `tests/proxy_echo_verification.rs` keeps it
//!   honest.
//! - Stream the upstream response body through `http_body_util::StreamBody`
//!   so SSE chunks land at the client as they arrive. No buffering,
//!   no per-chunk parse.
//! - Stamp `x-llamastash-served-by` + `x-llamastash-fallback-reason`
//!   only when [`RouteDecision::fallback == true`] — set by the
//!   family-MRU fallback path.

use std::sync::Arc;

use futures::TryStreamExt;
use http_body_util::{combinators::BoxBody, BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};

use super::router::{BodyError, ProxyResponse};
use super::state::ProxyState;

/// Hop-by-hop header set — RFC 7230 §6.1. Stripped on both the
/// outbound request (so we don't leak the inbound peer's keep-alive
/// state into the upstream connection) and the inbound response (so
/// the client doesn't see contradictory framing). Lower-case keys
/// because `HeaderName::as_str()` is lower-case canonical.
const HOP_BY_HOP: &[&str] = &[
  "connection",
  "keep-alive",
  "transfer-encoding",
  "te",
  "trailers",
  "upgrade",
  "proxy-authorization",
  "proxy-authenticate",
];

/// Inbound-request slice consumed by [`forward_to_upstream`]. Bundled
/// so the forwarding fn stays under clippy's argument limit and so
/// the call-site reads as "here is the inbound request, here is the
/// target" rather than five positional strings.
pub(crate) struct InboundRequest {
  pub method: Method,
  pub uri: hyper::Uri,
  pub headers: HeaderMap,
  pub body_bytes: Bytes,
}

/// Target the inbound request should be forwarded to. Populated by
/// [`super::route::decide`] into a [`super::route::RouteDecision::ReadyAt`]
/// variant; the forwarding fn lifts the fields off the variant.
pub(crate) struct Target<'a> {
  pub port: u16,
  pub served_model_id: &'a str,
  /// Canonical id of the supervisor that owns `port` at decision
  /// time. Used to re-verify the binding immediately before send so
  /// a Ready→Stopping→port-reuse race can't silently route to a
  /// different model.
  pub served_model_key: &'a crate::gguf::identity::ModelId,
  /// Upstream path prefix prepended to the inbound path before the
  /// request is sent (`None` for direct llama.cpp, which serves OpenAI
  /// at `/v1/...`; `Some("/api")` for the Lemonade umbrella, which
  /// serves it at `/api/v1/...`). Lets one forward path target backends
  /// whose OpenAI surface lives under different roots.
  pub upstream_path_prefix: Option<&'a str>,
  pub fallback: bool,
  pub fallback_reason: Option<&'a str>,
}

/// Forward an inbound `hyper::Request` to the upstream
/// `llama-server` on `target.port`, stream the response back to the
/// caller.
///
/// `inbound.body_bytes` is the already-buffered (and length-checked)
/// inbound body; the caller has run
/// [`super::route::buffer_and_extract`] so we don't repeat the cap
/// enforcement here.
pub(crate) async fn forward_to_upstream(
  state: &Arc<ProxyState>,
  inbound: InboundRequest,
  target: Target<'_>,
) -> ProxyResponse {
  let InboundRequest {
    method: inbound_method,
    uri: inbound_uri,
    headers: inbound_headers,
    body_bytes,
  } = inbound;
  let Target {
    port,
    served_model_id,
    served_model_key,
    upstream_path_prefix,
    fallback,
    fallback_reason,
  } = target;
  // Re-verify the supervisor at `port` is still the one we picked,
  // and take an in-flight guard on the matching ManagedModel in the
  // same snapshot walk so concurrent eviction can't tear down the
  // supervisor between our snapshot read and the body forward. The
  // guard's `Drop` decrements the inflight counter — covers happy-
  // path body completion, abandoned client connections, and upstream
  // errors uniformly because the response body owns the guard.
  let (inflight_guard, request_model, resolved_backend) =
    match acquire_inflight_guard(state, port, served_model_key).await {
      Some(g) => g,
      None => {
        return Ok(error_envelope(
          StatusCode::BAD_GATEWAY,
          "upstream_unreachable",
          "model exited before forwarding could begin",
        ));
      }
    };
  // Compose upstream URL: path + query from the original request,
  // host always 127.0.0.1 (loopback only — see plan §Scope Boundaries).
  let path_and_query = inbound_uri
    .path_and_query()
    .map(|p| p.as_str())
    .unwrap_or("/");
  // Lemonade serves OpenAI under `/api/v1/...`; llama.cpp under `/v1/...`.
  // The prefix (if any) is prepended so the same forward path reaches both.
  let prefix = upstream_path_prefix.unwrap_or("");
  let upstream_url = format!("http://127.0.0.1:{port}{prefix}{path_and_query}");

  // Translate hyper::Method into reqwest::Method. Both crates share
  // the underlying `http` types so this is a structural conversion
  // rather than a string round-trip — and hyper has already validated
  // the inbound method before we reach this point, so the parse can't
  // fail in practice.
  let upstream_method = reqwest::Method::from_bytes(inbound_method.as_str().as_bytes())
    .expect("hyper-validated method round-trips to reqwest");

  // Forwarded headers: drop hop-by-hop entries and anything named in
  // the inbound `Connection: <list>` header (RFC 7230 §6.1 extends
  // the hop-by-hop set per-request).
  let connection_listed = collect_connection_listed(&inbound_headers);
  let mut outbound_headers = reqwest::header::HeaderMap::new();
  for (name, value) in inbound_headers.iter() {
    let n = name.as_str();
    if HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(n)) {
      continue;
    }
    if connection_listed.iter().any(|h| h.eq_ignore_ascii_case(n)) {
      continue;
    }
    // `host` would mislabel the upstream-side virtual host; reqwest
    // computes the correct host from the URL we hand it.
    if n.eq_ignore_ascii_case("host") {
      continue;
    }
    // `content-length` is recomputed by reqwest from the body we
    // pass in; skip the inbound value (in particular when it's `0`
    // for an empty body, the upstream still gets the right framing).
    if n.eq_ignore_ascii_case("content-length") {
      continue;
    }
    // Convert reqwest <- hyper. Both libs use the `http` crate's
    // `HeaderName` / `HeaderValue` so the bytes round-trip cleanly.
    let outbound_name = match reqwest::header::HeaderName::from_bytes(n.as_bytes()) {
      Ok(n) => n,
      Err(_) => continue,
    };
    let outbound_value = match reqwest::header::HeaderValue::from_bytes(value.as_bytes()) {
      Ok(v) => v,
      Err(_) => continue,
    };
    outbound_headers.append(outbound_name, outbound_value);
  }

  // Umbrella upstreams (prefixed path) use the unpooled client so no
  // keep-alive connection ever idles against the umbrella port — see
  // `ProxyState::umbrella_client` for the restart-wedge this avoids.
  let client = if upstream_path_prefix.is_some() {
    &state.umbrella_client
  } else {
    &state.http_client
  };
  // Two surgical body edits, both of which copy every untouched entry as the
  // client's own bytes: the launch's request-model name, then whatever the
  // serving backend needs the engine to understand (see
  // [`crate::backend::Backend::rewrite_request_body`]).
  let body = outbound_body(
    body_bytes,
    inbound_uri.path(),
    &resolved_backend,
    request_model.as_deref(),
  );
  let request = client
    .request(upstream_method, &upstream_url)
    .headers(outbound_headers)
    .body(body);

  let upstream = match request.send().await {
    Ok(r) => r,
    Err(err) => {
      // Connect refused / DNS / mid-handshake error before the
      // status line came back. The model was Ready a moment ago but
      // the kernel disagrees — surface as 502 with a recognisable
      // OpenAI body so clients can branch on it.
      return Ok(error_envelope(
        StatusCode::BAD_GATEWAY,
        "upstream_unreachable",
        &format!("failed to reach upstream llama-server: {err}"),
      ));
    }
  };

  build_streaming_response(
    upstream,
    served_model_id,
    fallback,
    fallback_reason,
    inflight_guard,
  )
}

/// Find the supervisor that owns `expected_id` on `port`, take an
/// inflight guard, and return it. Returns `None` when no Ready
/// supervisor matches — same condition the legacy `verify_port_binding`
/// caught, just folded into one snapshot walk so the gate and the
/// guard acquisition can't race against an eviction landing in
/// between.
async fn acquire_inflight_guard(
  state: &Arc<ProxyState>,
  port: u16,
  expected_id: &crate::gguf::identity::ModelId,
) -> Option<(
  crate::daemon::supervisor::InflightGuard,
  Option<String>,
  String,
)> {
  let snap = state.ctx.supervisors.snapshot().await;
  for (_lid, model) in snap {
    if model.port() != port {
      continue;
    }
    if model.id() != expected_id {
      continue;
    }
    if !matches!(
      model.state().await,
      crate::daemon::supervisor::ManagedState::Ready
    ) {
      continue;
    }
    let request_model = model
      .params()
      .launch_config
      .get(crate::backend::REQUEST_MODEL_KEY)
      .cloned();
    return Some((
      model.inflight_guard(),
      request_model,
      model.resolved_backend().to_string(),
    ));
  }
  None
}

/// The request body as sent upstream: `body` with the launch's request-model
/// name, then the serving backend's own rewrite for `endpoint`. An unknown
/// backend id forwards like a backend that rewrites nothing.
fn outbound_body(
  body: Bytes,
  endpoint: &str,
  backend_id: &str,
  request_model: Option<&str>,
) -> Bytes {
  use crate::backend::{Backend as _, Backends};
  let body = match request_model {
    Some(model) => with_model(body, model),
    None => body,
  };
  let rewritten =
    Backends::from_id(backend_id).and_then(|backend| backend.rewrite_request_body(endpoint, &body));
  match rewritten {
    Some(rewritten) => Bytes::from(rewritten),
    None => body,
  }
}

/// `body` with its top-level `model` set to `model`. Every other entry is
/// copied as its raw bytes, in order, so key order and nested content survive
/// and no `Value` tree is built for a multi-MB image body. A body that is not a
/// JSON object carrying `model` is returned unchanged.
fn with_model(body: Bytes, model: &str) -> Bytes {
  let Some(entries) = crate::util::json_body::entries(&body) else {
    return body;
  };
  if !entries.iter().any(|(key, _)| key == "model") {
    return body;
  }
  // Encoding a `&str` cannot fail.
  let model_value = serde_json::to_vec(&model).expect("string encodes");
  Bytes::from(crate::util::json_body::write_object(entries.iter().map(
    |(key, value)| {
      (
        key.as_str(),
        if key == "model" {
          model_value.as_slice()
        } else {
          value.get().as_bytes()
        },
      )
    },
  )))
}

/// Translate `reqwest::Response` into `hyper::Response`, preserving
/// status + headers (minus hop-by-hop) and piping the body chunks
/// through `StreamBody` so SSE chunks reach the client as they
/// arrive upstream.
fn build_streaming_response(
  upstream: reqwest::Response,
  served_model_id: &str,
  fallback: bool,
  fallback_reason: Option<&str>,
  inflight_guard: crate::daemon::supervisor::InflightGuard,
) -> ProxyResponse {
  let status = upstream.status();
  let inbound_headers = upstream.headers().clone();

  // `bytes_stream()` yields `Result<Bytes, reqwest::Error>`. Wrap
  // each `Bytes` in a `Frame::data` and box the reqwest error into
  // the proxy's wider `BodyError` type. When the upstream errors
  // mid-stream the StreamBody surfaces it as a frame error; hyper
  // drops the client connection in turn, which is the desired
  // "mid-stream upstream death" behaviour the plan calls for.
  let stream = upstream
    .bytes_stream()
    .map_ok(Frame::data)
    .map_err(|e| -> BodyError {
      log::debug!("proxy: upstream stream error: {e}");
      Box::new(e)
    });
  let stream_body = StreamBody::new(stream);
  let inner_body: BoxBody<Bytes, BodyError> = stream_body.boxed();
  // Attach the inflight guard to the streamed body. When the body is
  // dropped — happy-path completion, client disconnect, or upstream
  // error — the guard's `Drop` decrements the inflight counter so the
  // idle-TTL sweeper sees `inflight == 0` and can evict the
  // supervisor at the next sweep tick.
  let body: BoxBody<Bytes, BodyError> = GuardedBody {
    inner: inner_body,
    _guard: inflight_guard,
  }
  .boxed();

  // Strip the static hop-by-hop set AND anything named in the upstream's
  // own `Connection: <list>` header — RFC 7230 §6.1 extends hop-by-hop
  // per-message. Mirrors the request-side stripping above.
  let upstream_connection_listed = collect_connection_listed(&inbound_headers);
  let mut builder = Response::builder().status(status_to_hyper(status));
  if let Some(map) = builder.headers_mut() {
    for (name, value) in inbound_headers.iter() {
      let n = name.as_str();
      if HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(n)) {
        continue;
      }
      if upstream_connection_listed
        .iter()
        .any(|h| h.eq_ignore_ascii_case(n))
      {
        continue;
      }
      let Ok(hyper_name) = HeaderName::from_bytes(n.as_bytes()) else {
        continue;
      };
      let Ok(hyper_value) = HeaderValue::from_bytes(value.as_bytes()) else {
        continue;
      };
      map.append(hyper_name, hyper_value);
    }
    if fallback {
      // Sanitize served_model_id for the HeaderValue alphabet (visible
      // ASCII): non-ASCII bytes are replaced with `_` so the header
      // can never silently drop on a model with CJK/emoji in its name.
      let sanitized = sanitize_header_value(served_model_id);
      if let Ok(v) = HeaderValue::from_str(&sanitized) {
        map.insert(HeaderName::from_static("x-llamastash-served-by"), v);
      }
      if let Some(reason) = fallback_reason {
        if let Ok(v) = HeaderValue::from_str(reason) {
          map.insert(HeaderName::from_static("x-llamastash-fallback-reason"), v);
        }
      }
    }
  }

  Ok(builder.body(body).expect("static headers always parse"))
}

/// Body wrapper that holds an `InflightGuard` next to the streamed
/// upstream body. When hyper drops the response body — end-of-stream,
/// client disconnect, or pipeline tear-down — the guard field drops
/// with it and decrements the supervisor's inflight counter. This is
/// the single ownership chain that ties "request is being served" to
/// "supervisor is not idle"; the idle-TTL sweeper reads `inflight`
/// straight off the supervisor and skips eviction while it's > 0.
struct GuardedBody {
  inner: BoxBody<Bytes, BodyError>,
  _guard: crate::daemon::supervisor::InflightGuard,
}

impl hyper::body::Body for GuardedBody {
  type Data = Bytes;
  type Error = BodyError;

  fn poll_frame(
    self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
    // `GuardedBody` is `Unpin` (both fields are: `BoxBody` is a
    // `Pin<Box<…>>` wrapper, `InflightGuard` holds only an `Arc`), so
    // `get_mut` is safe and `Pin::new(inner)` re-pins for the inner
    // body's `poll_frame` contract.
    let inner = &mut self.get_mut().inner;
    std::pin::Pin::new(inner).poll_frame(cx)
  }

  fn is_end_stream(&self) -> bool {
    self.inner.is_end_stream()
  }

  fn size_hint(&self) -> hyper::body::SizeHint {
    self.inner.size_hint()
  }
}

/// reqwest exposes `StatusCode` from the `http` crate; hyper too.
/// They're the same type but re-exported, so we go via the wire
/// number for safety.
fn status_to_hyper(s: reqwest::StatusCode) -> hyper::StatusCode {
  hyper::StatusCode::from_u16(s.as_u16()).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR)
}

/// Parse a `Connection: <list>` header into a lower-case list of
/// header names that should be stripped per RFC 7230 §6.1.
fn collect_connection_listed(headers: &HeaderMap) -> Vec<String> {
  headers
    .get_all(hyper::header::CONNECTION)
    .iter()
    .filter_map(|v| v.to_str().ok())
    .flat_map(|s| s.split(','))
    .map(|tok| tok.trim().to_ascii_lowercase())
    .filter(|tok| !tok.is_empty())
    .collect()
}

/// Coerce an arbitrary string into something `HeaderValue::from_str`
/// will accept (visible ASCII, no control characters). Non-ASCII
/// bytes and ASCII controls are replaced with `_` so a model with
/// CJK / emoji / whitespace in its display name still produces a
/// usable `x-llamastash-served-by` header instead of silently
/// dropping it.
fn sanitize_header_value(input: &str) -> String {
  input
    .chars()
    .map(|c| {
      if (' '..='~').contains(&c) && c != '\u{007f}' {
        c
      } else {
        '_'
      }
    })
    .collect()
}

/// Construct an OpenAI-shaped error response for the forwarding arm's
/// upstream-unreachable (502) cases, sharing the router's
/// `json_response` builder so the envelope shape stays identical.
fn error_envelope(
  status: StatusCode,
  kind: &str,
  message: &str,
) -> Response<BoxBody<Bytes, BodyError>> {
  let envelope = super::openai::ErrorResponse {
    error: super::openai::ErrorObject::new(kind, message),
  };
  let bytes = serde_json::to_vec(&envelope).expect("json encoding of fixed shape");
  super::router::json_response(status, bytes)
}

/// Helper to massage a hyper::Request<Incoming> into the parts the
/// forwarding fn wants. Pulled out so the router's match arms stay
/// short.
pub(crate) fn deconstruct(
  req: Request<hyper::body::Incoming>,
) -> (Method, hyper::Uri, HeaderMap, hyper::body::Incoming) {
  let (parts, body) = req.into_parts();
  (parts.method, parts.uri, parts.headers, body)
}

#[cfg(test)]
mod tests {
  use super::*;
  use http_body_util::BodyExt;

  #[test]
  fn with_model_replaces_only_a_top_level_model() {
    let out = with_model(
      Bytes::from(r#"{"model":"q@other","messages":[{"model":"keep"}],"stream":true}"#),
      "q",
    );
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["model"], "q");
    assert_eq!(v["messages"][0]["model"], "keep");
    assert_eq!(v["stream"], true);

    assert_eq!(
      with_model(Bytes::from(r#"{"z":1,"model":"a","b":{"y":1,"x":2}}"#), "q"),
      r#"{"z":1,"model":"q","b":{"y":1,"x":2}}"#.as_bytes(),
      "key order survives the rewrite"
    );

    for untouched in [r#"{"messages":[]}"#, "not json", "[1]", ""] {
      assert_eq!(
        with_model(Bytes::from(untouched), "q"),
        untouched.as_bytes()
      );
    }
  }

  #[test]
  fn outbound_body_forwards_client_bytes_when_there_is_nothing_to_rewrite() {
    // No effort field and no request-model pin: the endpoint hook has nothing
    // to map, so the client's exact bytes reach the engine.
    for body in [
      r#"{"model":"q","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
      r#"{"thinking":{"type":"adaptive"},"stream": true ,"output_config":{"format":{}}}"#,
    ] {
      assert_eq!(
        outbound_body(
          Bytes::from(body),
          "/v1/messages",
          crate::backend::DEFAULT_BACKEND_ID,
          None,
        ),
        body.as_bytes(),
        "{body}"
      );
    }
  }

  #[test]
  fn outbound_body_maps_effort_only_for_the_backend_that_needs_it() {
    assert_eq!(
      outbound_body(
        Bytes::from(r#"{"model":"other","output_config":{"effort":"xhigh"}}"#),
        "/v1/messages",
        crate::backend::DEFAULT_BACKEND_ID,
        Some("q"),
      ),
      r#"{"model":"q","output_config":{"effort":"xhigh"},"chat_template_kwargs":{"reasoning_effort":"xhigh"}}"#
        .as_bytes(),
      "the request-model pin and the effort mapping ride the same rewrite"
    );
    for endpoint in ["/v1/chat/completions", "/v1/messages/count_tokens"] {
      assert_eq!(
        outbound_body(
          Bytes::from(r#"{"output_config":{"effort":"xhigh"}}"#),
          endpoint,
          crate::backend::DEFAULT_BACKEND_ID,
          None,
        ),
        r#"{"output_config":{"effort":"xhigh"}}"#.as_bytes(),
        "{endpoint} is not the effort surface"
      );
    }
    let untouched = r#"{"output_config":{"effort":"xhigh"}}"#;
    // Every registered backend except the one whose engine drops the field
    // forwards the client's bytes; so does an unknown backend id.
    use crate::backend::{Backend as _, Backends};
    let others = Backends::all()
      .into_iter()
      .map(|backend| backend.id())
      .filter(|id| *id != crate::backend::DEFAULT_BACKEND_ID)
      .chain(std::iter::once("not-a-backend"));
    for backend_id in others {
      assert_eq!(
        outbound_body(Bytes::from(untouched), "/v1/messages", backend_id, None,),
        untouched.as_bytes(),
        "{backend_id} rewrites nothing"
      );
    }
  }

  #[test]
  fn sanitize_header_value_replaces_non_visible_ascii() {
    // Visible ASCII passes through unchanged.
    assert_eq!(sanitize_header_value("gemma-3-4b"), "gemma-3-4b");
    // CJK, emoji, and ASCII controls all collapse to `_` so the header
    // stays `HeaderValue::from_str`-acceptable.
    assert_eq!(sanitize_header_value("模型"), "__");
    assert_eq!(sanitize_header_value("a\tb\nc"), "a_b_c");
    // DEL (0x7f) is explicitly excluded from the visible range.
    assert_eq!(sanitize_header_value("x\u{007f}y"), "x_y");
    // The result is always a valid header value.
    assert!(HeaderValue::from_str(&sanitize_header_value("模型 🚀")).is_ok());
  }

  #[test]
  fn collect_connection_listed_splits_and_lowercases() {
    let mut headers = HeaderMap::new();
    headers.insert(
      hyper::header::CONNECTION,
      HeaderValue::from_static("Keep-Alive, X-Custom"),
    );
    let listed = collect_connection_listed(&headers);
    assert_eq!(
      listed,
      vec!["keep-alive".to_string(), "x-custom".to_string()]
    );
  }

  #[test]
  fn collect_connection_listed_drops_empty_tokens() {
    // A trailing comma / stray whitespace must not yield empty entries
    // that would then strip a header named "".
    let mut headers = HeaderMap::new();
    headers.insert(
      hyper::header::CONNECTION,
      HeaderValue::from_static("close, , upgrade"),
    );
    let listed = collect_connection_listed(&headers);
    assert_eq!(listed, vec!["close".to_string(), "upgrade".to_string()]);
  }

  #[test]
  fn collect_connection_listed_empty_when_header_absent() {
    assert!(collect_connection_listed(&HeaderMap::new()).is_empty());
  }

  #[test]
  fn status_to_hyper_round_trips_known_status() {
    assert_eq!(
      status_to_hyper(reqwest::StatusCode::NOT_FOUND),
      hyper::StatusCode::NOT_FOUND
    );
    assert_eq!(
      status_to_hyper(reqwest::StatusCode::OK),
      hyper::StatusCode::OK
    );
  }

  #[tokio::test]
  async fn error_envelope_builds_openai_shaped_body() {
    // The 502 forwarding-arm error must carry the OpenAI `{error:{...}}`
    // envelope shape so SDK clients surface it as a structured error.
    let resp = error_envelope(
      StatusCode::BAD_GATEWAY,
      "upstream_unreachable",
      "model exited before forwarding could begin",
    );
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = resp
      .into_body()
      .collect()
      .await
      .expect("collect")
      .to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["error"]["type"], "upstream_unreachable");
    assert_eq!(
      v["error"]["message"],
      "model exited before forwarding could begin"
    );
  }
}
