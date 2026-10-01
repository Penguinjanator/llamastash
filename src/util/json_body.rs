//! Byte-surgical edits to a JSON object body.
//!
//! The proxy forwards a client's body to the engine and every byte it does
//! not have to change has to stay as it arrived: bodies carry multi-MB image
//! payloads, and re-serializing through [`serde_json::Value`] builds a full
//! tree and re-escapes untouched strings. These helpers read an object's
//! top-level entries and write them back with each value copied as its raw
//! bytes, so only the entries a caller replaces change.
//!
//! Compacting: the writer emits `{"k":v,...}` with no inter-entry whitespace,
//! and a raw value is the value's own text with surrounding whitespace dropped.
//! So a rebuilt object comes out compact, while any value this edit does not
//! touch keeps its bytes exactly — a nested object with its own spacing
//! included.

use serde::de::{Deserializer as _, MapAccess, Visitor};
use serde_json::value::RawValue;

/// Top-level `(key, raw value)` pairs of a JSON object body, borrowed from
/// `body`. `None` when `body` is not a JSON object (an array, a scalar, or
/// anything that does not parse) or has trailing content after it.
pub fn entries(body: &[u8]) -> Option<Vec<(String, &RawValue)>> {
  struct Collect;
  impl<'de> Visitor<'de> for Collect {
    type Value = Vec<(String, &'de RawValue)>;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.write_str("a JSON object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
      let mut out = Vec::new();
      while let Some(entry) = map.next_entry::<String, &'de RawValue>()? {
        out.push(entry);
      }
      Ok(out)
    }
  }

  let mut de = serde_json::Deserializer::from_slice(body);
  de.deserialize_map(Collect)
    .and_then(|e| de.end().map(|()| e))
    .ok()
}

/// `{"key":raw,...}` from `entries`, keys serialized as JSON strings and
/// values copied as the raw bytes given, in iterator order.
pub fn write_object<'a, I>(entries: I) -> Vec<u8>
where
  I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
  let mut out = Vec::from(*b"{");
  for (i, (key, raw)) in entries.into_iter().enumerate() {
    if i > 0 {
      out.push(b',');
    }
    // Writing a `&str` into a `Vec<u8>` cannot fail.
    let _ = serde_json::to_writer(&mut out, key);
    out.push(b':');
    out.extend_from_slice(raw);
  }
  out.push(b'}');
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn entries_reads_top_level_pairs_in_order() {
    let got = entries(br#"{"b":1,"a":{"nested":[1,2]},"s":"x" }"#).expect("object");
    let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["b", "a", "s"]);
    assert_eq!(got[1].1.get(), r#"{"nested":[1,2]}"#);
  }

  #[test]
  fn entries_none_for_non_objects() {
    for not_object in ["[1]", "1", "\"x\"", "not json", "", "{"] {
      assert!(entries(not_object.as_bytes()).is_none(), "{not_object}");
    }
    // Trailing content after the object is not a JSON object body either.
    assert!(entries(br#"{"a":1}trailing"#).is_none());
  }

  #[test]
  fn write_object_copies_values_and_keeps_order() {
    let out = write_object([("z", b"1".as_slice()), ("b", br#"{"y": 1, "x": 2}"#)]);
    // Nested raw bytes are copied verbatim, inner whitespace included.
    assert_eq!(out, br#"{"z":1,"b":{"y": 1, "x": 2}}"#);
    assert_eq!(write_object([]), br#"{}"#);
  }

  #[test]
  fn write_object_escapes_keys() {
    assert_eq!(
      write_object([("a\"b", br#""v""#.as_slice())]),
      br#"{"a\"b":"v"}"#
    );
  }
}
