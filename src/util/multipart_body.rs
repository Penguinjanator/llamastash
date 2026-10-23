//! Byte-level reads and edits on a `multipart/form-data` body.
//!
//! The proxy has to know which model a `POST /v1/audio/transcriptions` names
//! before it can route it, and it has to be able to replace that name when it
//! answers for a name it does not know. Neither is a reason to pull in a
//! multipart crate: the proxy forwards the body verbatim, so what it needs is a
//! byte range, not a decoded tree. A parsed-and-re-serialized part loses its
//! filename quoting, its `Content-Type`, and its exact bytes — and file parts
//! carry the client's audio, which must go upstream untouched.
//!
//! Delimiters follow RFC 2046: each part starts after `--<boundary>` and a line
//! ending, and a part's content ends at the line ending before the next
//! delimiter. A bare LF is accepted where the spec requires CRLF, because
//! clients in the wild emit it.

use std::ops::Range;

/// Whether a `Content-Type` names a `multipart/form-data` body.
pub fn is_multipart(content_type: &str) -> bool {
  let rest = content_type.trim_start();
  rest
    .get(.."multipart/form-data".len())
    .is_some_and(|head| head.eq_ignore_ascii_case("multipart/form-data"))
    && matches!(
      rest.as_bytes().get("multipart/form-data".len()),
      None | Some(b';')
    )
}

/// The `boundary` parameter of a `multipart/form-data` `Content-Type`, with any
/// surrounding quotes removed. `None` when the header names none.
pub fn boundary(content_type: &str) -> Option<&str> {
  // ASCII-lowercasing keeps byte indices aligned with the original.
  let lower = content_type.to_ascii_lowercase();
  let at = lower.find("boundary=")?;
  let rest = content_type[at + "boundary=".len()..].trim_start();
  match rest.strip_prefix('"') {
    Some(rest) => Some(&rest[..rest.find('"')?]),
    None => {
      let end = rest
        .find([';', ' ', '\t', '\r', '\n'])
        .unwrap_or(rest.len());
      (!rest[..end].is_empty()).then(|| &rest[..end])
    }
  }
}

/// Byte range of the value of the form field `name`, or `None` when
/// `content_type` carries no boundary or the body carries no such field. A file
/// part of the same name is not a field value; only a part with no `filename`
/// counts.
pub fn field_range(body: &[u8], content_type: &str, name: &str) -> Option<Range<usize>> {
  let delimiter = format!("--{}", boundary(content_type)?);
  let delimiter = delimiter.as_bytes();
  let mut pos = 0;
  while let Some(off) = find_bytes(&body[pos..], delimiter) {
    let after = pos + off + delimiter.len();
    pos = after;
    let rest = body.get(after..)?;
    // `--<boundary>--` closes the body and carries no part.
    if rest.starts_with(b"--") {
      break;
    }
    // A delimiter with no line ending after it is a copy inside a binary part.
    let Some(lead) = line_end_len(rest) else {
      continue;
    };
    let mut line = after + lead;
    let mut named = false;
    let mut is_file = false;
    let value_start = loop {
      let (len, term) = line_end(body.get(line..).unwrap_or_default())?;
      if len == 0 {
        break line + term;
      }
      if is_param_line(&body[line..line + len], "content-disposition") {
        let line = &body[line..line + len];
        named |= has_param(line, b"name=", Some(name.as_bytes()));
        is_file |= has_param(line, b"filename=", None);
      }
      line += len + term;
    };
    let found = find_bytes(&body[value_start..], delimiter);
    if named && !is_file {
      let end = match found {
        Some(at) => content_end(body, value_start + at),
        None => body.len(),
      };
      return (value_start <= end).then_some(value_start..end);
    }
  }
  None
}

/// The value of the form field `name`, or `None` when there is no such field.
pub fn field_value<'a>(body: &'a [u8], content_type: &str, name: &str) -> Option<&'a [u8]> {
  body.get(field_range(body, content_type, name)?)
}

/// `body` with the value of the form field `name` replaced by `value`, every
/// other byte kept. `None` when the body carries no such field.
pub fn replace_field(body: &[u8], content_type: &str, name: &str, value: &[u8]) -> Option<Vec<u8>> {
  let range = field_range(body, content_type, name)?;
  let mut out = Vec::with_capacity(body.len() + value.len());
  out.extend_from_slice(&body[..range.start]);
  out.extend_from_slice(value);
  out.extend_from_slice(&body[range.end..]);
  Some(out)
}

