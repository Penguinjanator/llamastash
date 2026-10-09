//! Terminal bell: one `\x07` on stderr, nothing else.
//!
//! Callers own the decision to ring (config key, `--json`, what just
//! finished). This only writes, and stays silent when stderr is not a
//! terminal so redirected logs and captured test output collect no control
//! bytes.

use std::io::{IsTerminal, Write};

/// The bell byte. Bare `\a`: no escape sequences, no OS notification.
const BELL: &[u8] = b"\x07";

/// The bell writer for this run: stderr when it is a terminal, a discard sink
/// otherwise, so piped and captured output never sees a control byte. Call
/// sites take one `&mut dyn Write` either way, and a test takes a buffer.
pub fn tty_sink() -> Box<dyn Write + Send> {
  if std::io::stderr().is_terminal() {
    Box::new(std::io::stderr())
  } else {
    Box::new(std::io::sink())
  }
}

/// Ring into `sink` when the bell is on. One write path for every surface (the
/// CLI endings through [`ring_after_wait`], the download strip, the launch
/// watch), and the seam tests assert against a buffer instead of a terminal.
pub fn ring_into(enabled: bool, sink: &mut dyn Write) -> std::io::Result<()> {
  if !enabled {
    return Ok(());
  }
  sink.write_all(BELL)
}

/// Ring when a command the user walked away from ends: `pull`, `start --wait`.
/// `--json` is the machine contract, so it gets the exit code and no control
/// byte.
pub fn ring_after_wait(bell_enabled: bool, json: bool, sink: &mut dyn Write) {
  let _ = ring_into(bell_enabled && !json, sink);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_bell_is_one_bel_byte() {
    let mut out: Vec<u8> = Vec::new();
    ring_into(true, &mut out).expect("write");
    assert_eq!(out, b"\x07");
    let mut silent: Vec<u8> = Vec::new();
    ring_into(false, &mut silent).expect("write");
    assert!(silent.is_empty(), "the gate is part of the write path");
  }

  #[test]
  fn a_walk_away_command_ends_with_one_bell_unless_the_run_is_quiet() {
    let mut rang: Vec<u8> = Vec::new();
    ring_after_wait(true, false, &mut rang);
    assert_eq!(rang, BELL, "pull and `start --wait` both end on the bell");

    for (bell, json) in [(false, false), (true, true), (false, true)] {
      let mut silent: Vec<u8> = Vec::new();
      ring_after_wait(bell, json, &mut silent);
      assert!(
        silent.is_empty(),
        "bell={bell} json={json} must stay silent"
      );
    }
  }
}
