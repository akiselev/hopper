//! The profile table: skelter's virtual `table` reading a Rust `RowModel` over a [`LazyTree`] of
//! [`ProfileNode`]s, paged in through `Debugger::profile`.
//!
//! Like the call tree (`calls.rs`), column 0 draws the indentation and the triangle as text, and
//! activating a row toggles it. The other columns are costs as a share of the profiled root.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use hopper_core::api::{ProfileNode, ProfileNodeId, Threshold};
use skelter_base::rows::{Cell, Column, RowModel};

use crate::tree::{Expansion, LazyTree, Row, TreeItem};

impl TreeItem for ProfileNode {
    fn child_count(&self) -> Option<u64> {
        Some(self.child_count)
    }
}

pub type ProfileTree = LazyTree<ProfileNodeId, ProfileNode>;

/// The threshold presets the pane offers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThresholdPreset {
    TenthOfAPercent,
    HalfAPercent,
    OnePercent,
    FivePercent,
}

impl ThresholdPreset {
    pub const ALL: [ThresholdPreset; 4] = [
        ThresholdPreset::TenthOfAPercent,
        ThresholdPreset::HalfAPercent,
        ThresholdPreset::OnePercent,
        ThresholdPreset::FivePercent,
    ];

    /// The fraction of the root's cost a node must reach to be shown.
    pub fn of_root(self) -> f32 {
        match self {
            ThresholdPreset::TenthOfAPercent => 0.001,
            ThresholdPreset::HalfAPercent => 0.005,
            ThresholdPreset::OnePercent => 0.01,
            ThresholdPreset::FivePercent => 0.05,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThresholdPreset::TenthOfAPercent => "0.1%",
            ThresholdPreset::HalfAPercent => "0.5%",
            ThresholdPreset::OnePercent => "1%",
            ThresholdPreset::FivePercent => "5%",
        }
    }

    /// The preset whose fraction `t` shows, if it is one of them.
    pub fn of(t: &Threshold) -> Option<ThresholdPreset> {
        ThresholdPreset::ALL
            .into_iter()
            .find(|p| p.of_root() == t.of_root)
    }
}

/// What a control in the pane's header asks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProfileControl {
    Threshold(ThresholdPreset),
    /// Toggle between all code and the user's own code.
    MyCode,
    /// Toggle pinning the hops of the origin chain last shown in the query pane.
    PinOrigin,
    /// Profile the call selected in the Calls pane.
    ProfileSelectedCall,
    /// Profile every root call.
    AllRoots,
}

/// The tree with what the cells need besides the rows.
#[derive(Debug)]
pub struct ProfileView {
    pub tree: ProfileTree,
    /// The cost the percentages are of: the sum of the top-level nodes' inclusive costs (the
    /// root call's own cost when the request has a root).
    pub root_cost: u64,
    /// The threshold hidden children are described against.
    pub threshold: Threshold,
}

impl ProfileView {
    pub fn new(page_size: u32) -> Self {
        ProfileView {
            tree: ProfileTree::new(page_size),
            root_cost: 0,
            threshold: Threshold::default(),
        }
    }
}

