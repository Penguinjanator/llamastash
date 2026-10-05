//! Reads token counts and generation speed out of a response as it
//! streams through the proxy.
//!
//! The proxy forwards response bytes untouched and does not buffer them,
//! so the tap keeps only the first `HEAD_CAP` and the last `TAIL_CAP`
//! bytes and looks for three objects by key once the response has ended:
//!
//! - `usage`: token counts (OpenAI `prompt_tokens` / `completion_tokens`,
//!   Anthropic and Responses `input_tokens` / `output_tokens`), and
//!   `completion_tokens_per_second` on servers that report speed there.
//! - `timings`: the server's own counters (`prompt_n`, `cache_n`,
//!   `predicted_n`) and `predicted_per_second`.
//! - `metrics`: `tokens_per_second`, on servers that attach per-request
//!   metrics.
//!
//! Matching is on field names, never on which backend answered. The head
//! is kept because an Anthropic stream reports its input tokens in the
//! first event (`message_start`) and only the output tokens at the end.

use serde_json::{Map, Value};

/// Bytes kept from the start of a response.
pub(crate) const HEAD_CAP: usize = 4 * 1024;
/// Bytes kept from the end of a response.
pub(crate) const TAIL_CAP: usize = 8 * 1024;

/// What the tap read. A field the response did not carry is `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
  /// Whole prompt, cached tokens included.
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  /// Generation speed: what the server reported, or the proxy's estimate
  /// when [`Self::tokens_per_second_estimated`] is set.
  pub tokens_per_second: Option<f64>,
  /// The speed was worked out by [`Self::estimate_speed`].
  pub tokens_per_second_estimated: bool,
}

impl Usage {
  /// When the server reported no speed, estimate it as the generated
  /// tokens over `window`, the time generation took by the proxy's clock.
  /// A server-reported speed is never replaced, and without a token count
  /// there is nothing to estimate from.
  pub fn estimate_speed(&mut self, window: std::time::Duration) {
    let secs = window.as_secs_f64();
    match self.completion_tokens {
      Some(tokens) if self.tokens_per_second.is_none() && tokens > 0 && secs > 0.0 => {
        self.tokens_per_second = Some(tokens as f64 / secs);
        self.tokens_per_second_estimated = true;
      }
      _ => {}
    }
  }
}

#[derive(Default)]
pub(crate) struct ResponseTap {
  head: Vec<u8>,
  tail: Vec<u8>,
  /// Bytes between `head` and `tail` were discarded.
  gap: bool,
}

impl ResponseTap {
  pub(crate) fn push(&mut self, chunk: &[u8]) {
    let into_head = HEAD_CAP.saturating_sub(self.head.len()).min(chunk.len());
    self.head.extend_from_slice(&chunk[..into_head]);
    let rest = &chunk[into_head..];
    if rest.is_empty() {
      return;
    }
    // A chunk that alone fills the tail replaces it, so a multi-megabyte
    // body costs one `TAIL_CAP` copy per chunk, not a copy of every byte.
    if rest.len() >= TAIL_CAP {
      self.gap |= !self.tail.is_empty() || rest.len() > TAIL_CAP;
      self.tail.clear();
      self.tail.extend_from_slice(&rest[rest.len() - TAIL_CAP..]);
      return;
    }
    self.tail.extend_from_slice(rest);
    // Trim at twice the cap so the front is not shifted on every chunk.
    if self.tail.len() > 2 * TAIL_CAP {
      let cut = self.tail.len() - TAIL_CAP;
      self.tail.drain(..cut);
      self.gap = true;
    }
  }

  pub(crate) fn usage(&self) -> Usage {
    let mut fields = Fields::default();
    if self.gap {
      fields.scan(&self.head);
      fields.scan(&self.tail);
    } else {
      // Nothing was discarded, so the two halves are one contiguous body
      // and an object may straddle the join.
      fields.scan(&[self.head.as_slice(), self.tail.as_slice()].concat());
    }
    fields.usage()
  }
}

