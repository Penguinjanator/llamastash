//! Idle-TTL eviction sweeper + make-room unloading for
//! proxy-auto-started supervisors.
//!
//! Two policies over the same set of "which launches may be given back".
//!
//! **The sweep.** A background task alongside the proxy listener. Every tick
//! (~30 s, clamped against the configured TTL so very short TTLs sweep more
//! often) walks the supervisor snapshot and stops a `Ready` supervisor when all
//! of these hold:
//!
//! - `origin == LaunchOrigin::AutoStart` — manually-started models are durable
//!   user intent, mirroring LM Studio's exemption.
//! - `inflight == 0` — the refcount gate. A model with active in-flight requests
//!   stays resident even if its last `touch` is stale, so a long generation is
//!   never SIGTERM'd mid-stream.
//! - `now - last_request_at >= ttl` — the last-touch deadline, where `ttl` is the
//!   launch's own preset override when it pins one and `proxy.idle_ttl_secs`
//!   otherwise.
//!
//! The stop goes through the backend's own `stop` (5 s grace), the same path
//! `stop_model` uses, so the supervisor and its `state.running` row are dropped
//! with it.
//!
//! **Make-room.** When admission refuses a proxy auto-start, idle auto-started
//! launches are unloaded least-recently-used first until the refused demand fits,
//! instead of answering 503. See [`make_room`].
//!
//! A preset's `idle_ttl_secs` is read off the config store on every pass, so
//! editing `config.yaml` moves a running launch's deadline without a relaunch.
//! `0` there means never unload that launch; with the global TTL also `0` the
//! sweep still runs, because some preset pins a deadline.
//!
//! A global `proxy.idle_ttl_secs = 0` disables the sweep unless some
//! preset pins its own TTL; per-launch `0` always means "never unload".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::daemon::registry::LaunchId;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::supervisor::{LaunchOrigin, ManagedModel, ManagedState};
use crate::proxy::ProxyState;

/// SIGTERM grace given to evicted supervisors. Llama-server is well-
/// behaved on SIGTERM (flushes the HTTP server then exits) so 5 s
/// is plenty; if it ignores SIGTERM the supervisor escalates to
/// SIGKILL itself.
const EVICT_STOP_GRACE: Duration = Duration::from_secs(5);

/// How long make-room waits for a stopped launch's memory to show up as
/// free before re-running admission anyway. The sampler ticks at 1 Hz and
/// a graceful stop takes up to [`EVICT_STOP_GRACE`].
const MAKE_ROOM_FREE_WAIT: Duration = Duration::from_secs(20);