/// `part` as a percentage of `whole`, one decimal; "-" when there is no whole.
pub fn percent_of(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".into();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// A fraction as a percentage without trailing zeros: `0.001` is "0.1%", `0.05` is "5%".
pub fn fraction_label(f: f32) -> String {
    let s = format!("{:.2}", f * 100.0);
    format!("{}%", s.trim_end_matches('0').trim_end_matches('.'))
}

/// Column 0's text for a row: indentation, triangle, name, and a mark for a recursive or pinned
/// node.
pub fn label(tree: &ProfileTree, row: &Row<ProfileNodeId>) -> String {
    let pad = "    ".repeat(row.depth() as usize);
    match row {
        Row::Node { key, .. } => {
            let tri = match tree.expansion(key) {
                Some(Expansion::Collapsed) => "▸ ",
                Some(Expansion::Expanded) => "▾ ",
                _ => "   ",
            };
            let Some(n) = tree.item(key) else {
                return format!("{pad}{tri}?");
            };
            let recursion = if n.recursion > 0 {
                format!("  ↻{}", n.recursion)
            } else {
                String::new()
            };
            let pinned = if n.pinned { "  ◆" } else { "" };
            format!("{pad}{tri}{}{recursion}{pinned}", n.function)
        }
        Row::Loading { .. } => format!("{pad}   loading…"),
        Row::More { loaded, total, .. } => match total {
            Some(t) => format!(
                "{pad}   … {} more (activate to load)",
                t.saturating_sub(*loaded)
            ),
            None => format!("{pad}   … more (activate to load)"),
        },
    }
}

/// The hidden column: how many children the threshold hid, and their share.
pub fn hidden_text(n: &ProfileNode, threshold: &Threshold, root_cost: u64) -> String {
    if n.hidden.nodes == 0 {
        return String::new();
    }
    format!(
        "+{} below {} ({})",
        n.hidden.nodes,
        fraction_label(threshold.of_root),
        percent_of(n.hidden.cost, root_cost)
    )
}

/// The table's model: the shared view, read when the table draws.
#[derive(Debug)]
pub struct ProfileRows {
    pub view: Rc<RefCell<ProfileView>>,
    columns: Vec<Column>,
}

pub const COLUMNS: [(&str, &str, f32); 6] = [
    ("function", "Function", 340.0),
    ("share", "% of root", 80.0),
    ("ticks", "Ticks", 110.0),
    ("self", "Self %", 70.0),
    ("calls", "Calls", 70.0),
    ("hidden", "Hidden", 170.0),
];

impl ProfileRows {
    pub fn new(view: Rc<RefCell<ProfileView>>) -> Self {
        let columns = COLUMNS
            .iter()
            .map(|(id, title, width)| Column::new(*id, *title).width(*width).sortable(false))
            .collect();
        ProfileRows { view, columns }
    }
}

impl RowModel for ProfileRows {
    fn len(&self) -> usize {
        self.view.borrow().tree.len()
    }

    fn key(&self, row: usize) -> u64 {
        self.view.borrow().tree.row_key(row).unwrap_or(row as u64)
    }

    fn columns(&self) -> &[Column] {
        &self.columns
    }

    fn cell(&self, row: usize, col: usize) -> Cell<'_> {
        let view = self.view.borrow();
        let Some(r) = view.tree.row(row) else {
            return Cell::Empty;
        };
        if col == 0 {
            return Cell::Text(Cow::Owned(label(&view.tree, r)));
        }
        let Row::Node { key, .. } = r else {
            return Cell::Empty;
        };
        let Some(n) = view.tree.item(key) else {
            return Cell::Empty;
        };
        match col {
            1 => Cell::Text(Cow::Owned(percent_of(n.inclusive, view.root_cost))),
            2 => Cell::Int(i64::try_from(n.inclusive).unwrap_or(i64::MAX)),
            3 => Cell::Text(Cow::Owned(percent_of(n.self_cost, view.root_cost))),
            4 => Cell::Text(Cow::Owned(format!("×{}", n.calls))),
            5 => Cell::Text(Cow::Owned(hidden_text(n, &view.threshold, view.root_cost))),
            _ => Cell::Empty,
        }
    }

    fn depth(&self, _row: usize) -> u32 {
        0
    }

    fn version(&self) -> u64 {
        self.view.borrow().tree.version()
    }

    fn label(&self, row: usize) -> Option<Cow<'_, str>> {
        let view = self.view.borrow();
        let Row::Node { key, .. } = view.tree.row(row)? else {
            return None;
        };
        Some(Cow::Owned(view.tree.item(key)?.function.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::api::{CallId, Hidden};

    fn node(
        id: u64,
        f: &str,
        inclusive: u64,
        self_cost: u64,
        children: u64,
    ) -> (ProfileNodeId, ProfileNode) {
        (
            ProfileNodeId(id),
            ProfileNode {
                id: ProfileNodeId(id),
                parent: None,
                function: f.into(),
                location: None,
                calls: id + 1,
                inclusive,
                self_cost,
                pinned: false,
                child_count: children,
                hidden: Hidden::default(),
                first_call: CallId(id),
                recursion: 0,
            },
        )
    }

    #[test]
    fn cells_are_formatted_from_the_tree_as_drawn() {
        let view = Rc::new(RefCell::new(ProfileView::new(50)));
        let mut main = node(1, "main", 1000, 100, 2);
        main.1.hidden = Hidden { nodes: 12, cost: 5 };
        let mut walk = node(2, "walk", 400, 400, 0);
        walk.1.recursion = 3;
        walk.1.pinned = true;
        {
            let mut v = view.borrow_mut();
            v.root_cost = 1000;
            v.threshold = Threshold {
                of_root: 0.01,
                of_parent: None,
            };
            v.tree.set_roots(vec![main, walk]);
        }
        let rows = ProfileRows::new(view.clone());
        assert_eq!(rows.len(), 2);
        let text = |s: &str| Cell::Text(Cow::Owned(s.into()));
        assert_eq!(rows.cell(0, 0), text("▸ main"));
        assert_eq!(rows.cell(0, 1), text("100.0%"));
        assert_eq!(rows.cell(0, 2), Cell::Int(1000));
        assert_eq!(rows.cell(0, 3), text("10.0%"));
        assert_eq!(rows.cell(0, 4), text("×2"));
        assert_eq!(rows.cell(0, 5), text("+12 below 1% (0.5%)"));
        assert_eq!(rows.cell(1, 0), text("   walk  ↻3  ◆"));
        assert_eq!(rows.cell(1, 1), text("40.0%"));
        assert_eq!(rows.cell(1, 5), text(""));
        let v = rows.version();
        let fetch = view
            .borrow_mut()
            .tree
            .toggle(&ProfileNodeId(1))
            .expect("fetches main's children");
        assert_eq!(fetch.parent, Some(ProfileNodeId(1)));
        assert!(rows.version() > v);
        assert_eq!(rows.cell(1, 0), text("       loading…"));
    }

    #[test]
    fn percentages_and_thresholds_are_labelled() {
        assert_eq!(percent_of(1, 0), "-");
        assert_eq!(percent_of(1, 3), "33.3%");
        assert_eq!(fraction_label(0.001), "0.1%");
        assert_eq!(fraction_label(0.005), "0.5%");
        assert_eq!(fraction_label(0.05), "5%");
        assert_eq!(
            ThresholdPreset::of(&Threshold {
                of_root: 0.05,
                of_parent: None
            }),
            Some(ThresholdPreset::FivePercent)
        );
    }
}
