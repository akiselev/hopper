//! A lazy, paged tree flattened into visible rows: the view model behind the call tree and the
//! locals.
//!
//! skelter's `tree-view` re-flattens and clones every visible label on each expand, so it cannot
//! hold a call tree of 10^6 rows (`docs/requests.md`). This model keeps only:
//! - the rows that are visible, in pre-order, each a small [`Row`] (a key and a depth);
//! - every loaded node by key, with its expansion state.
//!
//! Expanding a node splices its children's rows in after it; collapsing removes its subtree's
//! rows. Children load lazily, a page at a time: an expand that has never loaded asks the caller
//! for a [`Fetch`], shows a loading row until [`LazyTree::children_loaded`] answers, and a
//! partly loaded level ends in a "more" row whose activation fetches the next page. Labels are
//! never copied: a view formats a row from [`LazyTree::item`] when it draws it.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use hopper_core::api::PageReq;

/// Whether an item can have children, as its source says.
pub trait TreeItem {
    /// `None` or `Some(0)`: a leaf.
    fn child_count(&self) -> Option<u64>;
}

/// One visible row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row<K> {
    /// A loaded node.
    Node { key: K, depth: u32 },
    /// Children of `parent` are being fetched.
    Loading { parent: Option<K>, depth: u32 },
    /// `parent` has more children than are loaded; activating this row fetches the next page.
    More {
        parent: Option<K>,
        depth: u32,
        loaded: u64,
        total: Option<u64>,
    },
}

impl<K> Row<K> {
    pub fn depth(&self) -> u32 {
        match self {
            Row::Node { depth, .. } | Row::Loading { depth, .. } | Row::More { depth, .. } => {
                *depth
            }
        }
    }
}

/// How a node's children stand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expansion {
    /// No children.
    Leaf,
    /// Children exist; not shown. `loaded` is how many are cached from an earlier expand.
    Collapsed,
    /// Children shown (possibly still loading, possibly partly paged in).
    Expanded,
}

/// A page of children the caller must fetch and hand back through
/// [`LazyTree::children_loaded`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetch<K> {
    /// `None`: the roots.
    pub parent: Option<K>,
    pub page: PageReq,
}

#[derive(Clone, Debug)]
struct Entry<K, T> {
    item: T,
    depth: u32,
    expansion: Expansion,
    /// Children loaded so far, in order.
    children: Vec<K>,
    /// The total the source reported, when it did.
    total: Option<u64>,
    /// A fetch for the next page is in flight.
    fetching: bool,
}

/// The tree. `K` names a node (a call id, a value handle); `T` is its data.
#[derive(Clone, Debug)]
pub struct LazyTree<K, T> {
    rows: Vec<Row<K>>,
    nodes: HashMap<K, Entry<K, T>>,
    roots: Vec<K>,
    root_total: Option<u64>,
    roots_fetching: bool,
    page_size: u32,
    /// Bumped by every change to the rows, so views know to redraw.
    version: u64,
}

impl<K: Clone + Eq + Hash, T: TreeItem> LazyTree<K, T> {
    /// An empty tree that fetches `page_size` children at a time.
    pub fn new(page_size: u32) -> Self {
        LazyTree {
            rows: Vec::new(),
            nodes: HashMap::new(),
            roots: Vec::new(),
            root_total: None,
            roots_fetching: false,
            page_size: page_size.max(1),
            version: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn rows(&self) -> &[Row<K>] {
        &self.rows
    }

    pub fn row(&self, i: usize) -> Option<&Row<K>> {
        self.rows.get(i)
    }

    pub fn item(&self, key: &K) -> Option<&T> {
        self.nodes.get(key).map(|e| &e.item)
    }

    pub fn expansion(&self, key: &K) -> Option<&Expansion> {
        self.nodes.get(key).map(|e| &e.expansion)
    }

    /// The row showing `key`, if it is visible.
    pub fn position(&self, key: &K) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| matches!(r, Row::Node { key: k, .. } if k == key))
    }

    /// A stable 64-bit key for row `i` (a table's row identity).
    pub fn row_key(&self, i: usize) -> Option<u64> {
        let mut h = DefaultHasher::new();
        match self.rows.get(i)? {
            Row::Node { key, .. } => {
                0u8.hash(&mut h);
                key.hash(&mut h);
            }
            Row::Loading { parent, .. } => {
                1u8.hash(&mut h);
                parent.hash(&mut h);
            }
            Row::More { parent, .. } => {
                2u8.hash(&mut h);
                parent.hash(&mut h);
            }
        }
        Some(h.finish())
    }

