//! `llamastash requests [<model-or-launch>] [-n N]` — the proxy's request log.
//!
//! Prints the newest requests the proxy handled, newest first, with a
//! summary above the table on a terminal. `--json` emits the daemon's
//! `requests_tail` body unchanged.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, RequestsArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult};
use crate::cli::output::pretty_json;
use crate::cli::resolve::{fetch_catalog, fetch_status, resolve_model_or_launch};
use crate::cli::{colors, format};
use crate::config::Config;
use crate::proxy::request_log::{RequestRow, Tail};

/// Cell text for a value the log does not have.
const NONE: &str = "-";

pub async fn handle(args: RequestsArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let mut params = json!({});
  if let Some(reference) = &args.model {
    let catalog = fetch_catalog(&mut client).await?;
    let running = fetch_status(&mut client).await?.models;
    let (model, _) = resolve_model_or_launch(&catalog, &running, reference)?;
    params["model_path"] = json!(model.path);
  }
  if let Some(n) = args.lines {
    params["limit"] = json!(n);
  }
  let body = client
    .call("requests_tail", Some(params))
    .await
    .map_err(CliExit::from_client_error)?;
  if args.json {
    println!("{}", pretty_json(&body));
  } else {
    print!("{}", requests_human(&body, args.model.is_some()));
  }
  Ok(())
}

/// The summary and the table. The summary is terminal-only so piped output
/// stays one header line plus one line per request. `one_model` drops the
/// MODEL column, which a model filter makes the same on every line.
fn requests_human(body: &Value, one_model: bool) -> String {
  let Tail { summary, rows } = Tail::deserialize(body).unwrap_or_default();

  let mut out = String::new();
  if console::colors_enabled() {
    let cells = summary.cells(NONE);
    let items: Vec<(&str, String)> = cells.iter().map(|(k, v)| (*k, v.clone())).collect();
    out.push_str(&format::kv_block(&items));
    out.push('\n');
  }
  if rows.is_empty() {
    out.push_str(&colors::dim("(no requests logged)"));
    out.push('\n');
    return out;
  }
  let mut header = vec![
    "TIME", "STATUS", "TOTAL", "TTFB", "TOK/S", "IN", "OUT", "ROUTE",
  ];
  if !one_model {
    header.push("MODEL");
  }
  header.extend(["LAUNCH", "CLIENT", "NOTE"]);
  let table: Vec<Vec<String>> = rows.iter().map(|r| cells_for(r, one_model)).collect();
  out.push_str(&format::table(&header, &table));
  out
}

fn cells_for(row: &RequestRow, one_model: bool) -> Vec<String> {
  let c = row.cells(NONE);
  // Red for an error row when colors are on.
  let status = if row.is_error() && console::colors_enabled() {
    console::style(c.status).red().to_string()
  } else {
    c.status
  };
  let mut cells = vec![
    c.time,
    status,
    c.total,
    c.ttfb,
    c.speed,
    c.tokens_in,
    c.tokens_out,
    c.route,
  ];
  if !one_model {
    cells.push(c.model);
  }
  cells.extend([
    c.launch,
    c.client,
    if c.note.is_empty() {
      NONE.to_string()
    } else {
      c.note
    },
  ]);
  cells
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::proxy::request_log::RequestState;

  fn row() -> RequestRow {
    RequestRow {
      seq: 1,
      started_at_ms: 1_700_000_000_000,
      client: Some("127.0.0.1:5000".to_string()),
      route: "/v1/chat/completions".to_string(),
      requested_model: Some("qwen".to_string()),
      model: Some("Qwen3.8-27B".to_string()),
      model_path: Some("/m/qwen.gguf".to_string()),
      launch_id: Some("L1".to_string()),
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(180),
      duration_ms: Some(1_240),
      prompt_tokens: Some(41),
      completion_tokens: Some(12_345),
      tokens_per_second: Some(39.64),
      ..RequestRow::default()
    }
  }

  /// Piped rendering, restoring the process-wide color flag afterwards.
  fn piped(rows: &[RequestRow], one_model: bool) -> String {
    let body = json!(Tail {
      rows: rows.to_vec(),
      ..Tail::default()
    });
    let prior = console::colors_enabled();
    console::set_colors_enabled(false);
    let out = requests_human(&body, one_model);
    console::set_colors_enabled(prior);
    out
  }

  #[test]
  fn piped_output_is_a_header_and_one_tab_separated_line_per_request() {
    let out = piped(&[row()], false);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
      lines[0],
      "TIME\tSTATUS\tTOTAL\tTTFB\tTOK/S\tIN\tOUT\tROUTE\tMODEL\tLAUNCH\tCLIENT\tNOTE"
    );
    assert_eq!(lines.len(), 2);
    let cells: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(
      &cells[1..],
      [
        "200",
        "1.2s",
        "180ms",
        "39.6",
        "41",
        "12.3k",
        "chat/completions",
        "Qwen3.8-27B",
        "L1",
        "127.0.0.1:5000",
        "-"
      ]
    );
  }

  #[test]
  fn a_model_filter_drops_the_model_column() {
    let out = piped(&[row()], true);
    assert!(!out.lines().next().unwrap().contains("MODEL"));
    assert_eq!(out.lines().nth(1).unwrap().split('\t').count(), 11);
  }

  #[test]
  fn a_failed_request_shows_its_error_and_the_model_asked_for() {
    let failed = RequestRow {
      model: None,
      model_path: None,
      launch_id: None,
      status: Some(404),
      error: Some("model_not_found".to_string()),
      ttfb_ms: None,
      prompt_tokens: None,
      completion_tokens: None,
      tokens_per_second: None,
      ..row()
    };
    let out = piped(&[failed], false);
    let cells: Vec<&str> = out.lines().nth(1).unwrap().split('\t').collect();
    assert_eq!(cells[1], "404");
    assert_eq!(cells[3], "-");
    assert_eq!(cells[8], "qwen");
    assert_eq!(cells[9], "-");
    assert_eq!(cells[11], "model_not_found");
  }

  #[test]
  fn an_empty_log_says_so() {
    assert_eq!(piped(&[], false), "(no requests logged)\n");
  }
}