/// Every field read so far. A later object overrides an earlier one field
/// by field, so a stream's last `usage` wins without erasing what only an
/// earlier event carried.
#[derive(Default)]
struct Fields {
  prompt_tokens: Option<u64>,
  input_tokens: Option<u64>,
  cache_read_input_tokens: Option<u64>,
  cache_creation_input_tokens: Option<u64>,
  completion_tokens: Option<u64>,
  usage_tps: Option<f64>,
  prompt_n: Option<u64>,
  cache_n: Option<u64>,
  predicted_n: Option<u64>,
  timings_tps: Option<f64>,
  metrics_tps: Option<f64>,
}

impl Fields {
  fn scan(&mut self, buf: &[u8]) {
    for obj in objects_keyed(buf, b"\"usage\"") {
      set(&mut self.prompt_tokens, count(&obj, "prompt_tokens"));
      set(&mut self.input_tokens, count(&obj, "input_tokens"));
      set(
        &mut self.cache_read_input_tokens,
        count(&obj, "cache_read_input_tokens"),
      );
      set(
        &mut self.cache_creation_input_tokens,
        count(&obj, "cache_creation_input_tokens"),
      );
      set(
        &mut self.completion_tokens,
        count(&obj, "completion_tokens").or_else(|| count(&obj, "output_tokens")),
      );
      set(
        &mut self.usage_tps,
        rate(&obj, "completion_tokens_per_second"),
      );
    }
    for obj in objects_keyed(buf, b"\"timings\"") {
      set(&mut self.prompt_n, count(&obj, "prompt_n"));
      set(&mut self.cache_n, count(&obj, "cache_n"));
      set(&mut self.predicted_n, count(&obj, "predicted_n"));
      set(&mut self.timings_tps, rate(&obj, "predicted_per_second"));
    }
    for obj in objects_keyed(buf, b"\"metrics\"") {
      set(&mut self.metrics_tps, rate(&obj, "tokens_per_second"));
    }
  }

  fn usage(&self) -> Usage {
    // OpenAI's `prompt_tokens` and the Responses API's `input_tokens`
    // count the whole prompt. Anthropic's `input_tokens` leaves out the
    // cached part and reports it in the two `cache_*` fields, which the
    // other shapes do not have, so adding them is right for all three.
    let from_usage = self.prompt_tokens.or_else(|| {
      self.input_tokens.map(|n| {
        n.saturating_add(self.cache_read_input_tokens.unwrap_or(0))
          .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
      })
    });
    // `prompt_n` is what the server evaluated, `cache_n` what it reused.
    let from_timings = match (self.prompt_n, self.cache_n) {
      (None, None) => None,
      (evaluated, cached) => Some(evaluated.unwrap_or(0).saturating_add(cached.unwrap_or(0))),
    };
    Usage {
      prompt_tokens: from_usage.or(from_timings),
      completion_tokens: self.completion_tokens.or(self.predicted_n),
      tokens_per_second: self.timings_tps.or(self.usage_tps).or(self.metrics_tps),
      tokens_per_second_estimated: false,
    }
  }
}

fn set<T>(slot: &mut Option<T>, value: Option<T>) {
  if value.is_some() {
    *slot = value;
  }
}

fn count(obj: &Map<String, Value>, key: &str) -> Option<u64> {
  let v = obj.get(key)?;
  v.as_u64().or_else(|| {
    v.as_f64()
      .filter(|f| f.is_finite() && *f >= 0.0)
      .map(|f| f as u64)
  })
}

fn rate(obj: &Map<String, Value>, key: &str) -> Option<f64> {
  obj.get(key)?.as_f64().filter(|f| f.is_finite() && *f > 0.0)
}