    /// Forgets everything and asks for the first page of roots.
    pub fn reset(&mut self) -> Fetch<K> {
        self.rows.clear();
        self.nodes.clear();
        self.roots.clear();
        self.root_total = None;
        self.roots_fetching = true;
        self.rows.push(Row::Loading {
            parent: None,
            depth: 0,
        });
        self.version += 1;
        Fetch {
            parent: None,
            page: PageReq {
                offset: 0,
                limit: self.page_size,
            },
        }
    }

    /// Replaces the tree with these roots, all of them (no paging).
    pub fn set_roots(&mut self, items: Vec<(K, T)>) {
        self.rows.clear();
        self.nodes.clear();
        self.roots.clear();
        self.roots_fetching = false;
        let n = items.len() as u64;
        self.root_total = Some(n);
        for (k, t) in items {
            self.insert_node(k.clone(), t, 0);
            self.roots.push(k.clone());
            self.rows.push(Row::Node { key: k, depth: 0 });
        }
        self.version += 1;
    }

    fn insert_node(&mut self, key: K, item: T, depth: u32) {
        let expansion = match item.child_count() {
            None | Some(0) => Expansion::Leaf,
            Some(_) => Expansion::Collapsed,
        };
        self.nodes.insert(
            key,
            Entry {
                item,
                depth,
                expansion,
                children: Vec::new(),
                total: None,
                fetching: false,
            },
        );
    }

    /// The rows of `key`'s subtree after its own row: its index plus one up to the next row at its
    /// depth or less.
    fn subtree_end(&self, at: usize) -> usize {
        let depth = self.rows[at].depth();
        let mut end = at + 1;
        while end < self.rows.len() && self.rows[end].depth() > depth {
            end += 1;
        }
        end
    }

    /// The rows a node's loaded children (and their expanded descendants) take, in pre-order.
    fn child_rows(&self, key: &K, out: &mut Vec<Row<K>>) {
        let Some(e) = self.nodes.get(key) else { return };
        for c in &e.children {
            let Some(ce) = self.nodes.get(c) else {
                continue;
            };
            out.push(Row::Node {
                key: c.clone(),
                depth: ce.depth,
            });
            if ce.expansion == Expansion::Expanded {
                self.child_rows(c, out);
            }
        }
        let depth = e.depth + 1;
        if e.fetching {
            out.push(Row::Loading {
                parent: Some(key.clone()),
                depth,
            });
        } else if more_remain(e.children.len() as u64, e.total) {
            out.push(Row::More {
                parent: Some(key.clone()),
                depth,
                loaded: e.children.len() as u64,
                total: e.total,
            });
        }
    }

    /// Expands a collapsed node or collapses an expanded one. Returns the page to fetch when the
    /// node has never loaded its children.
    pub fn toggle(&mut self, key: &K) -> Option<Fetch<K>> {
        match self.nodes.get(key).map(|e| e.expansion.clone()) {
            Some(Expansion::Collapsed) => self.expand(key),
            Some(Expansion::Expanded) => {
                self.collapse(key);
                None
            }
            _ => None,
        }
    }

    /// Shows a node's children, fetching the first page if none are loaded.
    pub fn expand(&mut self, key: &K) -> Option<Fetch<K>> {
        let page_size = self.page_size;
        let e = self.nodes.get_mut(key)?;
        if e.expansion != Expansion::Collapsed {
            return None;
        }
        e.expansion = Expansion::Expanded;
        let fetch = if e.children.is_empty() && !e.fetching {
            e.fetching = true;
            Some(Fetch {
                parent: Some(key.clone()),
                page: PageReq {
                    offset: 0,
                    limit: page_size,
                },
            })
        } else {
            None
        };
        if let Some(at) = self.position(key) {
            let mut rows = Vec::new();
            self.child_rows(key, &mut rows);
            self.rows.splice(at + 1..at + 1, rows);
        }
        self.version += 1;
        fetch
    }

    /// Hides a node's children (they stay cached).
    pub fn collapse(&mut self, key: &K) {
        let Some(e) = self.nodes.get_mut(key) else {
            return;
        };
        if e.expansion != Expansion::Expanded {
            return;
        }
        e.expansion = Expansion::Collapsed;
        if let Some(at) = self.position(key) {
            let end = self.subtree_end(at);
            self.rows.drain(at + 1..end);
        }
        self.version += 1;
    }

