//! The call tree: skelter's virtual `table` reading a Rust `RowModel` over a [`LazyTree`] of
//! [`CallNode`]s, paged in through `Debugger::calls`.
//!
//! The table realises only the rows in view and asks the model for cells as it draws them, so an
//! expand costs a splice of small row records, never a copy of every label (skelter's `tree-view`
//! does that; `docs/requests.md`). The table has no tree disclosure of its own yet, so column 0
//! draws the indentation and the triangle as text, and activating a row (double click, Enter)
//! toggles it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use hopper_core::api::{CallId, CallNode, ValueView};
use skelter_base::rows::{Cell, Column, RowModel};

use crate::tree::{Expansion, LazyTree, Row, TreeItem};

impl TreeItem for CallNode {
    fn child_count(&self) -> Option<u64> {
        Some(self.child_count)
    }
}

impl TreeItem for ValueView {
    fn child_count(&self) -> Option<u64> {
        self.child_count
    }
}

pub type CallTree = LazyTree<CallId, CallNode>;

/// The table's model: the shared tree, read when the table draws.
#[derive(Debug)]
pub struct CallRows {
    pub tree: Rc<RefCell<CallTree>>,
    columns: Vec<Column>,
}

impl CallRows {
    pub fn new(tree: Rc<RefCell<CallTree>>) -> Self {
        let columns = vec![
            Column::new("function", "Function")
                .width(360.0)
                .sortable(false),
            Column::new("entry", "Entry").width(110.0).sortable(false),
            Column::new("exit", "Exit").width(110.0).sortable(false),
            Column::new("tid", "Thread").width(70.0).sortable(false),
            Column::new("calls", "Calls").width(70.0).sortable(false),
        ];
        CallRows { tree, columns }
    }
}

/// Column 0's text for a row: indentation, triangle and name.
pub fn label(tree: &CallTree, row: &Row<CallId>) -> String {
    let pad = "    ".repeat(row.depth() as usize);
    match row {
        Row::Node { key, .. } => {
            let tri = match tree.expansion(key) {
                Some(Expansion::Collapsed) => "▸ ",
                Some(Expansion::Expanded) => "▾ ",
                _ => "   ",
            };
            let name = tree.item(key).map_or("?", |c| c.function.as_str());
            format!("{pad}{tri}{name}")
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

impl RowModel for CallRows {
    fn len(&self) -> usize {
        self.tree.borrow().len()
    }

    fn key(&self, row: usize) -> u64 {
        self.tree.borrow().row_key(row).unwrap_or(row as u64)
    }

    fn columns(&self) -> &[Column] {
        &self.columns
    }

    fn cell(&self, row: usize, col: usize) -> Cell<'_> {
        let tree = self.tree.borrow();
        let Some(r) = tree.row(row) else {
            return Cell::Empty;
        };
        if col == 0 {
            return Cell::Text(Cow::Owned(label(&tree, r)));
        }
        let Row::Node { key, .. } = r else {
            return Cell::Empty;
        };
        let Some(c) = tree.item(key) else {
            return Cell::Empty;
        };
        match col {
            1 => Cell::Text(Cow::Owned(c.entry.to_string())),
            2 => match &c.exit {
                Some(m) => Cell::Text(Cow::Owned(m.to_string())),
                None => Cell::Text(Cow::Borrowed("(no return)")),
            },
            3 => Cell::Int(i64::from(c.tid.0)),
            4 => Cell::Int(c.child_count as i64),
            _ => Cell::Empty,
        }
    }

    fn depth(&self, _row: usize) -> u32 {
        // the indentation is drawn in column 0: as a tree, the table's view would hide rows it
        // thinks are collapsed
        0
    }

    fn version(&self) -> u64 {
        self.tree.borrow().version()
    }

    fn label(&self, row: usize) -> Option<Cow<'_, str>> {
        let tree = self.tree.borrow();
        let r = tree.row(row)?;
        let Row::Node { key, .. } = r else {
            return None;
        };
        Some(Cow::Owned(tree.item(key)?.function.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::{Moment, RecordingId, Tid};

    fn call(id: u64, f: &str, children: u64) -> (CallId, CallNode) {
        let m = Moment {
            recording: RecordingId([0; 16]),
            frame_time: id,
            tid: Tid(9),
            ticks: 0,
            sub: 0,
        };
        (
            CallId(id),
            CallNode {
                id: CallId(id),
                parent: None,
                tid: Tid(9),
                function: f.into(),
                entry: m,
                exit: None,
                depth: 0,
                child_count: children,
                location: None,
            },
        )
    }

    #[test]
    fn cells_are_formatted_from_the_tree_as_drawn() {
        let tree = Rc::new(RefCell::new(CallTree::new(50)));
        tree.borrow_mut()
            .set_roots(vec![call(1, "main", 2), call(2, "exit", 0)]);
        let rows = CallRows::new(tree.clone());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.cell(0, 0), Cell::Text(Cow::Owned("▸ main".into())));
        assert_eq!(rows.cell(1, 0), Cell::Text(Cow::Owned("   exit".into())));
        assert_eq!(rows.cell(0, 2), Cell::Text(Cow::Borrowed("(no return)")));
        assert_eq!(rows.cell(0, 3), Cell::Int(9));
        let v = rows.version();
        let fetch = tree
            .borrow_mut()
            .toggle(&CallId(1))
            .expect("fetches main's children");
        assert_eq!(fetch.parent, Some(CallId(1)));
        assert!(rows.version() > v);
        assert_eq!(
            rows.cell(1, 0),
            Cell::Text(Cow::Owned("       loading…".into()))
        );
        tree.borrow_mut()
            .children_loaded(Some(&CallId(1)), 0, vec![call(3, "load", 0)], Some(2));
        assert_eq!(rows.cell(0, 0), Cell::Text(Cow::Owned("▾ main".into())));
        assert_eq!(
            rows.cell(1, 0),
            Cell::Text(Cow::Owned("       load".into()))
        );
        assert_eq!(
            rows.cell(2, 0),
            Cell::Text(Cow::Owned("       … 1 more (activate to load)".into()))
        );
    }
}
