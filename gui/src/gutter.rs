//! The source pane's own gutter column: hit counts, breakpoints and the execution arrow, drawn by
//! hopper beside quire's gutter (quire has no host gutter API yet: `docs/requests.md`).
//!
//! The column has two zones: the hit count on its left (a press lists every execution of the
//! line) and the marker zone on its right, where breakpoints and the arrow are drawn (a press
//! adds or removes a breakpoint).
//!
//! This module is the mapping only: from the lines the editor shows (and where) plus what hopper
//! knows about them to the marks the column draws, and which lines still need a count.

use std::collections::HashMap;

/// How often a line ran in the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitCount {
    /// Asked for, not answered yet.
    Pending,
    /// The index answered: this many executions (`partial`: the index has not covered the whole
    /// recording yet, so there may be more).
    Known { count: u64, partial: bool },
    /// The engine could not say (no line table entry, an error).
    Unavailable,
}

/// The hit counts of one file's lines (1-based, as `SourceLoc` numbers them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HitCounts {
    lines: HashMap<u32, HitCount>,
}

impl HitCounts {
    pub fn get(&self, line: u32) -> Option<HitCount> {
        self.lines.get(&line).copied()
    }

    pub fn set(&mut self, line: u32, count: HitCount) {
        self.lines.insert(line, count);
    }

    /// The lines of `visible` nobody has asked about yet; marks them pending.
    pub fn take_unasked(&mut self, visible: impl IntoIterator<Item = u32>) -> Vec<u32> {
        let mut out = Vec::new();
        for line in visible {
            if let std::collections::hash_map::Entry::Vacant(e) = self.lines.entry(line) {
                e.insert(HitCount::Pending);
                out.push(line);
            }
        }
        out
    }

    /// Drops the counts that were partial or pending, so they are asked again (the index grew).
    pub fn forget_incomplete(&mut self) {
        self.lines.retain(|_, c| {
            matches!(
                c,
                HitCount::Known { partial: false, .. } | HitCount::Unavailable
            )
        });
    }
}

/// Which arrow a line gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrow {
    /// The cursor: where the stopped thread is.
    Execution,
    /// The selected outer frame's call site.
    Frame,
}

/// A line's execution arrow, 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrowAt {
    pub line: u32,
    pub arrow: Arrow,
}

/// One row of the column.
#[derive(Clone, Debug, PartialEq)]
pub struct GutterMark {
    pub line: u32,
    /// Top of the row in the pane's local space.
    pub y: f32,
    /// The hit count as drawn ("12", "1.2k", "3+" while partial); `None` draws nothing.
    pub label: Option<String>,
    pub arrow: Option<Arrow>,
    /// A user breakpoint is set on the line.
    pub breakpoint: bool,
}

/// Width of the marker zone at the column's right edge.
pub const MARKER_W: f32 = 26.0;

/// Which zone of the column a press at `x` (from the column's left edge) is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterZone {
    /// The hit count: list the line's executions.
    HitCount,
    /// The marker zone: toggle a breakpoint.
    Marker,
}

pub fn zone_at(x: f32, column_w: f32) -> GutterZone {
    if x >= column_w - MARKER_W {
        GutterZone::Marker
    } else {
        GutterZone::HitCount
    }
}

/// The marks for the rows the editor shows: `rows` are (1-based line, top y in local space) of
/// each visible line; `breakpoints` the lines with a user breakpoint.
pub fn marks(
    rows: &[(u32, f32)],
    hits: &HitCounts,
    arrows: &[ArrowAt],
    breakpoints: &[u32],
) -> Vec<GutterMark> {
    rows.iter()
        .filter_map(|&(line, y)| {
            let label = match hits.get(line) {
                Some(HitCount::Known {
                    count: 0,
                    partial: false,
                }) => None,
                Some(HitCount::Known { count, partial }) => Some(format!(
                    "{}{}",
                    compact(count),
                    if partial { "+" } else { "" }
                )),
                Some(HitCount::Pending | HitCount::Unavailable) | None => None,
            };
            // the execution arrow wins over a frame arrow on the same line
            let arrow = arrows
                .iter()
                .filter(|a| a.line == line)
                .map(|a| a.arrow)
                .min_by_key(|a| match a {
                    Arrow::Execution => 0,
                    Arrow::Frame => 1,
                });
            let breakpoint = breakpoints.contains(&line);
            (label.is_some() || arrow.is_some() || breakpoint).then_some(GutterMark {
                line,
                y,
                label,
                arrow,
                breakpoint,
            })
        })
        .collect()
}

