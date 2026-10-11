//! Requests tab — the proxy requests of the focused model.
//!
//! A summary strip on top (totals and averages the daemon computed) and
//! below it a table of requests, newest first. Table columns have a width
//! and a rank; as the pane narrows the higher-rank ones drop out. The two
//! free-text columns are the exception: `Note` grows into the spare width
//! only up to `MAX_NOTE_W` and wraps onto continuation lines beyond it,
//! and `Route`, which is last, takes whatever `Note` left.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::proxy::request_log::{RequestCells, RequestRow, RequestSummary};
use crate::theme::Palette;
use crate::tui::columns::{self, cell, Column};
use crate::util::wrap::wrap_cells;

/// Rows asked for on each poll. More than any pane shows at once, so a
/// scroll has something to move through.
pub const POLL_ROWS: usize = 200;

/// Cell text for a value the log does not have.
const NONE: &str = "—";
const COL_SEP_W: usize = 1;
/// The least `Note` is worth showing at.
const MIN_NOTE_W: usize = 12;
/// The widest `Note` grows to. A refused auto-start carries the engine's
/// whole cause, so an uncapped `Note` would push every figure off the pane
/// on the one row where the numbers matter most; past this it wraps.
const MAX_NOTE_W: usize = 48;
/// `Route`'s own width. It is the last column, so it also takes the spare
/// width `Note` did not use.
const ROUTE_W: usize = 16;
/// Gap between two figures on the summary strip.
const SUMMARY_GAP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnId {
  Time,
  Status,
  Speed,
  Total,
  Ttfb,
  In,
  Out,
  Launch,
  Client,
  Note,
  Route,
}

/// Table columns in display order. Keeping them is by `rank`, not by
/// position (see [`columns::fit`]): the figures that answer "what did this
/// request cost" stay longest, `Client` is loopback for most setups and
/// goes first, and `Route` follows it because the tab already implies one
/// engine's routes. `Launch` ranks just under `Out` because with several
/// launches of a model it is what separates a slow one from a healthy one.
const COLUMNS: &[Column<ColumnId>] = &[
  Column {
    id: ColumnId::Time,
    label: "Time",
    width: 8,
    rank: 10,
  },
  Column {
    id: ColumnId::Status,
    label: "Code",
    width: 4,
    rank: 20,
  },
  Column {
    id: ColumnId::Speed,
    label: "Tok/s",
    width: 6,
    rank: 30,
  },
  Column {
    id: ColumnId::Total,
    label: "Total",
    width: 7,
    rank: 40,
  },
  Column {
    id: ColumnId::Ttfb,
    label: "TTFB",
    width: 7,
    rank: 45,
  },
  Column {
    id: ColumnId::In,
    label: "In",
    width: 6,
    rank: 50,
  },
  Column {
    id: ColumnId::Out,
    label: "Out",
    width: 6,
    rank: 60,
  },
  Column {
    id: ColumnId::Launch,
    label: "Launch",
    // Fits a launch id (`L3`) or a short launch name.
    width: 6,
    rank: 65,
  },
  Column {
    id: ColumnId::Client,
    label: "Client",
    width: 21,
    rank: 100,
  },
  Column {
    id: ColumnId::Note,
    label: "Note",
    width: MIN_NOTE_W,
    rank: 70,
  },
  Column {
    id: ColumnId::Route,
    label: "Route",
    // Fits `chat/completions`, the longest of the common routes.
    width: ROUTE_W,
    rank: 80,
  },
];

/// What the tab shows: the last poll's summary and rows for one model.
#[derive(Debug, Clone, Default)]
pub struct RequestsTabState {
  /// Model the data belongs to. `None` until the first poll lands.
  pub model_path: Option<String>,
  pub summary: RequestSummary,
  /// Newest first.
  pub rows: Vec<RequestRow>,
  /// Rows scrolled down from the newest. `0` keeps the newest on top.
  pub scroll_offset: usize,
}

impl RequestsTabState {
  /// Adopt a poll result.
  pub fn set(&mut self, model_path: String, summary: RequestSummary, rows: Vec<RequestRow>) {
    self.model_path = Some(model_path);
    self.summary = summary;
    self.rows = rows;
    self.scroll_offset = self.scroll_offset.min(self.rows.len().saturating_sub(1));
  }

  /// Drop what is shown, scroll included, when focus moves to a model the
  /// data is not for.
  pub fn clear(&mut self) {
    *self = Self::default();
  }

