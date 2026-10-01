//! Idle-TTL eviction sweeper for proxy-auto-started supervisors.
//!
//! Runs as a background task alongside the proxy listener. Every
//! sweep tick (~30 s by default, clamped against the configured TTL
//! so very short TTLs sweep more often) walks the supervisor
//! snapshot and stops any `Ready` supervisor where:
//!
//! 1. `origin == LaunchOrigin::AutoStart` — manually-started models
//!    are durable user intent; LM Studio's "exempt" rule.
//! 2. `inflight == 0` — refcount gate. A model with active
//!    in-flight requests stays resident even if the last `touch`
//!    timestamp is stale; otherwise long generations (>TTL minutes)
//!    would get SIGTERM'd mid-stream.
//! 3. `now - last_request_at >= ttl` — Ollama-style last-touch
//!    deadline.
//!
//! When all three hold, the sweeper stops the launch through its
//! backend's `stop` (5 s grace), the same path as `stop_model`, so the
//! supervisor and its `state.running` row are dropped with it.
//!
//! TTL = 0 disables eviction entirely; the daemon skips spawning
//! the sweeper task at all in that case (see `daemon::mod.rs`).

use std::sync::Arc;
use std::time::Duration;

use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::supervisor::{LaunchOrigin, ManagedModel, ManagedState};
use crate::proxy::ProxyState;

/// SIGTERM grace given to evicted supervisors. Llama-server is well-
/// behaved on SIGTERM (flushes the HTTP server then exits) so 5 s
/// is plenty; if it ignores SIGTERM the supervisor escalates to
/// SIGKILL itself.
const EVICT_STOP_GRACE: Duration = Duration::from_secs(5);

/// Run the eviction loop until the shutdown token fires. Sleeps for
/// `cadence` between sweeps. Per-sweep work is bounded by the size
/// of the supervisor snapshot; on a typical daemon (<20 active
/// launches) one sweep is microseconds of CPU.
pub async fn run(state: Arc<ProxyState>, ttl: Duration, shutdown: ShutdownToken) {
  if ttl.is_zero() {
    // Defence-in-depth: the spawner should have skipped us entirely
    // for `idle_ttl_secs = 0`, but if it doesn't, exit cleanly so a
    // misconfig doesn't pin a busy loop.
    log::debug!("proxy eviction sweeper: ttl=0, exiting");
    return;
  }
  let cadence = sweep_cadence(ttl);
  log::info!(
    "proxy eviction sweeper armed: ttl={:?}, cadence={:?}",
    ttl,
    cadence,
  );
  loop {
    tokio::select! {
      _ = shutdown.wait_until_triggered() => {
        log::debug!("proxy eviction sweeper: shutdown signalled");
        return;
      }
      _ = tokio::time::sleep(cadence) => {}
    }
    sweep_once(&state, ttl).await;
  }
}

/// Sweep cadence: tick at least every 30 s, but never longer than
/// the TTL itself (a 5 s TTL with a 30 s cadence would let idle
/// supervisors linger up to 35 s). Floor at 5 s so a 1 s TTL doesn't
/// turn the daemon into a stop_model storm.
fn sweep_cadence(ttl: Duration) -> Duration {
  const MIN: Duration = Duration::from_secs(5);
  const MAX: Duration = Duration::from_secs(30);
  ttl.min(MAX).max(MIN)
}

/// Pure per-row decision. Keeps `sweep_once` a thin orchestrator
/// and lets unit tests cover every branch without spinning up real
/// supervisors. `last_request_at = None` means "no MRU stamp yet";
/// the sweeper treats that as `Skip` because `auto_start` is
/// supposed to touch the MRU when the supervisor reaches Ready, so a
/// missing stamp signals either a race or a test fixture where the
/// eviction predicate shouldn't fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepDecision {
  Skip,
  Evict,
}

pub(crate) fn decide(
  origin: LaunchOrigin,
  state: &ManagedState,
  inflight: u64,
  idle_for: Option<Duration>,
  ttl: Duration,
) -> SweepDecision {
  if origin != LaunchOrigin::AutoStart {
    return SweepDecision::Skip;
  }
  if !matches!(state, ManagedState::Ready) {
    return SweepDecision::Skip;
  }
  if inflight > 0 {
    return SweepDecision::Skip;
  }
  match idle_for {
    Some(elapsed) if elapsed >= ttl => SweepDecision::Evict,
    _ => SweepDecision::Skip,
  }
}

/// One sweep pass. Public for integration tests; production use
/// comes via [`run`].
///
/// Each stop is dispatched via `tokio::spawn` so a sweep with N eligible
/// rows doesn't serialise into `N × grace` seconds of cadence drift.
pub async fn sweep_once(state: &Arc<ProxyState>, ttl: Duration) {
  let snap = state.ctx.supervisors.snapshot().await;
  for (launch_id, model) in snap {
    // An infrastructure launch (a managed-multiplexer umbrella) gets
    // lifecycle-aware eviction: never SIGTERM the shared process — free its
    // idle loaded model via the backend's unload API instead (the umbrella
    // stays Ready and autoloads on the next request). This is the `model.stop`
    // vs API-unload branch.
    if let Some(backend) = crate::backend::umbrella_owner(&launch_id) {
      unload_idle_umbrella_model(state, &model, backend, ttl).await;
      continue;
    }
    let current_state = model.state().await;
    let idle_for = state
      .mru
      .last_request_at(model.id())
      .await
      .map(|t| t.elapsed());
    if decide(
      model.origin(),
      &current_state,
      model.inflight(),
      idle_for,
      ttl,
    ) != SweepDecision::Evict
    {
      continue;
    }
    log::info!(
      "proxy eviction: stopping {launch_id} ({served}) — idle {idle:?} >= ttl {ttl:?}",
      launch_id = launch_id.as_str(),
      served = model.params().model_path.display(),
      idle = idle_for,
    );
    let ctx = state.ctx.clone();
    tokio::spawn(async move {
      use crate::backend::Backend;
      // A bare `model.stop` left the row in `state.running`, where it kept
      // holding the launch name and refused the next `<model>@<name>`.
      let backend = crate::daemon::launch_service::backend_for_launch(&ctx, &launch_id).await;
      let _ = backend
        .stop(&ctx, &launch_id, EVICT_STOP_GRACE.as_secs())
        .await;
    });
  }
}

