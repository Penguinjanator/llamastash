//! Requests tab — the proxy requests of the focused model.
//!
//! A summary strip on top (totals and averages the daemon computed) and
//! below it a table of requests, newest first. Table columns have a width
//! and a rank; as the pane narrows the higher-rank ones drop out. `Note`
//! is ranked like the rest and, when it shows, takes the width left over.

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
/// The least `Note` is worth showing at. It grows into whatever the other
/// columns leave.
const MIN_NOTE_W: usize = 12;
/// Gap between two figures on the summary strip.
const SUMMARY_GAP: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnId {
  Time,
  Status,
  Speed,
  Total,
  In,
  Out,
  Ttfb,
  Route,
  Client,
  Note,
}

/// Table columns in display order, which is also the order they are
/// worth keeping in: as the pane narrows they go from the right, `Note`
/// first and `Time` last. `Client` is the exception. It is loopback for
/// most setups, so it ranks below `Note` and is the first to go, but it
/// sits before `Note` on screen so that `Note` stays the last column and
/// can take the leftover width.
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
    id: ColumnId::Ttfb,
    label: "TTFB",
    width: 7,
    rank: 70,
  },
  Column {
    id: ColumnId::Route,
    label: "Route",
    // Fits `chat/completions`, the longest of the common routes.
    width: 16,
    rank: 80,
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
    rank: 90,
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

/// The columns one render pass shows, and the width `Note` gets: its
/// minimum plus what the others leave, or `0` when it does not show.
struct ColumnLayout {
  visible: Vec<&'static Column<ColumnId>>,
  note_w: usize,
}

impl ColumnLayout {
  fn width_of(&self, column: &Column<ColumnId>) -> usize {
    match column.id {
      ColumnId::Note => self.note_w,
      _ => column.width,
    }
  }
}

fn layout_columns(content_w: usize) -> ColumnLayout {
  let (visible, spent) = columns::fit(COLUMNS, content_w, COL_SEP_W);
  let note_w = if visible.iter().any(|c| c.id == ColumnId::Note) {
    // It is the last column, so the separator counted after it is its
    // to use too.
    MIN_NOTE_W + COL_SEP_W + content_w.saturating_sub(spent)
  } else {
    0
  };
  ColumnLayout { visible, note_w }
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

fn row_line(row: &RequestRow, layout: &ColumnLayout, palette: &Palette) -> Line<'static> {
  let cells = row.cells(NONE);
  let mut spans: Vec<Span<'static>> = Vec::with_capacity(layout.visible.len());
  for c in &layout.visible {
    let text = format!("{} ", cell(column_value(c.id, &cells), layout.width_of(c)));
    let style = match c.id {
      ColumnId::Status if row.is_error() => palette.error_style(),
      ColumnId::Status if row.status.is_some() => palette.success_style(),
      ColumnId::Note => palette.muted_style(),
      _ => palette.text_style(),
    };
    spans.push(Span::styled(text, style));
  }
  Line::from(spans)
}

fn column_value(id: ColumnId, cells: &RequestCells) -> &str {
  match id {
    ColumnId::Time => &cells.time,
    ColumnId::Status => &cells.status,
    ColumnId::Speed => &cells.speed,
    ColumnId::Total => &cells.total,
    ColumnId::In => &cells.tokens_in,
    ColumnId::Out => &cells.tokens_out,
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
    // rank order: Time 9, Code 14, Tok/s 21, Total 29, In 36, Out 43,
    // TTFB 51, Route 68, Note 81, Client 103.
    assert_eq!(
      labels(103),
      vec!["Time", "Code", "Tok/s", "Total", "In", "Out", "TTFB", "Route", "Client", "Note"]
    );
    // Client ranks below Note, so it is the first to go.
    assert_eq!(
      labels(102),
      vec!["Time", "Code", "Tok/s", "Total", "In", "Out", "TTFB", "Route", "Note"]
    );
    assert_eq!(
      labels(80),
      vec!["Time", "Code", "Tok/s", "Total", "In", "Out", "TTFB", "Route"]
    );
    assert_eq!(
      labels(67),
      vec!["Time", "Code", "Tok/s", "Total", "In", "Out", "TTFB"]
    );
    assert_eq!(
      labels(50),
      vec!["Time", "Code", "Tok/s", "Total", "In", "Out"]
    );
    assert_eq!(labels(40), vec!["Time", "Code", "Tok/s", "Total", "In"]);
    assert_eq!(labels(30), vec!["Time", "Code", "Tok/s", "Total"]);
    assert_eq!(labels(21), vec!["Time", "Code", "Tok/s"]);
    assert_eq!(labels(14), vec!["Time", "Code"]);
    assert_eq!(labels(9), vec!["Time"]);
    assert_eq!(labels(8), Vec::<&str>::new());
  }

  #[test]
  fn note_takes_the_width_the_other_columns_leave() {
    for width in [81usize, 90, 102, 103, 120, 200] {
      let layout = layout_columns(width);
      let others: usize = layout
        .visible
        .iter()
        .filter(|c| c.id != ColumnId::Note)
        .map(|c| c.width + COL_SEP_W)
        .sum();
      assert_eq!(others + layout.note_w, width, "width {width}");
      assert!(layout.note_w > MIN_NOTE_W, "width {width}");
    }
    // Too narrow for `Note`: it gets nothing and the row ends earlier.
    assert_eq!(layout_columns(80).note_w, 0);
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
    assert!(lines[header].ends_with("Note"), "{lines:?}");
    assert!(lines[header + 1].contains("200  39.6   1.2s"), "{lines:?}");
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