  /// Toward the newest request.
  pub fn scroll_up(&mut self) {
    self.scroll_offset = self.scroll_offset.saturating_sub(1);
  }

  /// Toward older requests.
  pub fn scroll_down(&mut self) {
    self.scroll_offset = self
      .scroll_offset
      .saturating_add(1)
      .min(self.rows.len().saturating_sub(1));
  }
}

/// Render the tab body into `area`. The right pane owns the block.
pub fn render(frame: &mut Frame<'_>, area: Rect, state: &RequestsTabState, palette: &Palette) {
  // No poll has landed for this model yet. Zeros and "no requests" would
  // read as an answer.
  if state.model_path.is_none() {
    let waiting = Line::from(Span::styled("loading", palette.muted_style()));
    frame.render_widget(Paragraph::new(waiting), area);
    return;
  }
  let width = area.width as usize;
  let mut lines = summary_lines(&state.summary, width, palette);
  lines.push(Line::default());
  if state.rows.is_empty() {
    lines.push(Line::from(Span::styled(
      // The Chat / Embed / Rerank tabs talk to the launch's own port, so
      // what they send does not show here.
      "no proxy requests for this model yet",
      palette.muted_style(),
    )));
  } else {
    let layout = layout_columns(width);
    lines.push(header_line(&layout, palette));
    let room = (area.height as usize).saturating_sub(lines.len());
    // Keep a full page on screen when the offset runs past the end.
    let offset = state
      .scroll_offset
      .min(state.rows.len().saturating_sub(room));
    // A wrapped note costs more than one line, so the page is counted in
    // lines, not rows: a long note takes room from the rows below it.
    lines.extend(
      state
        .rows
        .iter()
        .skip(offset)
        .flat_map(|row| row_lines(row, &layout, palette))
        .take(room),
    );
  }
  frame.render_widget(Paragraph::new(lines), area);
}

/// The summary figures as `Label value` cells, wrapped to `width`. A
/// figure is never split across lines.
fn summary_lines(summary: &RequestSummary, width: usize, palette: &Palette) -> Vec<Line<'static>> {
  let mut lines: Vec<Line<'static>> = Vec::new();
  let mut spans: Vec<Span<'static>> = Vec::new();
  let mut used = 0usize;
  for (label, value) in summary.cells(NONE) {
    let cell_w = label.chars().count() + 1 + value.chars().count();
    let gap = if spans.is_empty() { 0 } else { SUMMARY_GAP };
    if !spans.is_empty() && used + gap + cell_w > width {
      lines.push(Line::from(std::mem::take(&mut spans)));
      used = 0;
    }
    if !spans.is_empty() {
      spans.push(Span::raw(" ".repeat(SUMMARY_GAP)));
      used += SUMMARY_GAP;
    }
    let value_style = if label == "Errors" && summary.errors > 0 {
      palette.error_style()
    } else {
      palette.text_style().add_modifier(Modifier::BOLD)
    };
    spans.push(Span::styled(format!("{label} "), palette.muted_style()));
    spans.push(Span::styled(value, value_style));
    used += cell_w;
  }
  if !spans.is_empty() {
    lines.push(Line::from(spans));
  }
  lines
}

/// The columns one render pass shows, plus the widths the two flexible
/// columns got: `Note` up to [`MAX_NOTE_W`] (0 when it does not show) and
/// `Route`, which takes the spare width `Note` left.
struct ColumnLayout {
  visible: Vec<&'static Column<ColumnId>>,
  note_w: usize,
  route_w: usize,
}

impl ColumnLayout {
  fn width_of(&self, column: &Column<ColumnId>) -> usize {
    match column.id {
      ColumnId::Note => self.note_w,
      ColumnId::Route => self.route_w.max(column.width),
      _ => column.width,
    }
  }

  /// Cells of every visible column before `Note`, each padded and followed
  /// by its separator: the indent a wrapped note continues at.
  fn note_indent(&self) -> usize {
    self
      .visible
      .iter()
      .take_while(|c| c.id != ColumnId::Note)
      .map(|c| self.width_of(c) + COL_SEP_W)
      .sum()
  }
}

