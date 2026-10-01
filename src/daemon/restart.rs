//! Client-side stop / restart primitives, shared by the `daemon stop` /
//! `daemon restart` CLI commands and the TUI's `Ctrl+R` writer task.
//!
//! Both surfaces need the same sequence: ask the running daemon to shut down,
//! wait for the process to actually release its lockfile, and only then bring a
//! replacement up. The `shutdown` RPC only *requests* teardown, so a caller
//! that returns as soon as it is answered races the dying daemon's `flock` (and
//! any umbrella it is still stopping) and walks straight into "already running".

use std::{
  path::Path,
  time::{Duration, Instant},
};

use anyhow::{Context, Result};

use crate::daemon::{existing_daemon_pid, runtime_file};
use crate::ipc::{Client, ClientError};

/// Floor on the exit wait, so a daemon with no managed children still gets its
/// full cleanup (connection drain, `stop_all_managed`, lockfile release) before
/// the caller gives up on it.
const MIN_EXIT_WAIT: Duration = Duration::from_secs(10);

/// Lockfile poll interval while waiting for the old daemon to exit.
const POLL: Duration = Duration::from_millis(50);

/// What asking the daemon to stop actually achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
  /// The daemon exited.
  Stopped,
  /// Nothing answered over IPC. The caller decides what that means — genuinely
  /// down, or a stale process with no handshake that needs a signal by PID.
  NoChannel,
  /// Teardown was requested but the process was still alive when the wait
  /// window closed. `daemon stop` treats this as success; a restart refuses to
  /// spawn on top of it.
  StillExiting { pid: i32 },
}

/// Call `shutdown` on the daemon at `state_dir` and wait for the process to
/// exit. The window is the longest child stop grace the daemon reported (so a
/// slow-to-die engine is not mistaken for a hung daemon) plus 5 s for the
/// daemon's own teardown, floored at 10 s.
///
/// A shutdown RPC failure on a *live* daemon is an error — no wait fixes it. A
/// handshake that points at a process which is not there is not: it reports
/// [`StopOutcome::NoChannel`] and clears the handshake, so the caller's next
/// start is not aimed at a dead URL.
pub async fn shutdown_and_wait(state_dir: &Path) -> Result<StopOutcome> {
  let mut client = match Client::connect(state_dir).await {
    Ok(client) => client,
    Err(ClientError::Connect(_)) => return Ok(StopOutcome::NoChannel),
    Err(_other) if existing_daemon_pid(state_dir).is_none() => return Ok(StopOutcome::NoChannel),
    Err(other) => return Err(other).context("daemon shutdown request"),
  };
  let resp = match client.call("shutdown", None).await {
    Ok(resp) => resp,
    // `runtime.json` outlives its daemon on a crash, and `Client::connect` only
    // reads the file — it never probes the URL. So a call that cannot land on a
    // state dir nobody holds the lock on means there is nothing to stop, not a
    // stop that failed. Clearing the handshake here is what keeps `daemon
    // restart` working after a crash instead of failing until the file is
    // deleted by hand.
    Err(_stale) if existing_daemon_pid(state_dir).is_none() => {
      runtime_file::remove(state_dir);
      return Ok(StopOutcome::NoChannel);
    }
    Err(other) => return Err(other).context("daemon shutdown request"),
  };
  // Close the pooled keep-alive before waiting for the exit. The control plane
  // drains by polling its active-connection count down to zero, so a client
  // still holding a connection open here makes the daemon sit out the whole
  // drain window before it gets to `stop_all_managed` and the lockfile.
  drop(client);
  let grace = resp
    .get("stop_grace_secs")
    .and_then(|v| v.as_u64())
    .unwrap_or(0);
  let window = MIN_EXIT_WAIT.max(Duration::from_secs(grace.saturating_add(5)));
  let deadline = Instant::now() + window;
  loop {
    match existing_daemon_pid(state_dir) {
      None => return Ok(StopOutcome::Stopped),
      Some(pid) if Instant::now() >= deadline => return Ok(StopOutcome::StillExiting { pid }),
      Some(_) => tokio::time::sleep(POLL).await,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    crate::test_support::unique_temp_dir("ls-rst", label)
  }

  /// No handshake and no lockfile holder means there is nothing to wait for.
  #[tokio::test]
  async fn shutdown_and_wait_reports_no_channel_when_nothing_is_running() {
    let dir = unique_temp_dir("empty");
    let outcome = shutdown_and_wait(&dir)
      .await
      .expect("no-daemon stop is not an error");
    assert_eq!(outcome, StopOutcome::NoChannel);
    std::fs::remove_dir_all(&dir).ok();
  }

  /// A handshake left by a crashed daemon points at a control plane that is
  /// gone. With nobody holding the lock that is "nothing to stop", not a failed
  /// stop, and the dead handshake gets cleared so the caller's next start is
  /// not aimed at it.
  #[tokio::test]
  async fn shutdown_and_wait_clears_a_handshake_with_no_lock_holder() {
    use crate::daemon::runtime_file;

    let dir = unique_temp_dir("stale");
    let stale: runtime_file::RuntimeInfo = serde_json::from_str(
      r#"{"ipc_url":"http://127.0.0.1:1","ipc_token":"dead-token","started_at_unix":1,"daemon_pid":1}"#,
    )
    .expect("handshake fixture");
    runtime_file::save(&dir, &stale).expect("save handshake");

    let outcome = shutdown_and_wait(&dir)
      .await
      .expect("a stale handshake is not an error");
    assert_eq!(outcome, StopOutcome::NoChannel);
    assert!(
      !runtime_file::path(&dir).exists(),
      "the dead handshake should be gone"
    );
    std::fs::remove_dir_all(&dir).ok();
  }
}