/// Every JSON object in `buf` that is the value of `quoted_key` (the key
/// with its quotes), in order. An occurrence whose value is not a complete
/// object (`null`, or cut off by the edge of the buffer) is skipped.
///
/// A match can only be a real key: inside a JSON string the closing quote
/// of the needle would be escaped, and a string *value* equal to the key
/// is followed by `,` or `}`, not `:`.
fn objects_keyed(buf: &[u8], quoted_key: &[u8]) -> Vec<Map<String, Value>> {
  let mut found = Vec::new();
  let mut from = 0;
  while let Some(at) = buf[from..]
    .windows(quoted_key.len())
    .position(|w| w == quoted_key)
  {
    from += at + quoted_key.len();
    let rest = skip_ws(&buf[from..]);
    let Some(rest) = rest.strip_prefix(b":") else {
      continue;
    };
    let rest = skip_ws(rest);
    if !rest.starts_with(b"{") {
      continue;
    }
    if let Some(Ok(obj)) = serde_json::Deserializer::from_slice(rest)
      .into_iter::<Map<String, Value>>()
      .next()
    {
      found.push(obj);
    }
  }
  found
}

fn skip_ws(buf: &[u8]) -> &[u8] {
  let n = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
  &buf[n..]
}

#[cfg(test)]
mod tests {
  use super::*;

  fn usage_of(body: &str) -> Usage {
    let mut tap = ResponseTap::default();
    tap.push(body.as_bytes());
    tap.usage()
  }

  // The bodies below are responses captured from llama-server b11390
  // (2026-10-05), with the model path shortened.

