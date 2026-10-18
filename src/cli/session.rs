//! `llamastash session <tool> [MODEL]` — one coding tool, one model, one run.
//!
//! The tool's per-run settings go in as env vars, CLI flags, or a generated
//! file only that run loads, then the process is replaced by the tool. No
//! tool's global config is read-modified-written here: that is what
//! `integrations` does, and it changes what the tool does in every later run.
//!
//! Model resolution follows the proxy's rules (a `proxy.aliases` name that owns
//! no model, then a model reference, then a live launch), so the same string
//! works on every surface. A model that is not running is started and waited
//! for, which gives the real context window and surfaces a load failure before
//! the tool opens.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::cli::api_key::KEYLESS_STUB;
use crate::cli::cli_args::{Cli, LaunchMode, SessionArgs, SessionTool};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{
  CliExit, CliResult, BINARY_NOT_FOUND, DAEMON_UNREACHABLE, LAUNCH_FAILED, MODEL_NOT_FOUND,
  UNKNOWN, USAGE,
};
use crate::cli::resolve::{
  fetch_catalog, fetch_status, resolve_model_or_launch, CatalogRow, RunningRow,
};
use crate::cli::start::{poll_until_settled, start_row_with_preset};
use crate::config::Config;
use crate::init::external::proxy_url;
use crate::init::external::{all_patchers, PatchContext, PatchModel, SessionLaunch, ToolPatcher};
use crate::ipc::Client;
use crate::launch::resolve::{
  join_named_reference, name_matches, published_ids_for, resolve_exact_reference,
};
use crate::proxy::alias::AliasTable;

/// The states a launch can be handed to a tool from: anything the proxy can
/// route a request for.
const LIVE_STATES: [&str; 3] = ["launching", "loading", "ready"];

pub async fn handle(args: SessionArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let catalog = fetch_catalog(&mut client).await?;
  let status = fetch_status(&mut client).await?;
  // The tool's traffic has to reach a proxy that is actually serving, so this
  // is settled before anything is started or written. `proxy_url` is the one
  // rule for which address that is (a port that moved, a `--proxy-port` launch).
  let proxy_base_url = proxy_url::from_proxy_block(&status.proxy).ok_or_else(|| {
    let state = status
      .proxy
      .get("status")
      .and_then(Value::as_str)
      .unwrap_or("unknown");
    CliExit::new(
      DAEMON_UNREACHABLE,
      format!(
        "the proxy is not listening ({state}); a session tool routes its model traffic through it"
      ),
    )
  })?;
  // A real bearer key is a secret, so `--dry-run` prints it masked. The stub a
  // keyless proxy ignores is safe to show.
  let configured_key = config.proxy.effective_api_key();
  let mask = configured_key.is_some();
  let api_key = configured_key.unwrap_or_else(|| KEYLESS_STUB.to_string());

  let target = resolve_target(&mut client, &catalog, &status.models, &args, config).await?;
  let patcher = patcher_for(args.tool).ok_or_else(|| no_launch(args.tool))?;
  let files_dir = session_files_dir()?;
  let ctx = PatchContext {
    proxy_base_url,
    api_key,
    models: vec![target.patch_model()],
  };
  let launch = patcher
    .session(&ctx, &files_dir)
    .ok_or_else(|| no_launch(args.tool))?;

  if args.dry_run {
    return emit_dry_run(&args, &target, &launch, mask);
  }
  // Everything that can fail is checked before the process is handed over: once
  // the tool is exec'd there is nothing of ours left to report a late failure.
  let program = which::which(launch.program).map_err(|_| {
    CliExit::new(
      BINARY_NOT_FOUND,
      format!("`{}` not found on PATH", launch.program),
    )
  })?;
  write_session_files(&launch)?;
  if !cli.quiet {
    eprintln!("{}", target.note(args.tool.as_str()));
  }
  run(program, launch, &args.tool_args)
}

fn no_launch(tool: SessionTool) -> CliExit {
  CliExit::new(
    USAGE,
    format!("`{}` has no per-run launch", tool.patcher_id()),
  )
}