fn layout_columns(content_w: usize) -> ColumnLayout {
  let (visible, spent) = columns::fit(COLUMNS, content_w, COL_SEP_W);
  // The separator `fit` counted after the last column is the absorber's
  // to use too.
  let mut spare = content_w.saturating_sub(spent) + COL_SEP_W;
  let note_w = if visible.iter().any(|c| c.id == ColumnId::Note) {
    let grow = spare.min(MAX_NOTE_W - MIN_NOTE_W);
    spare -= grow;
    MIN_NOTE_W + grow
  } else {
    0
  };
  // `Route` is admitted last of all but `Client`, so whenever it shows it
  // is the right-most column.
  let route_w = if visible.iter().any(|c| c.id == ColumnId::Route) {
    ROUTE_W + spare
  } else {
    0
  };
  ColumnLayout {
    visible,
    note_w,
    route_w,
  }
}

fn header_line(layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let labels: Vec<String> = layout
    .visible
    .iter()
    .map(|c| cell(c.label, layout.width_of(c)))
    .collect();
  Line::from(Span::styled(
    labels.join(" "),
    palette.muted_style().add_modifier(Modifier::BOLD),
  ))
}

/// One row, as one line per wrapped note chunk. Only `Note` wraps; a
/// continuation line is the indent plus the next chunk, with nothing after
/// it, because `Route` belongs to the row's first line.
fn row_lines(row: &RequestRow, layout: &ColumnLayout, palette: &Palette) -> Vec<Line<'static>> {
  let cells = row.cells(NONE);
  let chunks = wrap_cells(cells.note.as_str(), layout.note_w);
  let mut lines = Vec::with_capacity(chunks.len());
  for (i, chunk) in chunks.into_iter().enumerate() {
    if i > 0 {
      lines.push(Line::from(Span::styled(
        format!(
          "{}{} ",
          " ".repeat(layout.note_indent()),
          cell(&chunk, layout.note_w)
        ),
        palette.muted_style(),
      )));
      continue;
    }
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(layout.visible.len());
    for c in &layout.visible {
      let text = format!(
        "{} ",
        cell(
          if c.id == ColumnId::Note {
            chunk.as_str()
          } else {
            column_value(c.id, &cells)
          },
          layout.width_of(c),
        )
      );
      let style = match c.id {
        ColumnId::Status if row.is_error() => palette.error_style(),
        ColumnId::Status if row.status.is_some() => palette.success_style(),
        ColumnId::Note => palette.muted_style(),
        _ => palette.text_style(),
      };
      spans.push(Span::styled(text, style));
    }
    lines.push(Line::from(spans));
  }
  lines
}

