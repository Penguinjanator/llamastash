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
use crate::cli::resolve::{
  fetch_catalog, fetch_status, resolve_model, resolve_model_or_launch, resolve_running, CatalogRow,
  RunningRow,
};
use crate::cli::{colors, format};
use crate::config::Config;
use crate::proxy::request_log::{RequestRow, Tail};
use crate::util::wrap::wrap_cells;

/// Cell text for a value the log does not have.
const NONE: &str = "-";

pub async fn handle(args: RequestsArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;
  let mut params = json!({});
  if let Some(reference) = &args.model {
    let catalog = fetch_catalog(&mut client).await?;
    let running = fetch_status(&mut client).await?.models;
    params = tail_scope(&catalog, &running, reference)?;
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

/// The `requests_tail` params a reference asks for: a launch reference scopes
/// to that one launch, a model reference to the model and every launch of it.
/// The catalog answers first, so a model whose own name looks like a launch
/// still reads as a model, and `model@name` with no such launch live keeps the
/// model scope rather than failing.
fn tail_scope(
  catalog: &[CatalogRow],
  running: &[RunningRow],
  reference: &str,
) -> Result<Value, CliExit> {
  if let Ok(model) = resolve_model(catalog, reference) {
    return Ok(json!({ "model_path": model.path }));
  }
  if let Ok(launch) = resolve_running(running, reference) {
    return Ok(json!({ "launch_id": launch.launch_id }));
  }
  let (model, _) = resolve_model_or_launch(catalog, running, reference)?;
  Ok(json!({ "model_path": model.path }))
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
  let mut header = vec!["TIME", "CODE", "TOK/S", "TOTAL", "TTFB", "IN", "OUT"];
  if !one_model {
    header.push("MODEL");
  }
  header.extend(["LAUNCH", "CLIENT", "NOTE", "ROUTE"]);
  let table: Vec<Vec<String>> = rows.iter().map(|r| cells_for(r, one_model)).collect();
  let table = wrap_notes(&header, table);
  out.push_str(&format::table(&header, &table));
  out
}

/// The widest the NOTE column gets on a terminal. One refused auto-start
/// carries the engine's whole cause, and a note wider than the screen
/// buries the figures of every row under it.
const NOTE_MAX_W: usize = 60;

/// Split long notes over continuation lines under the NOTE column, on a
/// terminal only. Piped output keeps one line per request with the whole
/// note in its field, so `awk -F\t` pipelines are untouched.
fn wrap_notes(header: &[&str], rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
  if !console::colors_enabled() {
    return rows;
  }
  let Some(note) = header.iter().position(|h| *h == "NOTE") else {
    return rows;
  };
  let mut out = Vec::with_capacity(rows.len());
  for row in rows {
    let note_text = row.get(note).cloned().unwrap_or_default();
    for (i, chunk) in wrap_cells(&note_text, NOTE_MAX_W).into_iter().enumerate() {
      let mut line = row.clone();
      line[note] = chunk;
      if i > 0 {
        // A continuation line carries only the note: the row's own
        // figures already appeared above it.
        for (j, cell) in line.iter_mut().enumerate() {
          if j != note {
            *cell = String::new();
          }
        }
      }
      out.push(line);
    }
  }
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
    c.speed,
    c.total,
    c.ttfb,
    c.tokens_in,
    c.tokens_out,
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
    c.route,
  ]);
  cells
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::resolve::running_rows_in;
  use crate::proxy::request_log::{RequestState, RequestSummary};

  #[test]
  fn a_reference_that_names_a_launch_scopes_to_that_launch() {
    let running = running_rows_in(&json!({
      "models": [{
        "launch_id": "L3",
        "id": {"path": "/m/qwen.gguf", "header_blake3": "deadbeef"},
        "port": 41100,
        "mode": "chat",
        "state": "ready",
        "name": "coder",
      }],
    }));
    // An empty catalog leaves only the launch half to resolve against.
    for reference in ["L3", "41100", "coder"] {
      assert_eq!(
        tail_scope(&[], &running, reference).unwrap(),
        json!({ "launch_id": "L3" }),
        "`{reference}` names a launch"
      );
    }
    let miss = tail_scope(&[], &running, "nope").unwrap_err();
    assert!(miss.message.unwrap_or_default().contains("nope"));
  }

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
  /// The lock keeps the other CLI test modules, which flip the same flag,
  /// out while it is changed.
  fn piped(rows: &[RequestRow], one_model: bool) -> String {
    let body = json!(Tail {
      rows: rows.to_vec(),
      ..Tail::default()
    });
    let _lock = crate::cli::test_lock::serialize();
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
      "TIME\tCODE\tTOK/S\tTOTAL\tTTFB\tIN\tOUT\tMODEL\tLAUNCH\tCLIENT\tNOTE\tROUTE"
    );
    assert_eq!(lines.len(), 2);
    let cells: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(
      &cells[1..],
      [
        "200",
        "39.6",
        "1.2s",
        "180ms",
        "41",
        "12.3k",
        "Qwen3.8-27B",
        "L1",
        "127.0.0.1:5000",
        "-",
        "chat/completions",
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
    assert_eq!(cells[2], "-");
    assert_eq!(cells[4], "-");
    assert_eq!(cells[7], "qwen");
    assert_eq!(cells[8], "-");
    assert_eq!(cells[10], "model_not_found");
    assert_eq!(cells[11], "chat/completions");
  }

  /// Terminal rendering, with the process-wide color flag restored. It
  /// gates the padded layout and the note wrap, so the same rows print
  /// differently here than under [`piped`].
  fn on_terminal(rows: &[RequestRow], one_model: bool) -> String {
    let body = json!(Tail {
      rows: rows.to_vec(),
      ..Tail::default()
    });
    let _lock = crate::cli::test_lock::serialize();
    let prior = console::colors_enabled();
    console::set_colors_enabled(true);
    let out = requests_human(&body, one_model);
    console::set_colors_enabled(prior);
    out
  }

  /// A row whose note runs past the NOTE column's cap.
  fn long_note_row() -> RequestRow {
    RequestRow {
      status: Some(503),
      error: Some("launch_failed".to_string()),
      cause: Some(
        "auto-start of the model failed because the KV cache did not fit; \
         unload another model or lower --ctx-size"
          .to_string(),
      ),
      ..row()
    }
  }

  #[test]
  fn a_long_note_wraps_onto_a_continuation_line_on_a_terminal() {
    let lines: Vec<String> = on_terminal(&[long_note_row()], false)
      .lines()
      .map(|l| console::strip_ansi_codes(l).to_string())
      // Drop the summary block: it is terminal-only and has its own wrap.
      .skip_while(|l| !l.starts_with("TIME"))
      .collect();
    assert_eq!(lines.len(), 4, "header, rule, row, continuation: {lines:?}");
    let (row_line, cont) = (&lines[2], &lines[3]);
    assert!(row_line.contains("launch_failed:"), "{row_line}");
    assert!(
      !row_line.contains("--ctx-size"),
      "the note must wrap: {row_line}"
    );
    assert!(cont.contains("--ctx-size"), "{cont}");
    // The continuation line carries the note and nothing else.
    assert!(!cont.contains("chat/completions"), "{cont}");
    assert!(!cont.contains("1.2s"), "{cont}");
  }

  #[test]
  fn a_piped_long_note_stays_one_field_on_one_line() {
    let out = piped(&[long_note_row()], false);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let cells: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(cells.len(), 12);
    assert!(cells[10].ends_with("lower --ctx-size"), "{}", cells[10]);
  }

  #[test]
  fn an_empty_log_says_so() {
    assert_eq!(piped(&[], false), "(no requests logged)\n");
  }

  /// Terminal rendering of a whole tail, summary block included.
  fn summary_on_terminal(summary: RequestSummary) -> String {
    let body = json!(Tail {
      summary,
      rows: vec![row()],
    });
    let _lock = crate::cli::test_lock::serialize();
    let prior = console::colors_enabled();
    console::set_colors_enabled(true);
    let out = requests_human(&body, false);
    console::set_colors_enabled(prior);
    console::strip_ansi_codes(&out).to_string()
  }

  #[test]
  fn the_summary_block_prints_the_clients_of_the_scope_and_a_live_pair() {
    let live = RequestSummary {
      requests: 3,
      hosts: Some(2),
      connections: Some(3),
      live_hosts: Some(1),
      live_connections: Some(2),
      tokens_per_second_last: Some(41.3),
      ..RequestSummary::default()
    };
    let strip = summary_on_terminal(live.clone());
    assert!(strip.contains("Live  1/2"), "{strip}");
    assert!(strip.contains("Hosts  2"), "{strip}");
    assert!(strip.contains("Connections  3"), "{strip}");
    // The newest speed is the row table's business, not the summary's.
    assert!(!strip.contains("Tok/s last"), "{strip}");

    // One host, one peer: one number, not a pair.
    let one_number = summary_on_terminal(RequestSummary {
      live_hosts: Some(2),
      live_connections: Some(2),
      ..live.clone()
    });
    assert!(one_number.contains("Live  2"), "{one_number}");

    // Nothing in flight: no Live line at all.
    let idle = summary_on_terminal(RequestSummary {
      live_hosts: Some(0),
      live_connections: Some(0),
      ..live
    });
    assert!(!idle.contains("Live"), "{idle}");

    // An older daemon that never computed them: placeholders, not zeros.
    let predates = summary_on_terminal(RequestSummary {
      hosts: None,
      connections: None,
      live_hosts: None,
      live_connections: None,
      ..RequestSummary::default()
    });
    assert!(predates.contains("Hosts  -"), "{predates}");
    assert!(predates.contains("Connections  -"), "{predates}");
  }
}