/// The model the tool is pointed at, and the launch behind it (none for a
/// `--dry-run` of a model that is not running).
struct Target {
  row: CatalogRow,
  launch: Option<RunningRow>,
  /// The id to hand the tool: what `/v1/models` publishes, plus `@<name>` when
  /// the chosen launch is named. Without the suffix the proxy would auto-start a
  /// second, unnamed copy of the same model.
  id: String,
  context_window: Option<u64>,
}

impl Target {
  fn patch_model(&self) -> PatchModel {
    let mut model = PatchModel::from_catalog_row(&self.row, self.id.clone());
    // The launch's own window, not the trained one: a tool sizes its history
    // against this and overflowing the server costs the whole session.
    model.context_window = self.context_window;
    model
  }

  /// The one line printed on stderr before the tool takes the process over.
  fn note(&self, tool: &str) -> String {
    let Some(launch) = &self.launch else {
      return format!("{tool} → {}", self.id);
    };
    let ctx = self
      .context_window
      .map(|c| format!(", ctx {c}"))
      .unwrap_or_default();
    format!("{tool} → {} ({}{ctx})", self.id, launch.launch_id)
  }
}

/// Resolve what to run. Starts a model only when one is missing and `--dry-run`
/// was not given; writes nothing.
async fn resolve_target(
  client: &mut Client,
  catalog: &[CatalogRow],
  running: &[RunningRow],
  args: &SessionArgs,
  config: &Config,
) -> Result<Target, CliExit> {
  let (row, launch) = match args.model.as_deref() {
    None => {
      let found = latest_chat_launch(running).ok_or_else(|| {
        CliExit::new(
          MODEL_NOT_FOUND,
          "no chat model is running; start one or pass a MODEL",
        )
      })?;
      let row = catalog_row(catalog, &found.model_path)?;
      let launch = if args.dry_run {
        found
      } else {
        wait_until_ready(client, &row, found).await?
      };
      (row, Some(launch))
    }
    Some(reference) => {
      let aliases = AliasTable::from_config(&config.proxy.aliases);
      let reference = alias_target(&aliases, catalog, reference);
      let (row, name) = resolve_model_or_launch(catalog, running, &reference)?;
      if let Some(kind) = non_chat_mode(&row) {
        return Err(CliExit::new(
          USAGE,
          format!(
            "`{}` is {kind} model; session tools need a chat model",
            row.name()
          ),
        ));
      }
      let launch = match matching_launch(running, &row.path, name.as_deref()) {
        Some(found) if args.dry_run => Some(found),
        Some(found) => Some(wait_until_ready(client, &row, found).await?),
        None if args.dry_run => None,
        None => {
          // `@<name>` is both the address and the preset to launch with;
          // anything else takes the model's own default preset, as `start` does.
          // The mode is stated because a session can only be chat: a model with
          // no mode hint would otherwise hit `start`'s "pass --mode" error, and
          // this command has no such flag.
          let resp =
            start_row_with_preset(client, &row, name.as_deref(), Some(LaunchMode::Chat)).await?;
          let launch_id = resp.get("launch_id").and_then(Value::as_str);
          match poll_until_settled(client, launch_id, &row.path).await? {
            Some(settled) if settled.state == "ready" => Some(settled),
            Some(settled) => return Err(launch_failed(&settled)),
            None => {
              return Err(CliExit::new(
                LAUNCH_FAILED,
                format!("{} did not become ready in time", row.name()),
              ))
            }
          }
        }
      };
      (row, launch)
    }
  };
  Ok(target(catalog, row, launch))
}

/// Wait for a live launch to be usable. A launch that settles in error or
/// stopped, or never settles, is the documented launch failure.
async fn wait_until_ready(
  client: &mut Client,
  row: &CatalogRow,
  launch: RunningRow,
) -> Result<RunningRow, CliExit> {
  if launch.state == "ready" {
    return Ok(launch);
  }
  match poll_until_settled(client, Some(&launch.launch_id), &row.path).await? {
    Some(settled) if settled.state == "ready" => Ok(settled),
    Some(settled) => Err(launch_failed(&settled)),
    None => Err(CliExit::new(
      LAUNCH_FAILED,
      format!(
        "{} ({}) did not become ready in time",
        row.name(),
        launch.launch_id
      ),
    )),
  }
}