fn column_value(id: ColumnId, cells: &RequestCells) -> &str {
  match id {
    ColumnId::Time => &cells.time,
    ColumnId::Status => &cells.status,
    ColumnId::Speed => &cells.speed,
    ColumnId::Total => &cells.total,
    ColumnId::In => &cells.tokens_in,
    ColumnId::Out => &cells.tokens_out,
    ColumnId::Launch => &cells.launch,
    ColumnId::Ttfb => &cells.ttfb,
    ColumnId::Route => &cells.route,
    ColumnId::Client => &cells.client,
    ColumnId::Note => &cells.note,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::proxy::request_log::RequestState;
  use ratatui::backend::TestBackend;
  use ratatui::Terminal;

  fn row(seq: u64) -> RequestRow {
    RequestRow {
      seq,
      started_at_ms: 1_700_000_000_000 + seq * 1000,
      client: Some("127.0.0.1:5000".to_string()),
      route: "/v1/chat/completions".to_string(),
      state: RequestState::Done,
      status: Some(200),
      ttfb_ms: Some(180),
      duration_ms: Some(1_240),
      prompt_tokens: Some(41),
      completion_tokens: Some(7),
      tokens_per_second: Some(39.64),
      ..RequestRow::default()
    }
  }

  fn palette() -> &'static Palette {
    crate::theme::palette_for(crate::theme::ThemeName::Macchiato)
  }

  fn labels(width: usize) -> Vec<&'static str> {
    layout_columns(width)
      .visible
      .iter()
      .map(|c| c.label)
      .collect()
  }

  fn rendered(state: &RequestsTabState, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let palette = palette();
    terminal
      .draw(|f| render(f, f.area(), state, palette))
      .unwrap();
    let buf = terminal.backend().buffer().clone();
    (0..height)
      .map(|y| {
        (0..width)
          .map(|x| buf[(x, y)].symbol().to_string())
          .collect::<String>()
          .trim_end()
          .to_string()
      })
      .collect()
  }

  #[test]
  fn columns_drop_by_rank_as_the_pane_narrows() {
    // Each column costs its width plus one separator. Running totals in
    // rank order: Time 9, Code 14, Tok/s 21, Total 29, TTFB 37, In 44,
    // Out 51, Launch 58, Note 71, Route 88, Client 110.
    let full = [
      "Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out", "Launch", "Client", "Note", "Route",
    ];
    assert_eq!(labels(110), full);
    // Client ranks lowest of all, so it is the first to go.
    assert_eq!(
      labels(109),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out", "Launch", "Note", "Route"]
    );
    assert_eq!(
      labels(88),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out", "Launch", "Note", "Route"]
    );
    assert_eq!(
      labels(87),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out", "Launch", "Note"]
    );
    assert_eq!(
      labels(70),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out", "Launch"]
    );
    assert_eq!(
      labels(57),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In", "Out"]
    );
    assert_eq!(
      labels(50),
      vec!["Time", "Code", "Tok/s", "Total", "TTFB", "In"]
    );
    assert_eq!(labels(43), vec!["Time", "Code", "Tok/s", "Total", "TTFB"]);
    assert_eq!(labels(36), vec!["Time", "Code", "Tok/s", "Total"]);
    assert_eq!(labels(28), vec!["Time", "Code", "Tok/s"]);
    assert_eq!(labels(21), vec!["Time", "Code", "Tok/s"]);
    assert_eq!(labels(20), vec!["Time", "Code"]);
    assert_eq!(labels(9), vec!["Time"]);
    assert_eq!(labels(8), Vec::<&str>::new());
  }

  #[test]
  fn note_and_route_split_the_spare_width() {
    // The visible columns plus their separators fill the pane, with one
    // cell of slack for the separator after the last column.
    for width in [88usize, 100, 110, 120, 160, 200] {
      let layout = layout_columns(width);
      let spent: usize = layout
        .visible
        .iter()
        .map(|c| layout.width_of(c) + COL_SEP_W)
        .sum();
      assert_eq!(spent, width + 1, "width {width}");
      assert!(layout.note_w <= MAX_NOTE_W, "width {width}");
      assert!(layout.route_w >= ROUTE_W, "width {width}");
    }
    // A pane wide enough for `Note` past its cap leaves the rest to
    // `Route`, which is last.
    let wide = layout_columns(200);
    assert_eq!(wide.note_w, MAX_NOTE_W);
    assert!(wide.route_w > ROUTE_W);
    // At the width `Route` just fits the only spare cell is the separator
    // after the last column, and `Note` grows first.
    let tight = layout_columns(88);
    assert_eq!(tight.note_w, MIN_NOTE_W + 1);
    assert_eq!(tight.route_w, ROUTE_W);
    // Too narrow for `Note`: it gets nothing and the row ends earlier.
    assert_eq!(layout_columns(70).note_w, 0);
  }

  #[test]
  fn summary_wraps_without_splitting_a_figure() {
    let palette = palette();
    let summary = RequestSummary {
      requests: 12,
      errors: 1,
      avg_duration_ms: Some(1_240),
      ..RequestSummary::default()
    };
    let wide = summary_lines(&summary, 200, palette);
    assert_eq!(wide.len(), 1);
    let narrow = summary_lines(&summary, 40, palette);
    assert!(narrow.len() > 1);
    for line in &narrow {
      assert!(line.width() <= 40, "{line:?}");
    }
    let text: String = narrow
      .iter()
      .map(|l| l.to_string())
      .collect::<Vec<_>>()
      .join("\n");
    assert!(text.contains("Requests 12"), "{text}");
    assert!(text.contains("Avg total 1.2s"), "{text}");
    assert!(text.contains("Tok/s avg —"), "{text}");
  }

  #[test]
  fn renders_the_summary_then_the_table_newest_first() {
    let mut state = RequestsTabState::default();
    state.set(
      "/m/a.gguf".to_string(),
      RequestSummary {
        requests: 2,
        ..RequestSummary::default()
      },
      vec![row(2), row(1)],
    );
    let lines = rendered(&state, 120, 8);
    assert!(lines[0].starts_with("Requests 2"), "{lines:?}");
    // The summary wraps to as many lines as it needs; the table starts
    // one blank line under it.
    let header = lines
      .iter()
      .position(|l| l.starts_with("Time     Code Tok/s  Total"))
      .expect("header row");
    assert_eq!(lines[header - 1], "");
    assert!(lines[header].ends_with("Route"), "{lines:?}");
    assert!(lines[header].contains("Launch"), "{lines:?}");
    assert!(lines[header + 1].contains("200  39.6   1.2s"), "{lines:?}");
    assert!(lines[header + 1].contains("chat/completions"), "{lines:?}");
    // Two rows, newest (`seq` 2, one second later) first.
    assert_eq!(&lines[header + 1][..8], row(2).cells(NONE).time);
    assert_eq!(&lines[header + 2][..8], row(1).cells(NONE).time);
    assert_eq!(lines[header + 3], "");
  }

  #[test]
  fn a_long_note_wraps_under_its_own_column() {
    let long = "launch_failed: auto-start of Qwen3.8-27B failed: could not allocate the KV cache";
    let mut state = RequestsTabState::default();
    state.set(
      "/m/a.gguf".to_string(),
      RequestSummary::default(),
      vec![RequestRow {
        seq: 1,
        started_at_ms: 1_700_000_000_000,
        route: "/v1/chat/completions".to_string(),
        state: RequestState::Done,
        status: Some(503),
        error: Some("launch_failed".to_string()),
        cause: Some(
          "auto-start of Qwen3.8-27B failed: could not allocate the KV cache".to_string(),
        ),
        ..RequestRow::default()
      }],
    );
    let lines = rendered(&state, 120, 10);
    let layout = layout_columns(120);
    let wrapped = wrap_cells(long, layout.note_w);
    assert!(wrapped.len() > 1, "the note must wrap, not truncate");
    // A continuation line is the indent plus the next chunk and nothing
    // after it: `Route` belongs to the row's first line.
    let header = lines
      .iter()
      .position(|l| l.starts_with("Time     Code Tok/s  Total"))
      .expect("header row");
    let indent = layout.note_indent();
    for (i, chunk) in wrapped.iter().enumerate().skip(1) {
      // Chunk 0 is on the row's own line, one line below the header.
      let line = lines[header + 1 + i].trim_end();
      let before = line.chars().take_while(|c| *c == ' ').count();
      assert_eq!(before, indent, "{line:?}");
      assert_eq!(
        line.chars().count(),
        indent + chunk.chars().count(),
        "{line:?}"
      );
    }
  }

  #[test]
  fn an_empty_log_says_so_under_the_summary() {
    let mut state = RequestsTabState::default();
    state.set(
      "/m/a.gguf".to_string(),
      RequestSummary::default(),
      Vec::new(),
    );
    let lines = rendered(&state, 80, 6);
    assert!(lines[0].starts_with("Requests 0"), "{lines:?}");
    assert!(
      lines
        .iter()
        .any(|l| l == "no proxy requests for this model yet"),
      "{lines:?}"
    );
  }

  #[test]
  fn before_the_first_poll_lands_the_tab_says_loading() {
    let lines = rendered(&RequestsTabState::default(), 80, 6);
    assert_eq!(lines[0], "loading");
    assert!(lines[1..].iter().all(String::is_empty), "{lines:?}");
  }

  #[test]
  fn scroll_moves_toward_older_rows_and_stops_at_the_last_page() {
    let mut state = RequestsTabState::default();
    let rows: Vec<RequestRow> = (1..=10).rev().map(row).collect();
    state.set("/m/a.gguf".to_string(), RequestSummary::default(), rows);
    state.scroll_up();
    assert_eq!(state.scroll_offset, 0);
    for _ in 0..50 {
      state.scroll_down();
    }
    assert_eq!(state.scroll_offset, 9);
    // 3 lines of summary, blank and header leave 3 for rows: the last
    // page is the 3 oldest, not a single row with blank space under it.
    let lines = rendered(&state, 200, 6);
    assert_eq!(&lines[3][..8], row(3).cells(NONE).time);
    assert_eq!(&lines[5][..8], row(1).cells(NONE).time);
  }

  #[test]
  fn set_clamps_the_scroll_and_clear_resets_it() {
    let mut state = RequestsTabState::default();
    state.set(
      "/m/a.gguf".to_string(),
      RequestSummary::default(),
      (1..=10).rev().map(row).collect(),
    );
    state.scroll_offset = 8;
    state.set(
      "/m/a.gguf".to_string(),
      RequestSummary::default(),
      (1..=4).rev().map(row).collect(),
    );
    assert_eq!(state.scroll_offset, 3);
    state.clear();
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.model_path, None);
  }
}