/// Run the eviction loop until the shutdown token fires. Sleeps for
/// `cadence` between sweeps. Per-sweep work is bounded by the size
/// of the supervisor snapshot; on a typical daemon (<20 active
/// launches) one sweep is microseconds of CPU.
///
/// `ttl` is the global `proxy.idle_ttl_secs` default; a launch whose
/// preset pins `idle_ttl_secs` overrides it per launch (see
/// `launch_ttls`). A `0` global means "no global deadline" — the loop
/// still runs so a preset-pinned TTL applies, and a launch with no
/// override is simply never picked.
pub async fn run(state: Arc<ProxyState>, ttl: Duration, shutdown: ShutdownToken) {
  let cadence = sweep_cadence(ttl);
  log::info!(
    "proxy eviction sweeper armed: ttl={}, cadence={:?}",
    if ttl.is_zero() {
      "off (preset TTLs still apply)".to_string()
    } else {
      format!("{ttl:?}")
    },
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
/// turn the daemon into a stop_model storm. A 0 global TTL ("no global
/// deadline, preset TTLs only") takes the slowest cadence.
fn sweep_cadence(ttl: Duration) -> Duration {
  const MIN: Duration = Duration::from_secs(5);
  const MAX: Duration = Duration::from_secs(30);
  if ttl.is_zero() {
    return MAX;
  }
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
/// `default_ttl` is the global `proxy.idle_ttl_secs`; a launch whose preset
/// pins `idle_ttl_secs` uses that instead, and `0` on a launch means "never
/// unload", so that one row is skipped while its neighbours still sweep.
///
/// Each stop is dispatched via `tokio::spawn` so a sweep with N eligible
/// rows doesn't serialise into `N × grace` seconds of cadence drift.
pub async fn sweep_once(state: &Arc<ProxyState>, default_ttl: Duration) {
  let ttls = launch_ttls(state).await;
  let snap = state.ctx.supervisors.snapshot().await;
  for (launch_id, model) in snap {
    let ttl = ttls.get(&launch_id).copied().unwrap_or(default_ttl);
    if ttl.is_zero() {
      continue;
    }
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
    unload_umbrella_models(&ctx, backend, umbrella_port).await;
  });
}

/// Free every model a managed-multiplexer umbrella currently holds, leaving the
/// umbrella process up. Shared by the idle sweep and make-room so the two can't
/// drift on the unload-vs-SIGTERM branch.
pub(crate) async fn unload_umbrella_models(
  ctx: &crate::daemon::context::MethodContext,
  backend: crate::backend::Backends,
  umbrella_port: u16,
) {
  use crate::backend::Backend;
  // The delegated models this umbrella serves (running snapshots on its port).
  // `stop` unloads each from the umbrella and drops its snapshot — the same end
  // state as a process eviction pruning a supervisor row; the umbrella stays up
  // and the catalog row stays, so the next request autoloads it.
  let targets: Vec<LaunchId> = ctx
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
      .stop(ctx, &launch_id, EVICT_STOP_GRACE.as_secs())
      .await;
  }
}

/// The idle-TTL each running launch is held to, keyed by launch id — its
/// preset's `idle_ttl_secs` when that preset pins one.
///
/// Resolved from the preset store on every call rather than stamped at launch,
/// so editing `presets:` in `config.yaml` moves a running launch's deadline
/// without a relaunch. A launch with no preset, or a preset that pins no TTL,
/// is absent from the table and keeps `proxy.idle_ttl_secs`.
pub(crate) async fn launch_ttls(state: &Arc<ProxyState>) -> HashMap<LaunchId, Duration> {
  let mut out = HashMap::new();
  let store = state.ctx.presets.snapshot().await;
  if store.is_empty() {
    return out;
  }
  let snapshot = state.ctx.state.snapshot().await;
  let rows = crate::ipc::methods::catalog_rows(&state.ctx).await;
  // `effective_presets` re-merges the whole store, so do it once per distinct
  // model path rather than once per launch of it.
  let mut by_path: HashMap<String, crate::launch::presets::EffectivePresets> = HashMap::new();
  for row in &snapshot.running {
    let (Some(launch_id), Some(preset)) = (row.launch_id.as_ref(), row.preset.as_deref()) else {
      continue;
    };
    let path = row.params.model_path.display().to_string();
    let eff = by_path.entry(path.clone()).or_insert_with(|| {
      let arch = rows
        .iter()
        .find(|r| r.path == path)
        .and_then(|r| r.arch.as_deref());
      crate::launch::presets::effective_presets(
        &crate::util::paths::model_file_label(&row.params.model_path),
        &path,
        arch,
        &store,
        &rows,
      )
    });
    if let Some(secs) = eff.named(preset).and_then(|p| p.idle_ttl_secs) {
      out.insert(launch_id.clone(), Duration::from_secs(secs));
    }
  }
  out
}

/// One launch make-room may give back, with what it is worth in bytes.
struct RoomCandidate {
  launch_id: LaunchId,
  /// Bytes this launch is credited for freeing — the demand it was admitted at,
  /// the same projection the refused launch is priced with.
  bytes: u64,
  last_request_at: Option<Instant>,
  /// The launch's listening port — an umbrella's resident models are found by
  /// the port they share with it.
  port: u16,
  /// `Some` for a managed-multiplexer umbrella: the shared process is never
  /// stopped, its resident models are freed through the backend's unload API.
  umbrella: Option<crate::backend::Backends>,
}

/// Unload idle launches so a refused auto-start can fit, least-recently-used
/// first. Public for integration tests; production use comes via
/// `crate::proxy::launch`'s auto-start retry.
///
/// Returns `true` when enough was freed that the caller should retry the launch
/// — admission runs again on that retry and stays the authority. All-or-nothing:
/// when every eligible launch together cannot cover the shortfall, nothing is
/// stopped and this returns `false`, because unloading models for a launch that
/// still will not fit is a pure loss.
///
/// Eligible: `Ready`, zero in-flight, `LaunchOrigin::AutoStart` (manual and
/// preloaded launches are durable user intent, the same exemption the sweep
/// applies) and not pinned to `idle_ttl_secs: 0`. The refused launch's own
/// demand and each candidate's credit come from the same admission projection,
/// so the two are in one unit.
pub async fn make_room(
  state: &Arc<ProxyState>,
  refusal: &crate::launch::admission::Refusal,
) -> bool {
  let short = refusal
    .demand_bytes
    .saturating_sub(refusal.available_bytes());
  if short == 0 {
    return false;
  }
  let ttls = launch_ttls(state).await;
  let candidates = room_candidates(state, &ttls).await;
  let mut picked: Vec<RoomCandidate> = Vec::new();
  let mut freed = 0u64;
  for candidate in candidates {
    freed = freed.saturating_add(candidate.bytes);
    picked.push(candidate);
    if freed >= short {
      break;
    }
  }
  if freed < short {
    log::info!(
      "proxy make-room: {} more needed, only {} freeable from {} idle launch(es) — refusing",
      crate::launch::admission::human_gib(short),
      crate::launch::admission::human_gib(freed),
      picked.len(),
    );
    return false;
  }
  log::info!(
    "proxy make-room: unloading {} idle launch(es) ({} freeable) to fit a {} launch",
    picked.len(),
    crate::launch::admission::human_gib(freed),
    crate::launch::admission::human_gib(refusal.demand_bytes),
  );
  let ctx = state.ctx.clone();
  for candidate in picked {
    match candidate.umbrella {
      Some(backend) => {
        if candidate.port != 0 {
          unload_umbrella_models(&ctx, backend, candidate.port).await;
        }
      }
      None => {
        use crate::backend::Backend;
        let launch_id = candidate.launch_id.clone();
        // Same path as `stop_model`: the launch's own backend handles it, so a
        // backend that overrides `stop` is dispatched, not silently defaulted.
        let backend = crate::daemon::launch_service::backend_for_launch(&ctx, &launch_id).await;
        let _ = backend
          .stop(&ctx, &launch_id, EVICT_STOP_GRACE.as_secs())
          .await;
      }
    }
  }
  wait_for_room(state, refusal.demand_bytes).await;
  true
}

/// The unloadable launches, least-recently-used first.
async fn room_candidates(
  state: &Arc<ProxyState>,
  ttls: &HashMap<LaunchId, Duration>,
) -> Vec<RoomCandidate> {
  let supervisors = state.ctx.supervisors.snapshot().await;
  let running = state.ctx.state.snapshot().await;
  let rows = crate::ipc::methods::catalog_rows(&state.ctx).await;
  let mut out = Vec::new();
  for (launch_id, model) in supervisors {
    if !matches!(model.state().await, ManagedState::Ready) || model.inflight() > 0 {
      continue;
    }
    if ttls.get(&launch_id).is_some_and(|t| t.is_zero()) {
      continue;
    }
    let last_request_at = state.mru.last_request_at(model.id()).await;
    // An umbrella holds its models inside the shared process; its credit is
    // what its resident models are worth, and freeing them is an unload call
    // rather than a SIGTERM. Idle is umbrella-granular here exactly as in the
    // sweep, so origin is not consulted: the umbrella row is the infra process
    // itself, always started by the daemon.
    if let Some(backend) = crate::backend::umbrella_owner(&launch_id) {
      let port = model.port();
      let bytes: u64 = running
        .running
        .iter()
        .filter(|r| r.port == port && r.delegated_backend_id().is_some())
        .filter_map(|r| resident_estimate(r, &rows))
        .sum();
      if bytes == 0 {
        continue;
      }
      out.push(RoomCandidate {
        launch_id,
        bytes,
        last_request_at,
        port,
        umbrella: Some(backend),
      });
      continue;
    }
    if model.origin() != LaunchOrigin::AutoStart {
      continue;
    }
    let Some(row) = running
      .running
      .iter()
      .find(|r| r.launch_id.as_ref() == Some(&launch_id))
    else {
      continue;
    };
    match resident_estimate(row, &rows) {
      Some(bytes) if bytes > 0 => out.push(RoomCandidate {
        launch_id,
        bytes,
        last_request_at,
        port: model.port(),
        umbrella: None,
      }),
      _ => log::debug!(
        "proxy make-room: {} has no size to credit — not a candidate",
        launch_id.as_str()
      ),
    }
  }
  // Never-touched rows sort first: an auto-start stamps the MRU on Ready, so a
  // missing stamp means it came up and was never used, which is the least the
  // user can want kept warm.
  out.sort_by_key(|c| {
    std::cmp::Reverse(
      c.last_request_at
        .map(|t| t.elapsed())
        .unwrap_or(Duration::MAX),
    )
  });
  out
}

/// What unloading `row` is worth: the demand the admission gate priced it at,
/// else its catalog weight size, else the file's own size. `None` when nothing
/// sizes it, which keeps the row out of the candidate set rather than crediting
/// it for nothing.
fn resident_estimate(
  row: &crate::daemon::state_store::RunningSnapshot,
  rows: &[crate::launch::resolve::CatalogRow],
) -> Option<u64> {
  if let Some(demand) = row.projected_demand_bytes {
    return Some(demand);
  }
  let path = &row.params.model_path;
  let path_str = path.display().to_string();
  if let Some(weights) = rows
    .iter()
    .find(|r| r.path == path_str)
    .and_then(|r| r.weights_bytes)
    .filter(|w| *w > 0)
  {
    return Some(weights);
  }
  std::fs::metadata(path)
    .ok()
    .filter(|m| m.is_file())
    .map(|m| m.len())
}

/// Poll the sampled free memory until it covers `needed` or the window closes,
/// so the retry is not priced against memory a stopped launch still holds. The
/// sampler ticks at 1 Hz; a graceful stop takes up to the stop grace. On a
/// timeout this returns anyway and admission decides — this is a wait, not a
/// second gate.
async fn wait_for_room(state: &Arc<ProxyState>, needed: u64) {
  let Some(slot) = state.ctx.host_metrics.as_ref() else {
    return;
  };
  let deadline = Instant::now() + MAKE_ROOM_FREE_WAIT;
  while Instant::now() < deadline {
    let free = crate::launch::admission::effective_free_bytes(&slot.read().await.clone());
    if free >= needed {
      return;
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
  }
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
    // A 0 global TTL ("no global deadline, preset TTLs only") takes the slowest
    // cadence rather than busy-sweeping.
    assert_eq!(sweep_cadence(Duration::ZERO), Duration::from_secs(30));
  }

  fn snapshot_row(path: &str, demand: Option<u64>) -> crate::daemon::state_store::RunningSnapshot {
    let mut row = crate::test_support::running_row(path);
    if let Some(bytes) = demand {
      row = row.projected_demand(bytes);
    }
    row.build()
  }

  fn catalog_row(path: &str, weights_bytes: Option<u64>) -> crate::launch::resolve::CatalogRow {
    crate::launch::resolve::CatalogRow {
      path: path.to_string(),
      model_id: None,
      parent: "/m".to_string(),
      source: "user".to_string(),
      arch: Some("llama".to_string()),
      quant: None,
      native_ctx: None,
      mode_hint: None,
      parameter_label: None,
      weights_bytes,
      display_label: None,
      parse_error: None,
      split_siblings: Vec::new(),
      has_chat_template: false,
      has_reasoning_hint: false,
      tokenizer_kind: None,
      total_parameters: None,
      backend: None,
      supported_backends: Vec::new(),
      multimodal: None,
      mtp: None,
    }
  }

  /// Make-room credits a launch with the figure admission priced it at, so the
  /// credit and the refused demand are in one unit — that stamp wins over every
  /// other size available.
  #[test]
  fn resident_estimate_prefers_the_admission_projection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("m.gguf");
    std::fs::write(&model, vec![0u8; 10]).expect("write");
    let path = model.to_str().unwrap();
    let row = snapshot_row(path, Some(4096));
    let rows = [catalog_row(path, Some(99))];
    assert_eq!(resident_estimate(&row, &rows), Some(4096));
  }

  /// A row adopted from an older `state.json`, or a delegated launch the gate
  /// never budgeted, falls back to the catalog weight size, then the file.
  #[test]
  fn resident_estimate_falls_back_to_catalog_weights_then_file_size() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = dir.path().join("m.gguf");
    std::fs::write(&model, vec![0u8; 4096]).expect("write");
    let path = model.to_str().unwrap();
    let row = snapshot_row(path, None);
    let weighted = [catalog_row(path, Some(123))];
    assert_eq!(resident_estimate(&row, &weighted), Some(123));

    let unweighted = [catalog_row(path, None)];
    assert_eq!(resident_estimate(&row, &unweighted), Some(4096));

    // Nothing sizes it: the row stays out of the candidate set instead of being
    // credited for memory nobody knows it holds.
    let ghost = snapshot_row("/no/such/model.gguf", None);
    assert_eq!(resident_estimate(&ghost, &[]), None);
  }
}