fn launch_failed(launch: &RunningRow) -> CliExit {
  CliExit::new(
    LAUNCH_FAILED,
    format!(
      "launch {} is {}: {}",
      launch.launch_id,
      launch.state,
      launch.state_cause.as_deref().unwrap_or("no cause reported")
    ),
  )
}

fn target(catalog: &[CatalogRow], row: CatalogRow, launch: Option<RunningRow>) -> Target {
  let published = published_ids_for(catalog, std::slice::from_ref(&row))
    .into_iter()
    .next()
    .unwrap_or_else(|| row.public_id());
  let id = match launch.as_ref().and_then(|l| l.name.clone()) {
    Some(name) => join_named_reference(&published, &name),
    None => published,
  };
  let context_window = declared_ctx(launch.as_ref(), &row);
  Target {
    row,
    launch,
    id,
    context_window,
  }
}

fn catalog_row(catalog: &[CatalogRow], path: &str) -> Result<CatalogRow, CliExit> {
  catalog
    .iter()
    .find(|c| c.path == path)
    .cloned()
    .ok_or_else(|| {
      CliExit::new(
        MODEL_NOT_FOUND,
        format!("the running model is not in the catalog: {path}"),
      )
    })
}

/// The most recently started live chat launch. Launch ids are never reused, so
/// the highest `L<n>` is the newest start.
fn latest_chat_launch(running: &[RunningRow]) -> Option<RunningRow> {
  running
    .iter()
    .filter(|r| r.mode == "chat" && LIVE_STATES.contains(&r.state.as_str()))
    .max_by_key(|r| launch_number(r))
    .cloned()
}

/// The live launch a resolved model plus optional name means. A named reference
/// needs that name; a plain one prefers an unnamed launch of that model, else
/// its latest launch.
fn matching_launch(running: &[RunningRow], path: &str, name: Option<&str>) -> Option<RunningRow> {
  let live: Vec<&RunningRow> = running
    .iter()
    .filter(|r| r.model_path == path && LIVE_STATES.contains(&r.state.as_str()))
    .collect();
  if let Some(want) = name {
    return live
      .into_iter()
      .find(|r| name_matches(r.name.as_deref(), want))
      .cloned();
  }
  let unnamed: Vec<&RunningRow> = live.iter().copied().filter(|r| r.name.is_none()).collect();
  let pool = if unnamed.is_empty() { &live } else { &unnamed };
  pool
    .iter()
    .copied()
    .max_by_key(|r| launch_number(r))
    .cloned()
}

fn launch_number(row: &RunningRow) -> u64 {
  row
    .launch_id
    .strip_prefix('L')
    .and_then(|n| n.parse().ok())
    .unwrap_or(0)
}

/// `Some(…)` for a model that cannot carry a chat session, phrased for the
/// error: "is an embedding model".
fn non_chat_mode(row: &CatalogRow) -> Option<&'static str> {
  match row.mode_hint.as_deref() {
    Some("embedding") => Some("an embedding"),
    Some("rerank") => Some("a rerank"),
    _ => None,
  }
}

/// The reference to resolve. A `proxy.aliases` name answers only when it owns no
/// model outright, which is the proxy's own precedence.
fn alias_target<'a>(
  aliases: &'a AliasTable,
  catalog: &[CatalogRow],
  reference: &'a str,
) -> Cow<'a, str> {
  let Some(target) = aliases.target(reference) else {
    return reference.into();
  };
  match resolve_exact_reference(catalog, reference) {
    Ok(_) => reference.into(),
    Err(_) => target.into(),
  }
}

