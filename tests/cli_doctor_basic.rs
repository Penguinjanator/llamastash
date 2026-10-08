//! Read-only smoke for `llamastash doctor` (Unit 3 stub).
//! Run binary as a subprocess and assert clap accepts the surface,
//! plus the stub emits a parseable JSON envelope under `--json`.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-doc", label)
}

/// A `doctor` run pointed at throwaway state + config dirs, so it can
/// never touch a real daemon's files or the user's own config.
fn isolated(dir: &std::path::Path) -> Command {
  let mut cmd = bin();
  cmd
    .env("LLAMASTASH_STATE_DIR", dir.join("state"))
    .env("LLAMASTASH_CONFIG_DIR", dir.join("config"))
    .env("LLAMASTASH_OFFLINE", "1");
  cmd
}

/// A wrong-mode config plus a `daemon.pid` with no flock holder — the two
/// findings `doctor --fix` can repair.
#[cfg(unix)]
fn dirty_setup(dir: &std::path::Path) -> (PathBuf, PathBuf) {
  use std::os::unix::fs::PermissionsExt;
  let config_dir = dir.join("config");
  let state_dir = dir.join("state");
  std::fs::create_dir_all(&config_dir).unwrap();
  std::fs::create_dir_all(&state_dir).unwrap();
  let config = config_dir.join("config.yaml");
  std::fs::write(&config, "proxy:\n  port: 11435\n").unwrap();
  std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
  std::fs::write(state_dir.join("daemon.pid"), b"4242\n").unwrap();
  std::fs::write(state_dir.join("runtime.json"), b"{}\n").unwrap();
  // Everything past the hardware section needs the `init` baseline, so
  // leave one behind: without it `config_mode_drift` never runs.
  llamastash::init::snapshot::save(
    &state_dir,
    &llamastash::init::snapshot::InitSnapshot::default(),
  )
  .unwrap();
  (config, state_dir)
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
  use std::os::unix::fs::PermissionsExt;
  std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn doctor_help_is_accepted() {
  let out = bin().args(["doctor", "--help"]).output().expect("run");
  assert!(out.status.success(), "doctor --help should exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  assert!(stdout.contains("--json"), "--help must mention --json");
  for flag in ["--fix", "--dry-run"] {
    assert!(stdout.contains(flag), "--help must mention {flag}");
  }
}

#[test]
fn doctor_json_emits_findings_envelope() {
  let out = bin().args(["doctor", "--json"]).output().expect("run");
  assert!(out.status.success(), "stub must exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  let parsed: serde_json::Value =
    serde_json::from_str(&stdout).expect("--json output must parse as JSON");
  assert!(
    parsed.get("findings").is_some(),
    "envelope must carry findings"
  );
  assert!(parsed["findings"].is_array(), "findings must be an array");
}

#[test]
fn doctor_plain_run_succeeds_zero_findings_today() {
  let out = bin().arg("doctor").output().expect("run");
  assert!(
    out.status.success(),
    "doctor must exit 0 when no findings are present"
  );
}

#[cfg(unix)]
#[test]
fn dry_run_reports_the_repairs_and_changes_nothing() {
  let dir = unique_temp_dir("dry-run");
  let (config, state) = dirty_setup(&dir);
  let out = isolated(&dir)
    .args(["doctor", "--fix", "--dry-run", "--json"])
    .output()
    .unwrap();
  assert!(
    out.status.success(),
    "--fix must not change the exit-0 contract"
  );
  let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
  let fixes = report["fixes"].as_array().expect("fixes array");
  assert_eq!(
    fixes.len(),
    3,
    "config + runtime.json + daemon.pid: {fixes:#?}"
  );
  assert!(
    fixes.iter().all(|f| f["outcome"] == "would_apply"),
    "{fixes:#?}"
  );
  assert_eq!(mode_of(&config), 0o644, "--dry-run must not chmod");
  assert!(
    state.join("daemon.pid").exists(),
    "--dry-run must not delete"
  );
  assert!(
    report["findings"]
      .as_array()
      .unwrap()
      .iter()
      .any(|f| f["id"] == "stale_daemon_files"),
    "the stale pair must be reported: {report:#?}"
  );
}

#[cfg(unix)]
#[test]
fn fix_repairs_a_wrong_mode_config_and_a_stale_pidfile() {
  let dir = unique_temp_dir("fix");
  let (config, state) = dirty_setup(&dir);
  let out = isolated(&dir)
    .args(["doctor", "--fix", "--json"])
    .output()
    .unwrap();
  assert!(out.status.success(), "doctor --fix exits 0");
  let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
  let fixes = report["fixes"].as_array().expect("fixes array");
  assert_eq!(fixes.len(), 3, "{fixes:#?}");
  assert!(
    fixes.iter().all(|f| f["outcome"] == "applied"),
    "{fixes:#?}"
  );
  assert_eq!(mode_of(&config), 0o600);
  assert!(!state.join("daemon.pid").exists());
  assert!(!state.join("runtime.json").exists());

  // A second read-only run has nothing left to report, and no repairs to do.
  let again = isolated(&dir).args(["doctor", "--json"]).output().unwrap();
  let clean: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
  let ids: Vec<&str> = clean["findings"]
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|f| f["id"].as_str())
    .collect();
  assert!(!ids.contains(&"config_mode_drift"), "{ids:?}");
  assert!(!ids.contains(&"stale_daemon_files"), "{ids:?}");
  assert_eq!(
    clean["fixes"]
      .as_array()
      .expect("fixes stays an array")
      .len(),
    0,
    "a read-only run applies nothing"
  );
}
