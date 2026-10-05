//! Fixed-width table columns that drop out by rank as a pane narrows.
//! Shared by the Models list and the Requests tab.

/// One data column. `rank` decides which columns survive under width
/// pressure (lower = stickier — see [`fit`]); the order columns are
/// declared in is the left-to-right display order, so visible columns
/// keep their familiar positions as the terminal resizes — only the
/// less-important ones drop out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Column<Id> {
  pub id: Id,
  pub label: &'static str,
  pub width: usize,
  pub rank: u8,
}

/// The columns of `candidates` that fit in `budget` cells, in the order
/// they were given, and the cells they take. Each column costs its width
/// plus `sep_w`. Lower-rank columns win first.
pub(crate) fn fit<'a, Id>(
  candidates: impl IntoIterator<Item = &'a Column<Id>>,
  budget: usize,
  sep_w: usize,
) -> (Vec<&'a Column<Id>>, usize) {
  let mut by_rank: Vec<(usize, &'a Column<Id>)> = candidates.into_iter().enumerate().collect();
  by_rank.sort_by_key(|(_, c)| c.rank);

  let mut taken: Vec<(usize, &'a Column<Id>)> = Vec::with_capacity(by_rank.len());
  let mut spent = 0usize;
  // Strict rank-tail drop: once a lower-rank column refuses to fit,
  // stop trying to admit any higher-rank columns. The alternative
  // (greedy: skip the big one, keep checking smaller ones) gives a
  // tighter information density but produces non-contiguous
  // visibility — a Port column slotting in where Arch can't makes
  // it look like the data jumped a slot as the pane resizes. The
  // cutoff is what users intuit from "rank = min-width threshold".
  for (idx, c) in by_rank {
    let cost = c.width + sep_w;
    if spent + cost > budget {
      break;
    }
    spent += cost;
    taken.push((idx, c));
  }
  // Restore declaration order so columns disappear from less-
  // important slots while the survivors keep their familiar
  // positions on screen.
  taken.sort_by_key(|(idx, _)| *idx);
  (taken.into_iter().map(|(_, c)| c).collect(), spent)
}

/// Left-aligned pad/truncate to `w` display columns. Truncated
/// strings end with `…` so overflow is visible.
pub(crate) fn cell(s: &str, w: usize) -> String {
  if w == 0 {
    return String::new();
  }
  let count = s.chars().count();
  if count <= w {
    let mut out = String::with_capacity(w);
    out.push_str(s);
    for _ in count..w {
      out.push(' ');
    }
    out
  } else {
    let ellipsis = crate::tui::glyphs::active().ellipsis();
    let keep = w.saturating_sub(ellipsis.chars().count());
    let mut out: String = s.chars().take(keep).collect();
    out.push_str(ellipsis);
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const COLS: &[Column<char>] = &[
    Column {
      id: 'a',
      label: "A",
      width: 4,
      rank: 30,
    },
    Column {
      id: 'b',
      label: "B",
      width: 4,
      rank: 10,
    },
    Column {
      id: 'c',
      label: "C",
      width: 2,
      rank: 20,
    },
  ];

  fn ids(budget: usize) -> (Vec<char>, usize) {
    let (cols, spent) = fit(COLS, budget, 1);
    (cols.iter().map(|c| c.id).collect(), spent)
  }

  #[test]
  fn lower_rank_survives_and_display_order_is_kept() {
    assert_eq!(ids(100), (vec!['a', 'b', 'c'], 13));
    assert_eq!(ids(8), (vec!['b', 'c'], 8));
    assert_eq!(ids(5), (vec!['b'], 5));
    assert_eq!(ids(4), (vec![], 0));
  }

  #[test]
  fn a_column_that_does_not_fit_ends_the_pick() {
    // `b` (rank 10) costs 5 and fits. `c` (rank 20) costs 3 and does
    // not fit in the 2 cells left, so the pick stops there.
    assert_eq!(ids(7), (vec!['b'], 5));
    // With 12 cells `b` and `c` fit and `a` (rank 30, cost 5) does not.
    assert_eq!(ids(12), (vec!['b', 'c'], 8));
  }

  #[test]
  fn cell_pads_and_truncates() {
    assert_eq!(cell("ab", 4), "ab  ");
    assert_eq!(cell("abcdef", 4).chars().count(), 4);
    assert_eq!(cell("abc", 0), "");
  }
}