/// The window to hand the tool: what the engine settled on, else what the launch
/// asked for, else the trained window.
fn declared_ctx(launch: Option<&RunningRow>, row: &CatalogRow) -> Option<u64> {
  launch
    .and_then(|l| l.resolved_ctx.map(u64::from))
    .or_else(|| {
      launch
        .and_then(|l| l.params.as_ref())
        .and_then(|p| p.get("ctx"))
        .and_then(Value::as_u64)
    })
    .or(row.native_ctx)
}

fn patcher_for(tool: SessionTool) -> Option<Box<dyn ToolPatcher>> {
  let id = tool.patcher_id();
  all_patchers().into_iter().find(|p| p.id() == id)
}

/// Where a tool's generated per-run files go. One file per distinct content, so
/// concurrent sessions never overwrite each other.
fn session_files_dir() -> Result<PathBuf, CliExit> {
  crate::util::paths::cache_dir()
    .map(|d| d.join("sessions"))
    .ok_or_else(|| CliExit::new(UNKNOWN, "no cache directory available for session files"))
}

fn write_session_files(launch: &SessionLaunch) -> Result<(), CliExit> {
  for file in &launch.files {
    let dir = file.path.parent().unwrap_or(Path::new("."));
    crate::util::atomic_write::write_secure(
      dir,
      "session-",
      &file.path,
      file.content.as_bytes(),
      None,
    )
    .map_err(|e| {
      CliExit::new(
        UNKNOWN,
        format!("could not write {}: {e}", file.path.display()),
      )
    })?;
  }
  Ok(())
}

/// The key is the one value a dry run must not repeat when the proxy enforces
/// one. The keyless stub is not a secret, so it prints as is.
fn shown_value(mask: bool, value: &str) -> String {
  if mask && value != KEYLESS_STUB {
    "***".to_string()
  } else {
    value.to_string()
  }
}

/// `--dry-run` output: one shell-quoted line, or the contract's JSON object.
fn emit_dry_run(
  args: &SessionArgs,
  target: &Target,
  launch: &SessionLaunch,
  mask: bool,
) -> CliResult {
  let body = if args.json {
    let mut env = Map::new();
    for (name, value) in &launch.env {
      env.insert(name.clone(), json!(shown_value(mask, value)));
    }
    crate::cli::output::pretty_json(&json!({
      "tool": args.tool.as_str(),
      "model": target.id,
      "launch_id": target.launch.as_ref().map(|l| l.launch_id.clone()),
      "running": target.launch.is_some(),
      "context_window": target.context_window,
      "program": launch.program,
      "args": launch.args,
      "env": env,
      "env_remove": launch.env_remove,
      "files": launch.files,
      "tool_args": args.tool_args,
    }))
  } else {
    let mut line = String::new();
    for (name, value) in &launch.env {
      line.push_str(&format!(
        "{name}={} ",
        shell_quote(&shown_value(mask, value))
      ));
    }
    line.push_str(launch.program);
    for arg in launch.args.iter().chain(args.tool_args.iter()) {
      line.push(' ');
      line.push_str(&shell_quote(arg));
    }
    line
  };
  println!("{body}");
  Ok(())
}

/// Single-quote a token the way a POSIX shell needs it, so the `--dry-run` line
/// can be pasted back.
fn shell_quote(value: &str) -> String {
  format!("'{}'", value.replace('\'', r"'\''"))
}

