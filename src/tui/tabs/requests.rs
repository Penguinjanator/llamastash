//! Requests tab — the proxy requests of the focused model.
//!
//! A summary strip on top (totals and averages the daemon computed) and
//! below it a table of requests, newest first. Table columns have a fixed
//! width and a rank; as the pane narrows the higher-rank ones drop out,
//! and whatever width is left goes to the trailing `Note` column.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::proxy::request_log::{RequestCells, RequestRow, RequestSummary};
use crate::theme::Palette;
use crate::tui::columns::{self, cell, Column};

/// Rows asked for on each poll. More than any pane shows at once, so a
/// scroll has something to move through.
pub const POLL_ROWS: usize = 200;

/// Cell text for a value the log does not have.
const NONE: &str = "—";
const COL_SEP_W: usize = 1;
/// Cells kept for the `Note` column before any other column is admitted.
const MIN_NOTE_W: usize = 12;
/// Gap between two figures on the summary strip.
const SUMMARY_GAP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnId {
  Time,
  Status,
  Total,
  Speed,
  Ttfb,
  In,
  Out,
  Route,
  Client,
}

/// Table columns in display order. Lower rank stays longer:
///
/// - `Time`, `Status` (10): what happened and when. Never dropped while
///   the pane has room for them at all.
/// - `Total` (20): how long the request took.
/// - `Tok/s` (30): generation speed.
/// - `TTFB` (40): time to the first response byte.
/// - `In` (50), `Out` (55): prompt and generated tokens.
/// - `Route` (60): the same for most of one model's requests.
/// - `Client` (70): loopback for most setups, so the first to go.
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
    rank: 10,
  },
  Column {
    id: ColumnId::Total,
    label: "Total",
    width: 7,
    rank: 20,
  },
  Column {
    id: ColumnId::Speed,
    label: "Tok/s",
    width: 6,
    rank: 30,
  },
  Column {
    id: ColumnId::Ttfb,
    label: "TTFB",
    width: 7,
    rank: 40,
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
    rank: 55,
  },
  Column {
    id: ColumnId::Route,
    label: "Route",
    // Fits `chat/completions`, the longest of the common routes.
    width: 16,
    rank: 60,
  },
  Column {
    id: ColumnId::Client,
    label: "Client",
    width: 21,
    rank: 70,
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
    lines.extend(
      state
        .rows
        .iter()
        .skip(offset)
        .take(room)
        .map(|row| row_line(row, &layout, palette)),
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

/// The columns one render pass shows and the cells left for `Note`.
struct ColumnLayout {
  visible: Vec<&'static Column<ColumnId>>,
  note_w: usize,
}

fn layout_columns(content_w: usize) -> ColumnLayout {
  // `Note` is the last column, so it needs a separator before it only
  // when a fixed column precedes it.
  let budget = content_w.saturating_sub(MIN_NOTE_W + COL_SEP_W);
  let (visible, spent) = columns::fit(COLUMNS, budget, COL_SEP_W);
  ColumnLayout {
    visible,
    note_w: content_w.saturating_sub(spent),
  }
}

fn header_line(layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let mut text = String::new();
  for c in &layout.visible {
    text.push_str(&cell(c.label, c.width));
    text.push(' ');
  }
  text.push_str(&cell("Note", layout.note_w));
  Line::from(Span::styled(
    text,
    palette.muted_style().add_modifier(Modifier::BOLD),
  ))
}

fn row_line(row: &RequestRow, layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let cells = row.cells(NONE);
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(layout.visible.len() + 1);
  for c in &layout.visible {
    let text = format!("{} ", cell(column_value(c.id, &cells), c.width));
    let style = match c.id {
      ColumnId::Status if row.is_error() => palette.error_style(),
      ColumnId::Status if row.status.is_some() => palette.success_style(),
      _ => palette.text_style(),
    };
    spans.push(Span::styled(text, style));
  }
  spans.push(Span::styled(
    cell(&cells.note, layout.note_w),
    palette.muted_style(),
  ));
  Line::from(spans)
}

fn column_value(id: ColumnId, cells: &RequestCells) -> &str {
  match id {
    ColumnId::Time => &cells.time,
    ColumnId::Status => &cells.status,
    ColumnId::Total => &cells.total,
    ColumnId::Speed => &cells.speed,
    ColumnId::Ttfb => &cells.ttfb,
    ColumnId::In => &cells.tokens_in,
    ColumnId::Out => &cells.tokens_out,
    ColumnId::Route => &cells.route,
    ColumnId::Client => &cells.client,
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
    assert_eq!(
      labels(120),
      vec!["Time", "Code", "Total", "Tok/s", "TTFB", "In", "Out", "Route", "Client"]
    );
    // 13 for Note and 68 for everything up to Route: Client is gone.
    assert_eq!(
      labels(100),
      vec!["Time", "Code", "Total", "Tok/s", "TTFB", "In", "Out", "Route"]
    );
    assert_eq!(
      labels(80),
      vec!["Time", "Code", "Total", "Tok/s", "TTFB", "In", "Out"]
    );
    assert_eq!(
      labels(60),
      vec!["Time", "Code", "Total", "Tok/s", "TTFB", "In"]
    );
    assert_eq!(labels(40), vec!["Time", "Code", "Total"]);
    assert_eq!(labels(27), vec!["Time", "Code"]);
    assert_eq!(labels(20), Vec::<&str>::new());
  }

  #[test]
  fn note_takes_the_width_the_columns_leave() {
    for width in [20usize, 40, 60, 80, 120, 200] {
      let layout = layout_columns(width);
      let fixed: usize = layout.visible.iter().map(|c| c.width + COL_SEP_W).sum();
      assert_eq!(fixed + layout.note_w, width, "width {width}");
      assert!(layout.note_w >= MIN_NOTE_W, "width {width}");
    }
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
      .position(|l| l.starts_with("Time     Code Total"))
      .expect("header row");
    assert_eq!(lines[header - 1], "");
    assert!(lines[header].ends_with("Note"), "{lines:?}");
    assert!(lines[header + 1].contains("200  1.2s"), "{lines:?}");
    assert!(lines[header + 1].contains("chat/completions"), "{lines:?}");
    // Two rows, newest (`seq` 2, one second later) first.
    assert_eq!(&lines[header + 1][..8], row(2).cells(NONE).time);
    assert_eq!(&lines[header + 2][..8], row(1).cells(NONE).time);
    assert_eq!(lines[header + 3], "");
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