/// `(length, terminator length)` of the line at the front of `line`; a length of
/// `0` is the blank line that ends a header block.
fn line_end(line: &[u8]) -> Option<(usize, usize)> {
  match (find_bytes(line, b"\r\n"), find_bytes(line, b"\n")) {
    (Some(c), Some(l)) => Some(if c < l { (c, 2) } else { (l, 1) }),
    (Some(c), None) => Some((c, 2)),
    (None, Some(l)) => Some((l, 1)),
    (None, None) => None,
  }
}

fn line_end_len(line: &[u8]) -> Option<usize> {
  line_end(line).map(|(_, term)| term)
}

/// Where a part's content ends: the line ending that belongs to the next
/// delimiter is not part of the content.
fn content_end(body: &[u8], end: usize) -> usize {
  let last = end.checked_sub(1).and_then(|i| body.get(i));
  let before = end.checked_sub(2).and_then(|i| body.get(i));
  match (before, last) {
    (Some(&b'\r'), Some(&b'\n')) => end - 2,
    (_, Some(&b'\n')) => end - 1,
    _ => end,
  }
}

/// Whether `line` is the `head` header, ignoring case, with nothing but the
/// colon after its name.
fn is_param_line(line: &[u8], head: &str) -> bool {
  line
    .get(..head.len())
    .is_some_and(|got| got.eq_ignore_ascii_case(head.as_bytes()))
    && line.get(head.len()) == Some(&b':')
}