/// Hand the process to the tool. On Unix the child replaces us, so signals and
/// the terminal go straight to it and nothing of ours stays resident; elsewhere
/// it is spawned and waited for, and its exit code becomes ours.
fn run(program: PathBuf, launch: SessionLaunch, tool_args: &[String]) -> CliResult {
  let mut cmd = std::process::Command::new(&program);
  cmd.args(&launch.args).args(tool_args);
  for (name, value) in &launch.env {
    cmd.env(name, value);
  }
  for name in &launch.env_remove {
    cmd.env_remove(name);
  }
  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    // Only returns when the exec itself failed.
    let err = cmd.exec();
    Err(CliExit::new(
      BINARY_NOT_FOUND,
      format!("could not start `{}`: {err}", launch.program),
    ))
  }
  #[cfg(not(unix))]
  match cmd.status() {
    Ok(status) => Err(CliExit::code_only(status.code().unwrap_or(UNKNOWN))),
    Err(e) => Err(CliExit::new(
      BINARY_NOT_FOUND,
      format!("could not start `{}`: {e}", launch.program),
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A running row with the fields a session looks at; the rest are what a
  /// `ready` row always carries.
  fn live(launch_id: &str, path: &str, mode: &str, name: Option<&str>) -> RunningRow {
    RunningRow {
      launch_id: launch_id.to_string(),
      model_path: path.to_string(),
      id: None,
      port: 41100,
      mode: mode.to_string(),
      name: name.map(str::to_string),
      preset: None,
      state: "ready".into(),
      state_cause: None,
      pid: Some(1),
      ready_at: None,
      params: None,
      latest_rss_bytes: None,
      latest_cpu_pct: None,
      resolved_ctx: None,
      ctx_clamped: false,
      preset_count: 0,
      preset_default: None,
      backend: None,
      stop_grace_secs: 0,
      request_stats: None,
    }
  }

  fn chat(path: &str) -> CatalogRow {
    let mut row = CatalogRow::for_resolution(path.into(), None, None);
    row.mode_hint = Some("chat".into());
    row
  }

  #[test]
  fn every_session_tool_maps_to_a_patcher_with_a_launch() {
    use clap::ValueEnum;
    let ctx = PatchContext::fixture(&["qwen3-coder-30b"]);
    for tool in SessionTool::value_variants() {
      let patcher =
        patcher_for(*tool).unwrap_or_else(|| panic!("{} is not a patcher", tool.as_str()));
      assert!(
        patcher.session(&ctx, Path::new("/sessions")).is_some(),
        "{} has no per-run launch",
        tool.as_str()
      );
    }
  }

  #[test]
  fn the_latest_start_wins_when_no_model_is_given() {
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", None),
      {
        let mut loading = live("L5", "/m/chat.gguf", "chat", Some("coder"));
        loading.state = "loading".into();
        loading
      },
      live("L7", "/m/embed.gguf", "embedding", None),
    ];
    assert_eq!(
      latest_chat_launch(&running)
        .expect("a chat launch")
        .launch_id,
      "L5",
      "the highest launch number, even while it is still loading"
    );
  }

  #[test]
  fn an_embedding_only_daemon_has_nothing_to_hand_a_tool() {
    let running = vec![
      live("L2", "/m/embed.gguf", "embedding", None),
      live("L3", "/m/rerank.gguf", "rerank", None),
    ];
    assert!(latest_chat_launch(&running).is_none());
  }

  #[test]
  fn a_plain_reference_prefers_the_unnamed_launch() {
    let catalog = vec![chat("/m/chat.gguf")];
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", Some("coder")),
      live("L3", "/m/chat.gguf", "chat", None),
    ];
    let unnamed = matching_launch(&running, "/m/chat.gguf", None).expect("unnamed launch");
    assert_eq!(unnamed.launch_id, "L3");
    assert_eq!(
      target(&catalog, catalog[0].clone(), Some(unnamed)).id,
      "chat",
      "an unnamed launch keeps the plain published id"
    );
  }

  #[test]
  fn only_named_launches_fall_to_the_latest_one_and_carry_the_name() {
    let catalog = vec![chat("/m/chat.gguf")];
    let running = vec![
      live("L2", "/m/chat.gguf", "chat", Some("coder")),
      live("L4", "/m/chat.gguf", "chat", Some("fast")),
    ];
    assert_eq!(
      matching_launch(&running, "/m/chat.gguf", None)
        .expect("latest launch")
        .launch_id,
      "L4"
    );
    let coder = matching_launch(&running, "/m/chat.gguf", Some("coder")).expect("named launch");
    assert_eq!(
      target(&catalog, catalog[0].clone(), Some(coder)).id,
      "chat@coder",
      "without the suffix the proxy would start a second, unnamed copy"
    );
    assert!(
      matching_launch(&running, "/m/chat.gguf", Some("gone")).is_none(),
      "a name with no live launch is not running"
    );
  }

  #[test]
  fn a_model_that_cannot_chat_is_refused_as_a_usage_error() {
    let mut embed = chat("/m/embed.gguf");
    embed.mode_hint = Some("embedding".into());
    assert_eq!(non_chat_mode(&embed), Some("an embedding"));
    let mut rerank = chat("/m/rerank.gguf");
    rerank.mode_hint = Some("rerank".into());
    assert_eq!(non_chat_mode(&rerank), Some("a rerank"));
    assert_eq!(non_chat_mode(&chat("/m/chat.gguf")), None);
    assert_eq!(
      launch_failed(&live("L1", "/m/x.gguf", "chat", None)).code,
      LAUNCH_FAILED
    );
    assert_eq!(no_launch(SessionTool::Claude).code, USAGE);
  }

  #[test]
  fn a_model_named_like_an_alias_wins_over_the_alias() {
    let catalog = vec![chat("/m/qwen.gguf")];
    let aliases = AliasTable::from_config(&crate::config::ProxyAliases::from_pairs(vec![(
      "qwen".to_string(),
      "Other-Model".to_string(),
    )]));
    assert_eq!(
      alias_target(&aliases, &catalog, "qwen"),
      "qwen",
      "a model that owns the name answers first, same rule as the proxy"
    );
    assert_eq!(alias_target(&aliases, &catalog, "gemma"), "gemma");
  }

  #[test]
  fn an_alias_that_names_no_model_answers_with_its_target() {
    let catalog = vec![chat("/m/qwen.gguf")];
    let aliases = AliasTable::from_config(&crate::config::ProxyAliases::from_pairs(vec![(
      "small".to_string(),
      "gemma-3-1b-it-Q4_K_M".to_string(),
    )]));
    assert_eq!(
      alias_target(&aliases, &catalog, "small"),
      "gemma-3-1b-it-Q4_K_M"
    );
  }

  #[test]
  fn the_context_window_prefers_what_the_engine_settled_on() {
    let mut row = chat("/m/chat.gguf");
    row.native_ctx = Some(131_072);
    let mut launch = live("L1", "/m/chat.gguf", "chat", None);
    assert_eq!(
      declared_ctx(Some(&launch), &row),
      Some(131_072),
      "catalog figure"
    );
    launch.params = Some(json!({ "ctx": 32768 }));
    assert_eq!(
      declared_ctx(Some(&launch), &row),
      Some(32768),
      "what the launch asked for"
    );
    launch.resolved_ctx = Some(8192);
    assert_eq!(
      declared_ctx(Some(&launch), &row),
      Some(8192),
      "what the engine served"
    );
    assert_eq!(declared_ctx(None, &row), Some(131_072), "no launch at all");
  }

  #[test]
  fn the_context_window_of_a_running_launch_reaches_the_model() {
    let catalog = vec![{
      let mut row = chat("/m/chat.gguf");
      row.native_ctx = Some(131_072);
      row
    }];
    let mut launch = live("L3", "/m/chat.gguf", "chat", Some("coder"));
    launch.resolved_ctx = Some(32_768);
    let t = target(&catalog, catalog[0].clone(), Some(launch));
    let model = t.patch_model();
    assert_eq!(model.id, "chat@coder");
    assert_eq!(model.context_window, Some(32_768));
    assert_eq!(t.note("claude"), "claude → chat@coder (L3, ctx 32768)");
  }

  #[test]
  fn only_a_real_key_is_masked_in_a_dry_run() {
    assert_eq!(shown_value(true, "s3cr3t"), "***");
    assert_eq!(shown_value(true, KEYLESS_STUB), KEYLESS_STUB);
    assert_eq!(shown_value(false, "s3cr3t"), "s3cr3t");
  }

  #[test]
  fn the_shell_line_quotes_everything_that_needs_it() {
    assert_eq!(shell_quote("plain"), "'plain'");
    assert_eq!(shell_quote("qwen'3"), r"'qwen'\''3'");
    assert_eq!(shell_quote("a b"), "'a b'");
  }
}
