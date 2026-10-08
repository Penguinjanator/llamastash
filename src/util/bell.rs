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
