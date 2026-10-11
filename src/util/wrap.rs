//! Splitting text to fit a fixed number of display cells.

/// Break `text` into pieces of at most `w` cells, on a space where one
/// falls inside the limit and mid-word otherwise. Returns the text whole
/// when it already fits, and unchanged for `w` of 0, so a caller whose
/// column is hidden pays nothing.
///
/// Used by the two request tables (the TUI `Requests` tab and
/// `llamastash requests`), which both cap the note column instead of
/// letting one long cause push the figures out of view.
pub fn wrap_cells(text: &str, w: usize) -> Vec<String> {
  if w == 0 || text.chars().count() <= w {
    return vec![text.to_string()];
  }
  let mut out: Vec<String> = Vec::new();
  let mut line = String::new();
  let mut used = 0usize;
  for word in text.split_whitespace() {
    let len = word.chars().count();
    if len > w {
      // A single token longer than the column (a path, an engine cause).
      if !line.is_empty() {
        out.push(std::mem::take(&mut line));
      }
      let mut rest: String = word.chars().skip(w).collect();
      out.push(word.chars().take(w).collect());
      while rest.chars().count() > w {
        let head: String = rest.chars().take(w).collect();
        out.push(head);
        rest = rest.chars().skip(w).collect();
      }
      line.push_str(&rest);
      used = rest.chars().count();
      continue;
    }
    let gap = if line.is_empty() { 0 } else { 1 };
    if used + gap + len > w {
      out.push(std::mem::take(&mut line));
      used = 0;
    }
    if used > 0 {
      line.push(' ');
      used += 1;
    }
    line.push_str(word);
    used += len;
  }
  if !line.is_empty() || out.is_empty() {
    out.push(line);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn breaks_on_spaces_and_hard_breaks_a_long_token() {
    assert_eq!(wrap_cells("short", 10), vec!["short"]);
    assert_eq!(wrap_cells("a b c", 0), vec!["a b c"]);
    assert_eq!(wrap_cells("", 10), vec![""]);
    assert_eq!(wrap_cells("one two three", 9), vec!["one two", "three"]);
    assert_eq!(wrap_cells("abcdefghij k", 4), vec!["abcd", "efgh", "ij k"]);
    let long = "launch_failed: auto-start of a very long model name that will not fit";
    for chunk in wrap_cells(long, 12) {
      assert!(chunk.chars().count() <= 12, "{chunk:?}");
    }
    // Nothing is dropped: at a width no token needs splitting, the chunks
    // are the words in order.
    assert_eq!(wrap_cells(long, 20).join(" "), long);
  }
}