  #[test]
  fn chat_non_streamed() {
    let body = r#"{"choices":[{"finish_reason":"stop","index":0,"message":{"role":"assistant","content":"Hi, How are you?"}}],"created":1791206934,"model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion","usage":{"completion_tokens":7,"prompt_tokens":41,"total_tokens":48,"prompt_tokens_details":{"cached_tokens":0}},"id":"chatcmpl-19uf","timings":{"cache_n":0,"prompt_n":41,"prompt_ms":149.353,"prompt_per_token_ms":3.6427560975609756,"prompt_per_second":274.5174184649789,"predicted_n":7,"predicted_ms":151.366,"predicted_per_token_ms":25.227666666666668,"predicted_per_second":39.639020651929755}}"#;
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: Some(39.639020651929755),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn chat_streamed_has_timings_but_no_usage() {
    let body = concat!(
      r#"data: {"choices":[{"finish_reason":null,"index":0,"delta":{"content":"Hi"}}],"created":1791206934,"id":"chatcmpl-Vs5P","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk"}"#,
      "\n\n",
      r#"data: {"choices":[{"finish_reason":"stop","index":0,"delta":{}}],"created":1791206934,"id":"chatcmpl-Vs5P","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk","timings":{"cache_n":40,"prompt_n":1,"prompt_ms":25.704,"prompt_per_token_ms":25.704,"prompt_per_second":38.90445066915655,"predicted_n":3,"predicted_ms":51.759,"predicted_per_token_ms":25.8795,"predicted_per_second":38.64062288684094}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    // No `usage` object: the counts come from `timings`, and the prompt
    // is the evaluated token plus the 40 cached ones.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(3),
        tokens_per_second: Some(38.64062288684094),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn chat_streamed_with_include_usage() {
    let body = concat!(
      r#"data: {"choices":[],"created":1791206935,"id":"chatcmpl-tTRS","model":"m.gguf","system_fingerprint":"b11390-dd266785c","object":"chat.completion.chunk","usage":{"completion_tokens":7,"prompt_tokens":41,"total_tokens":48,"prompt_tokens_details":{"cached_tokens":40}},"timings":{"cache_n":40,"prompt_n":1,"prompt_ms":26.4,"prompt_per_token_ms":26.4,"prompt_per_second":37.87878787878788,"predicted_n":7,"predicted_ms":153.356,"predicted_per_token_ms":25.55933333333333,"predicted_per_second":39.12465113852735}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: Some(39.12465113852735),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn anthropic_messages_streamed() {
    let body = concat!(
      "event: message_start\n",
      r#"data: {"type":"message_start","message":{"id":"chatcmpl-x","type":"message","role":"assistant","content":[],"model":"m.gguf","stop_reason":null,"stop_sequence":null,"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":0}}}"#,
      "\n\nevent: content_block_delta\n",
      r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
      "\n\nevent: message_delta\n",
      r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}"#,
      "\n\nevent: message_stop\n",
      r#"data: {"type":"message_stop"}"#,
      "\n\n",
    );
    // Input tokens come from the first event, output tokens from the
    // last, and this surface carries no speed.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(4),
        tokens_per_second: None,
        ..Usage::default()
      }
    );
  }

  #[test]
  fn anthropic_messages_non_streamed() {
    let body = r#"{"id":"chatcmpl-11kF","type":"message","role":"assistant","content":[{"type":"text","text":"Hi, How are you?"}],"model":"m.gguf","stop_reason":"end_turn","stop_sequence":null,"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":7}}"#;
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(7),
        tokens_per_second: None,
        ..Usage::default()
      }
    );
  }

  #[test]
  fn responses_streamed() {
    let body = concat!(
      "event: response.completed\n",
      r#"data: {"type":"response.completed","response":{"id":"resp_k7MN","object":"response","created_at":1791206936,"status":"completed","model":"m.gguf","output":[{"type":"message","status":"completed","id":"msg_p0CB","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":"Hello friend."}],"role":"assistant"}],"usage":{"input_tokens":41,"output_tokens":4,"total_tokens":45,"input_tokens_details":{"cached_tokens":40}}},"timings":{"cache_n":40,"prompt_n":1,"prompt_ms":22.405,"prompt_per_token_ms":22.405,"prompt_per_second":44.63289444320464,"predicted_n":4,"predicted_ms":75.864,"predicted_per_token_ms":25.288,"predicted_per_second":39.544447959506485}}"#,
      "\n\n",
    );
    // The Responses `input_tokens` already counts the cached tokens.
    assert_eq!(
      usage_of(body),
      Usage {
        prompt_tokens: Some(41),
        completion_tokens: Some(4),
        tokens_per_second: Some(39.544447959506485),
        ..Usage::default()
      }
    );
  }

  #[test]
  fn speed_is_estimated_only_when_the_server_reported_none() {
    use std::time::Duration;
    let mut counted = Usage {
      completion_tokens: Some(30),
      ..Usage::default()
    };
    counted.estimate_speed(Duration::from_millis(1500));
    assert_eq!(counted.tokens_per_second, Some(20.0));
    assert!(counted.tokens_per_second_estimated);

    let mut reported = Usage {
      completion_tokens: Some(30),
      tokens_per_second: Some(41.5),
      ..Usage::default()
    };
    reported.estimate_speed(Duration::from_millis(1500));
    assert_eq!(reported.tokens_per_second, Some(41.5));
    assert!(!reported.tokens_per_second_estimated);

    // No token count, no tokens, or no elapsed time: nothing to divide.
    for (tokens, window) in [
      (None, Duration::from_secs(1)),
      (Some(0), Duration::from_secs(1)),
      (Some(30), Duration::ZERO),
    ] {
      let mut usage = Usage {
        completion_tokens: tokens,
        ..Usage::default()
      };
      usage.estimate_speed(window);
      assert_eq!(usage.tokens_per_second, None, "{tokens:?} {window:?}");
      assert!(!usage.tokens_per_second_estimated);
    }
  }

  #[test]
  fn speed_is_read_from_usage_and_from_metrics_when_timings_is_absent() {
    let in_usage = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20,"completion_tokens_per_second":55.5}}"#;
    assert_eq!(usage_of(in_usage).tokens_per_second, Some(55.5));
    let in_metrics = r#"{"usage":{"prompt_tokens":10,"completion_tokens":20},"metrics":{"time_to_first_token_ms":12.0,"tokens_per_second":61.25}}"#;
    assert_eq!(usage_of(in_metrics).tokens_per_second, Some(61.25));
  }

  #[test]
  fn a_null_usage_on_earlier_chunks_is_skipped() {
    let body = concat!(
      r#"data: {"choices":[{"delta":{"content":"a"}}],"usage":null}"#,
      "\n\n",
      r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
      "\n\ndata: [DONE]\n\n",
    );
    let u = usage_of(body);
    assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(5), Some(2)));
  }

  #[test]
  fn usage_text_inside_the_reply_is_not_read() {
    // The model wrote a `usage` object in its answer. In the JSON body
    // its quotes are escaped, so it is not a key.
    let body = r#"{"choices":[{"message":{"content":"{\"usage\":{\"prompt_tokens\":999,\"completion_tokens\":999}}"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
    let u = usage_of(body);
    assert_eq!((u.prompt_tokens, u.completion_tokens), (Some(5), Some(2)));
    let only_text =
      r#"{"choices":[{"message":{"content":"{\"usage\":{\"prompt_tokens\":999}}"}}]}"#;
    assert_eq!(usage_of(only_text), Usage::default());
  }

  #[test]
  fn a_body_without_the_fields_reads_as_nothing() {
    assert_eq!(usage_of(""), Usage::default());
    assert_eq!(
      usage_of(r#"{"error":{"message":"boom"}}"#),
      Usage::default()
    );
    assert_eq!(
      usage_of(r#"{"usage":"none","timings":[1,2]}"#),
      Usage::default()
    );
    assert_eq!(usage_of(r#"{"usage":{"prompt_tokens":"#), Usage::default());
  }

  #[test]
  fn fields_survive_any_chunking() {
    let body = concat!(
      "event: message_start\n",
      r#"data: {"type":"message_start","message":{"usage":{"cache_read_input_tokens":40,"input_tokens":1,"output_tokens":0}}}"#,
      "\n\n",
      r#"data: {"type":"message_delta","usage":{"output_tokens":4}}"#,
      "\n\n",
    );
    for size in [1, 2, 7, 64] {
      let mut tap = ResponseTap::default();
      for chunk in body.as_bytes().chunks(size) {
        tap.push(chunk);
      }
      let u = tap.usage();
      assert_eq!(
        (u.prompt_tokens, u.completion_tokens),
        (Some(41), Some(4)),
        "chunk size {size}"
      );
    }
  }

  #[test]
  fn a_long_stream_keeps_the_first_event_and_the_last() {
    let mut tap = ResponseTap::default();
    tap.push(
      br#"data: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0}}}"#,
    );
    let delta = br#"data: {"type":"content_block_delta","delta":{"text":"word "}}"#;
    for _ in 0..2000 {
      tap.push(delta);
      tap.push(b"\n\n");
    }
    tap.push(br#"data: {"type":"message_delta","usage":{"output_tokens":2000}}"#);
    assert!(tap.gap);
    assert!(tap.head.len() <= HEAD_CAP);
    assert!(tap.tail.len() <= 2 * TAIL_CAP);
    let u = tap.usage();
    assert_eq!(
      (u.prompt_tokens, u.completion_tokens),
      (Some(12), Some(2000))
    );
  }

  #[test]
  fn a_large_single_chunk_keeps_only_its_tail() {
    let mut body = vec![b' '; 3 * 1024 * 1024];
    body.extend_from_slice(br#"{"usage":{"prompt_tokens":9,"completion_tokens":1}}"#);
    let mut tap = ResponseTap::default();
    tap.push(&body);
    assert_eq!(tap.head.len(), HEAD_CAP);
    assert_eq!(tap.tail.len(), TAIL_CAP);
    assert_eq!(tap.usage().prompt_tokens, Some(9));
  }

  #[test]
  fn an_object_across_the_head_and_tail_join_is_read() {
    let mut body = vec![b' '; HEAD_CAP - 10];
    body.extend_from_slice(br#"{"usage":{"prompt_tokens":9,"completion_tokens":1}}"#);
    let mut tap = ResponseTap::default();
    tap.push(&body);
    assert!(!tap.gap);
    assert_eq!(tap.usage().prompt_tokens, Some(9));
  }
}