    /// Activates row `i`: toggles a node, or fetches the next page of a "more" row.
    pub fn activate(&mut self, i: usize) -> Option<Fetch<K>> {
        match self.rows.get(i)?.clone() {
            Row::Node { key, .. } => self.toggle(&key),
            Row::More {
                parent,
                depth,
                loaded,
                ..
            } => {
                let fetching = match &parent {
                    None => &mut self.roots_fetching,
                    Some(p) => &mut self.nodes.get_mut(p)?.fetching,
                };
                if *fetching {
                    return None;
                }
                *fetching = true;
                self.rows[i] = Row::Loading {
                    parent: parent.clone(),
                    depth,
                };
                self.version += 1;
                Some(Fetch {
                    parent,
                    page: PageReq {
                        offset: loaded,
                        limit: self.page_size,
                    },
                })
            }
            Row::Loading { .. } => None,
        }
    }

    /// A fetched page of `parent`'s children (`None`: the roots) starting at `offset`, with the
    /// total the source reported. A page for a node that is gone or no longer fetching is ignored.
    pub fn children_loaded(
        &mut self,
        parent: Option<&K>,
        offset: u64,
        items: Vec<(K, T)>,
        total: Option<u64>,
    ) {
        let depth = match parent {
            None => {
                if !self.roots_fetching || offset != self.roots.len() as u64 {
                    return;
                }
                self.roots_fetching = false;
                self.root_total = total;
                0
            }
            Some(p) => {
                let Some(e) = self.nodes.get_mut(p) else {
                    return;
                };
                if !e.fetching || offset != e.children.len() as u64 {
                    return;
                }
                e.fetching = false;
                e.total = total;
                e.depth + 1
            }
        };
        let got = items.len() as u64;
        let mut keys = Vec::with_capacity(items.len());
        for (k, t) in items {
            self.insert_node(k.clone(), t, depth);
            keys.push(k);
        }
        // a page shorter than asked ends the level even when the source gave no total
        let short = got < u64::from(self.page_size);
        let (loaded, total) = match parent {
            None => {
                self.roots.extend(keys.iter().cloned());
                if short && self.root_total.is_none() {
                    self.root_total = Some(self.roots.len() as u64);
                }
                (self.roots.len() as u64, self.root_total)
            }
            Some(p) => {
                let e = self.nodes.get_mut(p).expect("checked above");
                e.children.extend(keys.iter().cloned());
                if short && e.total.is_none() {
                    e.total = Some(e.children.len() as u64);
                }
                (e.children.len() as u64, e.total)
            }
        };
        let mut rows: Vec<Row<K>> = keys
            .into_iter()
            .map(|key| Row::Node { key, depth })
            .collect();
        if more_remain(loaded, total) {
            rows.push(Row::More {
                parent: parent.cloned(),
                depth,
                loaded,
                total,
            });
        }
        // the loading row this page answers, if the parent is shown
        let at = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Loading { parent: lp, .. } if lp.as_ref() == parent));
        if let Some(at) = at {
            self.rows.splice(at..at + 1, rows);
        }
        self.version += 1;
    }

    /// A fetch failed: the loading row goes, and the parent can be expanded again.
    pub fn fetch_failed(&mut self, parent: Option<&K>) {
        match parent {
            None => self.roots_fetching = false,
            Some(p) => {
                if let Some(e) = self.nodes.get_mut(p) {
                    e.fetching = false;
                    if e.children.is_empty() {
                        e.expansion = Expansion::Collapsed;
                    }
                }
            }
        }
        if let Some(at) = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Loading { parent: lp, .. } if lp.as_ref() == parent))
        {
            self.rows.remove(at);
        }
        self.version += 1;
    }
}

