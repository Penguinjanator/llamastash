//! Terminal bell: one `\x07` on stderr, nothing else.
//!
//! Callers own the decision to ring (config key, TTY context, what just
//! finished). This only writes, and stays silent when stderr is not a
//! terminal so redirected logs and captured test output collect no control
//! bytes.

use std::io::{IsTerminal, Write};

/// Ring the terminal bell.
pub fn ring() {
  let mut err = std::io::stderr();
  if !err.is_terminal() {
    return;
  }
  let _ = err.write_all(b"\x07");
  let _ = err.flush();
}