/// Lifecycle-aware eviction for a managed-multiplexer umbrella (R-eviction).
/// Unlike a process-per-model child, the umbrella is shared and long-lived, so
/// when it goes idle we free its resident model(s) via the agnostic
/// [`Backend::stop`] rather than killing the process — for a delegated model
/// `stop` unloads it from the umbrella (which stays Ready for an instant
/// autoload on the next request) instead of a SIGTERM. The umbrella process is
/// never stopped here (it persists regardless of `LaunchOrigin`); only the
/// delegated models it serves are released. The same idle gates as process
/// eviction apply: Ready, no in-flight requests, and idle for >= TTL.
///
/// Idle is umbrella-granular: every delegated request flows through the umbrella
/// and takes its inflight guard + MRU touch (see `proxy::forward`), so a quiet
/// umbrella means every model it serves is quiet. The delegated models are read
/// off the running snapshots on the umbrella's port — `stop` reverse-maps and
/// unloads each, keeping no delegation vocabulary in this sweep.
async fn unload_idle_umbrella_model(
  state: &Arc<ProxyState>,
  umbrella: &ManagedModel,
  backend: crate::backend::Backends,
  ttl: Duration,
) {
  if !matches!(umbrella.state().await, ManagedState::Ready) {
    return;
  }
  // A delegated request takes an inflight guard on the umbrella (see
  // `proxy::forward`), so this skips unloading mid-generation.
  if umbrella.inflight() > 0 {
    return;
  }
  match state.mru.last_request_at(umbrella.id()).await {
    Some(t) if t.elapsed() >= ttl => {}
    _ => return,
  }
  let ctx = state.ctx.clone();
  let umbrella_port = umbrella.port();
  tokio::spawn(async move {
    use crate::backend::Backend;
    // The delegated models this umbrella serves (running snapshots on its port).
    // `stop` unloads each from the umbrella and drops its snapshot — the same end
    // state as a process eviction pruning a supervisor row; the umbrella stays up
    // and the catalog row stays, so the next request autoloads it.
    let targets: Vec<crate::daemon::registry::LaunchId> = ctx
      .state
      .snapshot()
      .await
      .running
      .iter()
      .filter(|r| r.port == umbrella_port && r.delegated_backend_id().is_some())
      .filter_map(|r| r.launch_id.clone())
      .collect();
    for launch_id in targets {
      log::info!(
        "proxy eviction: unloading idle {} (umbrella stays up)",
        launch_id.as_str()
      );
      let _ = backend
        .stop(&ctx, &launch_id, EVICT_STOP_GRACE.as_secs())
        .await;
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ttl() -> Duration {
    Duration::from_secs(60)
  }

  #[test]
  fn decide_skips_manual_origin() {
    let d = decide(
      LaunchOrigin::Manual,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(3600)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_skips_non_ready_states() {
    for s in [
      ManagedState::Launching,
      ManagedState::Loading,
      ManagedState::Stopping,
      ManagedState::Stopped,
      ManagedState::Error { cause: "x".into() },
    ] {
      let d = decide(
        LaunchOrigin::AutoStart,
        &s,
        0,
        Some(Duration::from_secs(3600)),
        ttl(),
      );
      assert_eq!(d, SweepDecision::Skip, "state {s:?} should skip");
    }
  }

  #[test]
  fn decide_skips_when_inflight_gt_zero() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      1,
      Some(Duration::from_secs(3600)),
      ttl(),
    );
    assert_eq!(
      d,
      SweepDecision::Skip,
      "in-flight requests must not be evicted mid-stream"
    );
  }

  #[test]
  fn decide_skips_when_idle_under_ttl() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(30)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_skips_when_no_mru_stamp_yet() {
    // auto_start touches the MRU on Ready, so missing stamp signals a
    // race. Skip rather than evict so a first request doesn't get
    // pre-empted.
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      None,
      ttl(),
    );
    assert_eq!(d, SweepDecision::Skip);
  }

  #[test]
  fn decide_evicts_idle_auto_start_ready_supervisor() {
    let d = decide(
      LaunchOrigin::AutoStart,
      &ManagedState::Ready,
      0,
      Some(Duration::from_secs(61)),
      ttl(),
    );
    assert_eq!(d, SweepDecision::Evict);
  }

  #[test]
  fn sweep_cadence_clamps_against_short_and_long_ttls() {
    assert_eq!(
      sweep_cadence(Duration::from_secs(1)),
      Duration::from_secs(5)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(10)),
      Duration::from_secs(10)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(30)),
      Duration::from_secs(30)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(120)),
      Duration::from_secs(30)
    );
    assert_eq!(
      sweep_cadence(Duration::from_secs(30 * 60)),
      Duration::from_secs(30)
    );
  }
}