/// A count in at most four characters: 999, 1.2k, 12k, 3.4M.
pub fn compact(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "k")];
    for (scale, unit) in UNITS {
        if n >= scale {
            let whole = n / scale;
            if whole >= 10 {
                return format!("{whole}{unit}");
            }
            let tenth = (n % scale) * 10 / scale;
            return format!("{whole}.{tenth}{unit}");
        }
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_arrows_map_to_their_rows() {
        let mut hits = HitCounts::default();
        hits.set(
            10,
            HitCount::Known {
                count: 1,
                partial: false,
            },
        );
        hits.set(
            11,
            HitCount::Known {
                count: 0,
                partial: false,
            },
        );
        hits.set(
            12,
            HitCount::Known {
                count: 1500,
                partial: true,
            },
        );
        hits.set(13, HitCount::Pending);
        let rows = [(10, 0.0), (11, 18.0), (12, 36.0), (13, 54.0), (14, 72.0)];
        let arrows = [
            ArrowAt {
                line: 14,
                arrow: Arrow::Frame,
            },
            ArrowAt {
                line: 11,
                arrow: Arrow::Execution,
            },
        ];
        let m = marks(&rows, &hits, &arrows, &[13]);
        assert_eq!(
            m,
            vec![
                GutterMark {
                    line: 10,
                    y: 0.0,
                    label: Some("1".into()),
                    arrow: None,
                    breakpoint: false
                },
                GutterMark {
                    line: 11,
                    y: 18.0,
                    label: None,
                    arrow: Some(Arrow::Execution),
                    breakpoint: false
                },
                GutterMark {
                    line: 12,
                    y: 36.0,
                    label: Some("1.5k+".into()),
                    arrow: None,
                    breakpoint: false
                },
                GutterMark {
                    line: 13,
                    y: 54.0,
                    label: None,
                    arrow: None,
                    breakpoint: true
                },
                GutterMark {
                    line: 14,
                    y: 72.0,
                    label: None,
                    arrow: Some(Arrow::Frame),
                    breakpoint: false
                },
            ]
        );
    }

    #[test]
    fn the_execution_arrow_wins_a_shared_line() {
        let hits = HitCounts::default();
        let arrows = [
            ArrowAt {
                line: 5,
                arrow: Arrow::Frame,
            },
            ArrowAt {
                line: 5,
                arrow: Arrow::Execution,
            },
        ];
        assert_eq!(
            marks(&[(5, 3.0)], &hits, &arrows, &[])[0].arrow,
            Some(Arrow::Execution)
        );
    }

    #[test]
    fn presses_on_the_right_toggle_breakpoints() {
        assert_eq!(zone_at(4.0, 58.0), GutterZone::HitCount);
        assert_eq!(zone_at(31.9, 58.0), GutterZone::HitCount);
        assert_eq!(zone_at(32.0, 58.0), GutterZone::Marker);
        assert_eq!(zone_at(57.0, 58.0), GutterZone::Marker);
    }

    #[test]
    fn lines_are_asked_once_and_partial_counts_again() {
        let mut hits = HitCounts::default();
        assert_eq!(hits.take_unasked(1..=3), vec![1, 2, 3]);
        assert_eq!(hits.take_unasked(2..=4), vec![4]);
        hits.set(
            1,
            HitCount::Known {
                count: 4,
                partial: true,
            },
        );
        hits.set(
            2,
            HitCount::Known {
                count: 4,
                partial: false,
            },
        );
        hits.forget_incomplete();
        assert_eq!(hits.take_unasked(1..=4), vec![1, 3, 4]);
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(7), "7");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_000), "1.0k");
        assert_eq!(compact(1_250), "1.2k");
        assert_eq!(compact(12_400), "12k");
        assert_eq!(compact(3_400_000), "3.4M");
    }
}
