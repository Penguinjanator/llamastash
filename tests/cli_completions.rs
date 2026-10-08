//! Integration coverage for `llamastash completions`: the accepted
//! shells, a script that actually registers with each shell, and the
//! contract that printing a script has no side effects (no daemon, no
//! state writes, not even a readable config).

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
  Command::new(env!("CARGO_BIN_EXE_llamastash"))
}

fn unique_temp_dir(label: &str) -> PathBuf {
  llamastash::test_support::unique_temp_dir("ls-compl", label)
}

/// Run `completions` against an isolated state dir, optionally with a
/// `--config` path.
fn completions(shell: &str, state_dir: &Path, config: Option<&Path>) -> std::process::Output {
  let mut cmd = bin();
  cmd
    .env("LLAMASTASH_STATE_DIR", state_dir)
    .env("LLAMASTASH_CONFIG_DIR", state_dir.join("config"))
    .arg("completions")
    .arg(shell);
  if let Some(cfg) = config {
    cmd.arg("--config").arg(cfg);
  }
  cmd.output().expect("run")
}

/// The marker each generator uses to hand `llamastash` over to its shell.
const SHELLS: [(&str, &str); 3] = [
  ("bash", "complete -F _llamastash"),
  ("zsh", "#compdef llamastash"),
  ("fish", "complete -c llamastash"),
];

#[test]
fn help_lists_the_accepted_shells() {
  let out = bin().args(["completions", "--help"]).output().expect("run");
  assert!(out.status.success(), "completions --help must exit 0");
  let stdout = String::from_utf8_lossy(&out.stdout);
  for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
    assert!(
      stdout.contains(shell),
      "--help must list `{shell}`: {stdout}"
    );
  }
}

#[test]
fn each_shell_gets_a_script_that_registers_the_command() {
  let dir = unique_temp_dir("scripts");
  std::fs::create_dir_all(&dir).unwrap();
  for (shell, marker) in SHELLS {
    let out = completions(shell, &dir, None);
    assert!(
      out.status.success(),
      "{shell} must exit 0: {}",
      String::from_utf8_lossy(&out.stderr)
    );
    let script = String::from_utf8_lossy(&out.stdout);
    assert!(
      script.contains(marker),
      "{shell} script must contain `{marker}`"
    );
  }
}

#[test]
fn printing_a_script_writes_no_state() {
  let dir = unique_temp_dir("no-write");
  std::fs::create_dir_all(&dir).unwrap();
  let out = completions("bash", &dir, None);
  assert!(out.status.success());
  assert_eq!(
    std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0),
    0,
    "completions must not create anything under the state dir"
  );
}

/// A broken config normally exits `64` before the handler runs. The
/// script generator reads no config, so it stays usable — that is how a
/// user with an unbootable `config.yaml` still gets working completion.
#[test]
fn a_broken_config_does_not_block_the_script() {
  let dir = unique_temp_dir("broken-config");
  std::fs::create_dir_all(&dir).unwrap();
  let cfg = dir.join("broken.yaml");
  std::fs::write(&cfg, "[[[\n").unwrap();
  let out = completions("fish", &dir, Some(&cfg));
  assert!(
    out.status.success(),
    "completions must ignore an unparseable config: {}",
    String::from_utf8_lossy(&out.stderr)
  );
  assert!(String::from_utf8_lossy(&out.stdout).contains("complete -c llamastash"));
}

#[test]
fn an_unknown_shell_is_a_usage_error() {
  let dir = unique_temp_dir("bad-shell");
  let out = completions("tcsh", &dir, None);
  assert_eq!(out.status.code(), Some(64), "unknown shell must exit 64");
}

/// The script is the whole stdout contract, so `--json` is refused
/// rather than wrapping the script in an envelope.
#[test]
fn json_is_not_accepted() {
  let out = bin()
    .args(["completions", "bash", "--json"])
    .output()
    .expect("run");
  assert_eq!(out.status.code(), Some(64), "--json must be refused");
}
