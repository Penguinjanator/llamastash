//! Terminal bell: one `\x07` on stderr, nothing else.
//!
//! Callers own the decision to ring (config key, TTY context, what just
//! finished). This only writes, and stays silent when stderr is not a
//! terminal so redirected logs and captured test output collect no control
//! bytes.

use std::io::{IsTerminal, Write};

/// The bell byte. Bare `\a`: no escape sequences, no OS notification.
const BELL: &[u8] = b"\x07";

/// Whether a command should ring: the user kept `bell` on, and the output is
/// not the machine contract. `--json` gets the exit code and no control byte.
pub fn command_rings(bell_enabled: bool, json: bool) -> bool {
  bell_enabled && !json
}

/// Ring the terminal bell.
pub fn ring() {
  let mut err = std::io::stderr();
  if !err.is_terminal() {
    return;
  }
  let _ = ring_to(&mut err);
  let _ = err.flush();
}

/// Write the bell to `sink`. The seam tests assert against: `ring` is a no-op
/// unless stderr is a terminal, which it never is under a test runner.
pub fn ring_to<W: Write + ?Sized>(sink: &mut W) -> std::io::Result<()> {
  sink.write_all(BELL)
}

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

/// Ring when a command the user walked away from ends: `pull`, `start --wait`.
/// `--json` is the machine contract, so it gets the exit code and no control
/// byte. Takes the sink so a test can watch the bytes on either command.
pub fn ring_after_wait(bell_enabled: bool, json: bool, sink: &mut dyn Write) {
  if command_rings(bell_enabled, json) {
    let _ = ring_to(sink);
  }
}

/// Ring into `sink` when the bell is on. The two endings a user notices (a pull
/// finishing, a TUI download finishing or failing) go through this so the gate
/// is one line at each and a test can watch the bytes.
pub fn ring_into(enabled: bool, sink: &mut dyn Write) -> std::io::Result<()> {
  if !enabled {
    return Ok(());
  }
  ring_to(sink)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_bell_is_one_bel_byte() {
    let mut out: Vec<u8> = Vec::new();
    ring_to(&mut out).expect("write");
    assert_eq!(out, b"\x07");
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

  #[test]
  fn only_a_live_run_of_a_bell_keeping_command_rings() {
    assert!(command_rings(true, false));
    assert!(
      !command_rings(false, false),
      "`bell: false` in config.yaml is silence"
    );
    assert!(
      !command_rings(true, true),
      "`--json` is the machine contract: exit code, no control byte"
    );
    assert!(!command_rings(false, true));
  }
}