fn more_remain(loaded: u64, total: Option<u64>) -> bool {
    match total {
        Some(t) => loaded < t,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct N(Option<u64>);
    impl TreeItem for N {
        fn child_count(&self) -> Option<u64> {
            self.0
        }
    }

    fn shape(t: &LazyTree<u32, N>) -> Vec<String> {
        t.rows()
            .iter()
            .map(|r| match r {
                Row::Node { key, depth } => format!("{}{key}", "  ".repeat(*depth as usize)),
                Row::Loading { depth, .. } => format!("{}…", "  ".repeat(*depth as usize)),
                Row::More {
                    depth,
                    loaded,
                    total,
                    ..
                } => format!("{}+{loaded}/{total:?}", "  ".repeat(*depth as usize)),
            })
            .collect()
    }

    #[test]
    fn roots_page_in_and_end_with_a_more_row() {
        let mut t = LazyTree::<u32, N>::new(2);
        let f = t.reset();
        assert_eq!(
            f,
            Fetch {
                parent: None,
                page: PageReq {
                    offset: 0,
                    limit: 2
                }
            }
        );
        assert_eq!(shape(&t), ["…"]);
        t.children_loaded(None, 0, vec![(1, N(None)), (2, N(Some(3)))], Some(3));
        assert_eq!(shape(&t), ["1", "2", "+2/Some(3)"]);
        let f = t.activate(2).expect("more fetches");
        assert_eq!(
            f.page,
            PageReq {
                offset: 2,
                limit: 2
            }
        );
        assert_eq!(shape(&t), ["1", "2", "…"]);
        // activating the loading row does nothing
        assert_eq!(t.activate(2), None);
        t.children_loaded(None, 2, vec![(3, N(None))], Some(3));
        assert_eq!(shape(&t), ["1", "2", "3"]);
    }

    #[test]
    fn expand_fetches_once_then_collapse_and_reexpand_reuse_the_cache() {
        let mut t = LazyTree::<u32, N>::new(10);
        t.set_roots(vec![(1, N(Some(2))), (9, N(None))]);
        assert_eq!(t.toggle(&9), None, "a leaf does not expand");
        let f = t.toggle(&1).expect("first expand fetches");
        assert_eq!(f.parent, Some(1));
        assert_eq!(shape(&t), ["1", "  …", "9"]);
        t.children_loaded(Some(&1), 0, vec![(10, N(Some(1))), (11, N(None))], Some(2));
        assert_eq!(shape(&t), ["1", "  10", "  11", "9"]);
        let f = t.toggle(&10).expect("grandchildren fetch");
        t.children_loaded(f.parent.as_ref(), 0, vec![(100, N(None))], Some(1));
        assert_eq!(shape(&t), ["1", "  10", "    100", "  11", "9"]);
        t.toggle(&1);
        assert_eq!(shape(&t), ["1", "9"]);
        assert_eq!(t.toggle(&1), None, "re-expanding uses the cached children");
        assert_eq!(
            shape(&t),
            ["1", "  10", "    100", "  11", "9"],
            "and keeps their own expansion"
        );
    }

    #[test]
    fn a_page_shorter_than_asked_ends_a_level_without_a_total() {
        let mut t = LazyTree::<u32, N>::new(3);
        t.reset();
        t.children_loaded(
            None,
            0,
            vec![(1, N(None)), (2, N(None)), (3, N(None))],
            None,
        );
        assert_eq!(
            shape(&t),
            ["1", "2", "3", "+3/None"],
            "a full page may have more"
        );
        t.activate(3);
        t.children_loaded(None, 3, vec![(4, N(None))], None);
        assert_eq!(shape(&t), ["1", "2", "3", "4"]);
    }

    #[test]
    fn stale_and_failed_pages_leave_the_rows_consistent() {
        let mut t = LazyTree::<u32, N>::new(5);
        t.set_roots(vec![(1, N(Some(4)))]);
        t.toggle(&1);
        // a page at the wrong offset (a reply to an older request) is ignored
        t.children_loaded(Some(&1), 3, vec![(7, N(None))], Some(4));
        assert_eq!(shape(&t), ["1", "  …"]);
        t.fetch_failed(Some(&1));
        assert_eq!(shape(&t), ["1"]);
        assert_eq!(t.expansion(&1), Some(&Expansion::Collapsed));
        assert!(t.toggle(&1).is_some(), "it can be retried");
        // a page for a node that is gone is ignored
        t.children_loaded(Some(&42), 0, vec![(8, N(None))], Some(1));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn row_keys_are_stable_across_splices() {
        let mut t = LazyTree::<u32, N>::new(5);
        t.set_roots(vec![(1, N(Some(1))), (2, N(None))]);
        let k2 = t.row_key(1).unwrap();
        t.toggle(&1);
        t.children_loaded(Some(&1), 0, vec![(3, N(None))], Some(1));
        assert_eq!(t.position(&2), Some(2));
        assert_eq!(t.row_key(2), Some(k2));
        assert_ne!(t.row_key(0), t.row_key(1));
    }
}