/// Whether a `Content-Disposition` line carries the parameter `key`, and with
/// `want` set a value equal to it. Field names are case-sensitive; a `filename`
/// marks a file part, whose content is bytes and not a field value.
fn has_param(line: &[u8], key: &[u8], want: Option<&[u8]>) -> bool {
  let mut from = 0;
  while let Some(off) = find_bytes(&line[from..], key) {
    let at = from + off;
    from = at + key.len();
    // Must be a parameter of its own, not the tail of `filename=`.
    if at > 0 && !matches!(line[at - 1], b';' | b' ' | b'\t') {
      continue;
    }
    let Some(want) = want else {
      return true;
    };
    let rest = &line[from..];
    let got = match rest.strip_prefix(b"\"".as_ref()) {
      Some(rest) => &rest[..find_bytes(rest, b"\"").unwrap_or(rest.len())],
      None => &rest[..rest.iter().position(|b| *b == b';').unwrap_or(rest.len())],
    };
    if got == want {
      return true;
    }
  }
  false
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
  if needle.is_empty() || hay.len() < needle.len() {
    return None;
  }
  hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
  use super::*;

  const B: &str = "----WebKitFormBoundary7MA4YWxkTrZu0gW";
  const CT: &str = "multipart/form-data; boundary=----WebKitFormBoundary7MA4YWxkTrZu0gW";

  /// A body of `(field, filename, value)` parts; an empty filename means a plain
  /// field rather than a file part.
  fn body(parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (field, filename, value) in parts {
      let filename = *filename;
      out.extend_from_slice(format!("--{B}\r\n").as_bytes());
      let disp = match filename {
        "" => format!(r#"Content-Disposition: form-data; name="{field}""#),
        f => format!(r#"Content-Disposition: form-data; name="{field}"; filename="{f}""#),
      };
      out.extend_from_slice(disp.as_bytes());
      out.push(b'\r');
      out.push(b'\n');
      if !filename.is_empty() {
        out.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
      }
      out.extend_from_slice(b"\r\n");
      out.extend_from_slice(value);
      out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{B}--\r\n").as_bytes());
    out
  }

  #[test]
  fn reads_a_field_in_any_position() {
    let first = body(&[("model", "", b"whisper-1"), ("language", "", b"en")]);
    let last = body(&[("language", "", b"en"), ("model", "", b"whisper-1")]);
    for case in [first, last] {
      assert_eq!(field_value(&case, CT, "model"), Some(&b"whisper-1"[..]));
      assert_eq!(field_value(&case, CT, "language"), Some(&b"en"[..]));
    }
  }

  #[test]
  fn reads_no_field_that_is_not_there() {
    let case = body(&[("language", "", b"en")]);
    assert_eq!(field_value(&case, CT, "model"), None);
    assert_eq!(field_value(&case, "application/json", "model"), None);
    assert_eq!(field_value(&[], CT, "model"), None);
  }

  #[test]
  fn value_is_the_content_without_the_line_ending() {
    let case = body(&[("model", "", b"whisper-1")]);
    let range = field_range(&case, CT, "model").expect("field");
    assert_eq!(&case[range], b"whisper-1");
  }

  #[test]
  fn a_file_part_does_not_answer_for_a_field() {
    let case = body(&[
      ("file", "audio.wav", b"RIFF....data"),
      ("model", "", b"whisper-1"),
    ]);
    assert_eq!(field_value(&case, CT, "model"), Some(&b"whisper-1"[..]));
    assert_eq!(field_value(&case, CT, "file"), None);
  }

  #[test]
  fn a_binary_file_part_hides_no_boundary() {
    // A copy of the delimiter inside the file bytes is not the next part, and the
    // `model` field after the file still reads back.
    let case = body(&[
      (
        "file",
        "a.bin",
        b"prefix ----WebKitFormBoundary7MA4YWxkTrZu0gW suffix",
      ),
      ("model", "", b"whisper-large"),
    ]);
    assert_eq!(field_value(&case, CT, "model"), Some(&b"whisper-large"[..]));
  }

  #[test]
  fn tolerates_a_bare_line_feed_where_the_spec_wants_crlf() {
    let case = b"--x\nContent-Disposition: form-data; name=\"model\"\n\nwhisper-1\n--x--\n";
    assert_eq!(
      field_value(case, "multipart/form-data; boundary=x", "model"),
      Some(&b"whisper-1"[..])
    );
  }

  #[test]
  fn accepts_the_unquoted_and_quoted_name_parameter() {
    let bare = b"--x\r\nContent-Disposition: form-data; name=model\r\n\r\nwhisper-1\r\n--x--\r\n";
    assert_eq!(
      field_value(bare, "multipart/form-data; boundary=x", "model"),
      Some(&b"whisper-1"[..])
    );
    let filename =
      b"--x\r\nContent-Disposition: form-data; filename=\"model\"\r\n\r\nno\r\n--x--\r\n";
    assert_eq!(
      field_value(filename, "multipart/form-data; boundary=x", "model"),
      None
    );
  }

  #[test]
  fn content_type_is_only_multipart_when_it_says_so() {
    assert!(is_multipart("multipart/form-data; boundary=x"));
    assert!(is_multipart("multipart/form-data"));
    assert!(is_multipart("Multipart/Form-Data; boundary=x"));
    assert!(is_multipart("  multipart/form-data; boundary=x"));
    assert!(!is_multipart("multipart/form-datab; boundary=x"));
    assert!(!is_multipart("application/json"));
  }

  #[test]
  fn boundary_reads_quoted_extra_and_plain() {
    assert_eq!(
      boundary(r#"multipart/form-data; boundary="abc""#),
      Some("abc")
    );
    assert_eq!(boundary("multipart/form-data; boundary=abc"), Some("abc"));
    assert_eq!(
      boundary("multipart/form-data; boundary=abc; charset=utf-8"),
      Some("abc")
    );
    assert_eq!(boundary("multipart/form-data"), None);
    assert_eq!(boundary("application/json"), None);
  }

  #[test]
  fn replace_keeps_every_other_byte() {
    let case = body(&[
      ("file", "audio.wav", b"RIFF....data"),
      ("model", "", b"whisper-1"),
      ("language", "", b"en"),
    ]);
    let out = replace_field(&case, CT, "model", b"my-whisper").expect("replaced");
    assert_eq!(field_value(&out, CT, "model"), Some(&b"my-whisper"[..]));
    assert_eq!(field_value(&out, CT, "language"), Some(&b"en"[..]));
    assert!(
      out
        .windows(b"RIFF....data".len())
        .any(|w| w == b"RIFF....data"),
      "the file part is untouched"
    );
    assert_eq!(
      out.len(),
      case.len() + b"my-whisper".len() - b"whisper-1".len()
    );
    assert_eq!(
      replace_field(&case, "application/json", "model", b"x"),
      None
    );
  }
}
