//! The calling-context tree behind [`hopper_core::api::Debugger::profile`].
//!
//! [`build_profile`] walks the subtree of the profiled root once, depth first, over the
//! in-memory call rows. Each call is mapped to a node of a trie keyed by (parent node, function):
//! calls reaching a function by the same path of shown functions share a node. The walk keeps
//! only the chain of open calls (one [`Frame`] per level), so memory is the trie plus the walk's
//! stack, not a copy of the rows.
//!
//! - A call that is not own code under [`CodeScope::Own`] is transparent: it gets no node, its
//!   children attach to the nearest own ancestor's node and its self ticks count there.
//! - A call to a function already on the trie path is folded into the topmost node of that
//!   function. A node counts how many calls of it are open (`active`); only the outermost call
//!   adds its ticks to `inclusive`, so nested calls are not counted twice.
//! - After the walk, nodes are shown or hidden top-down by the threshold and the pins.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use hopper_core::api::{CallId, CodeScope, Hidden, ProfileNode, ProfileNodeId, ProfileRequest};
use hopper_core::{HopperError, Moment, RecordingId, Result, SourceLoc};

use crate::index::CallCache;
use crate::store::{Coverage, Stamp};

/// How many built trees an index keeps (the most recently asked).
const CACHED_TREES: usize = 8;

/// The identity of a request for caching; the floating-point thresholds compare by bits.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProfileKey {
    root: Option<CallId>,
    tid: Option<i32>,
    scope: CodeScope,
    of_root: u32,
    of_parent: Option<u32>,
    pins: Vec<Moment>,
}

impl ProfileKey {
    pub(crate) fn of(req: &ProfileRequest) -> ProfileKey {
        ProfileKey {
            root: req.root,
            tid: req.tid.map(|t| t.0),
            scope: req.scope,
            of_root: req.threshold.of_root.to_bits(),
            of_parent: req.threshold.of_parent.map(f32::to_bits),
            pins: req.pins.clone(),
        }
    }
}

/// Built trees, the most recently used last.
#[derive(Debug, Default)]
pub(crate) struct ProfileCache {
    trees: VecDeque<(ProfileKey, Arc<ProfileTree>)>,
}

impl ProfileCache {
    pub(crate) fn clear(&mut self) {
        if !self.trees.is_empty() {
            self.trees.clear();
        }
    }

    pub(crate) fn get(&mut self, key: &ProfileKey) -> Option<Arc<ProfileTree>> {
        let i = self.trees.iter().position(|(k, _)| k == key)?;
        let entry = self.trees.remove(i)?;
        let tree = Arc::clone(&entry.1);
        self.trees.push_back(entry);
        Some(tree)
    }

    pub(crate) fn insert(&mut self, key: ProfileKey, tree: Arc<ProfileTree>) {
        if self.trees.len() >= CACHED_TREES {
            self.trees.pop_front();
        }
        self.trees.push_back((key, tree));
    }
}

#[derive(Debug)]
struct TreeNode {
    fid: u32,
    parent: Option<u32>,
    calls: u64,
    inclusive: u64,
    self_cost: u64,
    pinned: bool,
    first_call: u64,
    recursion: u32,
    /// Shown children, most expensive first.
    shown: Vec<u32>,
    hidden: Hidden,
}

/// A built profile: the trie, with what the threshold shows decided.
#[derive(Debug)]
pub(crate) struct ProfileTree {
    nodes: Vec<TreeNode>,
    /// Shown top-level nodes, most expensive first.
    top: Vec<u32>,
    /// The row of the profiled root call, when one was named.
    pub(crate) root_row: Option<usize>,
}

impl ProfileTree {
    /// The shown children of `parent` (the top-level nodes for `None`).
    pub(crate) fn children(&self, parent: Option<ProfileNodeId>) -> Result<&[u32]> {
        match parent {
            None => Ok(&self.top),
            Some(id) => self
                .nodes
                .get(id.0 as usize)
                .map(|n| n.shown.as_slice())
                .ok_or_else(|| HopperError::NotFound(format!("profile node {}", id.0))),
        }
    }

    pub(crate) fn node(
        &self,
        index: u32,
        functions: &HashMap<u32, (String, Option<SourceLoc>)>,
    ) -> ProfileNode {
        let n = &self.nodes[index as usize];
        let (function, location) = functions
            .get(&n.fid)
            .cloned()
            .unwrap_or_else(|| (format!("<function {}>", n.fid), None));
        ProfileNode {
            id: ProfileNodeId(u64::from(index)),
            parent: n.parent.map(|p| ProfileNodeId(u64::from(p))),
            function,
            location,
            calls: n.calls,
            inclusive: n.inclusive,
            self_cost: n.self_cost,
            pinned: n.pinned,
            child_count: n.shown.len() as u64,
            hidden: n.hidden,
            first_call: CallId(n.first_call),
            recursion: n.recursion,
        }
    }
}

/// One open call of the walk.
struct Frame {
    tid: i32,
    entry_ticks: u64,
    exit_ticks: Option<u64>,
    /// The node this call's self ticks belong to; `None` above every own call.
    target: Option<u32>,
    /// The node whose open-call count this call holds.
    counted: Option<u32>,
    /// This call is the outermost open call of `counted`: it adds its cost to `inclusive`.
    outermost: bool,
    /// The latest tick seen among this call's returned descendants on its thread.
    last_ticks: u64,
    /// The summed cost of its direct children on its thread.
    child_cost: u64,
}

enum Work {
    Enter(usize),
    Exit,
}

struct Builder<'a> {
    cache: &'a CallCache,
    scope: CodeScope,
    own: HashMap<u32, bool>,
    nodes: Vec<TreeNode>,
    active: Vec<u32>,
    edges: HashMap<u64, u32>,
    pin_rows: HashSet<usize>,
    /// The latest covered tick of each thread, for calls that never returned.
    covered_through: Vec<Stamp>,
    root_cost: u64,
}

impl Builder<'_> {
    fn is_own(&mut self, fid: u32) -> bool {
        match self.scope {
            CodeScope::All => true,
            CodeScope::Own => {
                let cache = self.cache;
                *self.own.entry(fid).or_insert_with(|| {
                    cache
                        .functions
                        .get(&fid)
                        .and_then(|(_, loc)| loc.as_ref())
                        .is_some_and(SourceLoc::is_own_code)
                })
            }
        }
    }

    /// The topmost node of `fid` on the path from `from` to the top.
    fn topmost_on_path(&self, from: Option<u32>, fid: u32) -> Option<u32> {
        let mut found = None;
        let mut cur = from;
        while let Some(n) = cur {
            let node = &self.nodes[n as usize];
            if node.fid == fid {
                found = Some(n);
            }
            cur = node.parent;
        }
        found
    }

    fn child_node(&mut self, parent: Option<u32>, fid: u32, first_call: u64) -> u32 {
        let key = (parent.map_or(0, |p| u64::from(p) + 1) << 32) | u64::from(fid);
        if let Some(&n) = self.edges.get(&key) {
            return n;
        }
        let n = self.nodes.len() as u32;
        self.nodes.push(TreeNode {
            fid,
            parent,
            calls: 0,
            inclusive: 0,
            self_cost: 0,
            pinned: false,
            first_call,
            recursion: 0,
            shown: Vec::new(),
            hidden: Hidden::default(),
        });
        self.active.push(0);
        self.edges.insert(key, n);
        n
    }

    fn covered_ticks(&self, tid: i32) -> u64 {
        self.covered_through
            .iter()
            .filter(|s| s.1 == tid)
            .map(|s| s.2)
            .max()
            .unwrap_or(0)
    }

    fn enter(&mut self, row_index: usize, parent: Option<&Frame>) -> Frame {
        let row = &self.cache.rows[row_index];
        let parent_target = parent.and_then(|f| f.target);
        let (target, counted, outermost) = if self.is_own(row.fid) {
            let n = match self.topmost_on_path(parent_target, row.fid) {
                Some(n) => n,
                None => self.child_node(parent_target, row.fid, row.id),
            };
            let before = self.active[n as usize];
            let node = &mut self.nodes[n as usize];
            node.calls += 1;
            node.recursion = node.recursion.max(before);
            self.active[n as usize] += 1;
            (Some(n), Some(n), before == 0)
        } else {
            (parent_target, None, false)
        };
        if let Some(n) = target
            && self.pin_rows.contains(&row_index)
        {
            self.nodes[n as usize].pinned = true;
        }
        Frame {
            tid: row.entry.1,
            entry_ticks: row.entry.2,
            exit_ticks: row.exit.map(|x| x.2),
            target,
            counted,
            outermost,
            last_ticks: row.entry.2,
            child_cost: 0,
        }
    }

    fn exit(&mut self, frame: Frame, parent: Option<&mut Frame>) {
        let end = frame.exit_ticks.unwrap_or_else(|| {
            frame
                .last_ticks
                .max(frame.entry_ticks)
                .max(self.covered_ticks(frame.tid))
        });
        let cost = end.saturating_sub(frame.entry_ticks);
        if let Some(n) = frame.counted {
            self.active[n as usize] -= 1;
            if frame.outermost {
                self.nodes[n as usize].inclusive += cost;
            }
        }
        if let Some(n) = frame.target {
            self.nodes[n as usize].self_cost += cost.saturating_sub(frame.child_cost);
        }
        match parent {
            Some(p) => {
                if p.tid == frame.tid {
                    p.child_cost += cost;
                    p.last_ticks = p.last_ticks.max(end);
                }
            }
            None => self.root_cost += cost,
        }
    }
}

/// The row of the deepest call of `pin`'s thread that is active at `pin`, below one of `starts`.
fn deepest_active(cache: &CallCache, starts: &[usize], pin: &Moment) -> Option<usize> {
    let key = (pin.frame_time, pin.ticks, pin.sub);
    let contains = |i: usize| {
        let r = &cache.rows[i];
        r.entry.1 == pin.tid.0 && r.entry.key() <= key && r.exit.is_none_or(|x| key <= x.key())
    };
    let mut cur = starts.iter().copied().find(|&i| contains(i))?;
    while let Some(kids) = cache.children.get(&cache.rows[cur].id) {
        let after = kids.partition_point(|id| {
            cache
                .by_id
                .get(id)
                .is_some_and(|&i| cache.rows[i].entry.key() <= key)
        });
        let Some(next) = after
            .checked_sub(1)
            .and_then(|k| cache.by_id.get(&kids[k]).copied())
            .filter(|&i| contains(i))
        else {
            break;
        };
        cur = next;
    }
    Some(cur)
}

/// Build the profile of `req` over `cache`. `coverage` bounds the cost of calls that never
/// returned; `recording` names the recording whose pins count.
pub(crate) fn build_profile(
    cache: &CallCache,
    coverage: &Coverage,
    recording: RecordingId,
    req: &ProfileRequest,
) -> Result<ProfileTree> {
    let root_row = match req.root {
        Some(id) => Some(
            cache
                .by_id
                .get(&id.0)
                .copied()
                .ok_or_else(|| HopperError::NotFound(format!("call {}", id.0)))?,
        ),
        None => None,
    };
    let mut starts: Vec<usize> = match root_row {
        Some(i) => vec![i],
        None => cache
            .roots
            .iter()
            .filter_map(|id| cache.by_id.get(id).copied())
            .collect(),
    };
    if let Some(t) = req.tid {
        starts.retain(|&i| cache.rows[i].entry.1 == t.0);
    }
    let pin_rows: HashSet<usize> = req
        .pins
        .iter()
        .filter(|p| p.recording == recording)
        .filter_map(|p| deepest_active(cache, &starts, p))
        .collect();
    let mut b = Builder {
        cache,
        scope: req.scope,
        own: HashMap::new(),
        nodes: Vec::new(),
        active: Vec::new(),
        edges: HashMap::new(),
        pin_rows,
        covered_through: coverage
            .intervals
            .iter()
            .filter_map(|iv| iv.through)
            .collect(),
        root_cost: 0,
    };
    let mut work: Vec<Work> = starts.iter().rev().map(|&i| Work::Enter(i)).collect();
    let mut frames: Vec<Frame> = Vec::new();
    while let Some(w) = work.pop() {
        match w {
            Work::Enter(i) => {
                let frame = b.enter(i, frames.last());
                frames.push(frame);
                work.push(Work::Exit);
                if let Some(kids) = cache.children.get(&cache.rows[i].id) {
                    work.extend(
                        kids.iter()
                            .rev()
                            .filter_map(|id| cache.by_id.get(id))
                            .map(|&k| Work::Enter(k)),
                    );
                }
            }
            Work::Exit => {
                if let Some(frame) = frames.pop() {
                    b.exit(frame, frames.last_mut());
                }
            }
        }
    }
    let Builder {
        mut nodes,
        root_cost,
        ..
    } = b;
    // Nodes are created after their parents, so one reverse pass lifts pins to every ancestor.
    for n in (0..nodes.len()).rev() {
        if nodes[n].pinned
            && let Some(p) = nodes[n].parent
        {
            nodes[p as usize].pinned = true;
        }
    }
    // One forward pass decides what is shown: a node is shown when its parent is and it passes a
    // threshold or is pinned.
    let of_root = f64::from(req.threshold.of_root) * root_cost as f64;
    let mut visible = vec![false; nodes.len()];
    let mut kids: Vec<Vec<u32>> = vec![Vec::new(); nodes.len()];
    let mut top: Vec<u32> = Vec::new();
    for n in 0..nodes.len() {
        let parent = nodes[n].parent;
        let (parent_visible, parent_inclusive) = match parent {
            None => (true, root_cost),
            Some(p) => (visible[p as usize], nodes[p as usize].inclusive),
        };
        if !parent_visible {
            continue;
        }
        let inclusive = nodes[n].inclusive;
        let passes = inclusive as f64 >= of_root
            || req
                .threshold
                .of_parent
                .is_some_and(|f| inclusive as f64 >= f64::from(f) * parent_inclusive as f64)
            || nodes[n].pinned;
        if passes {
            visible[n] = true;
            match parent {
                None => top.push(n as u32),
                Some(p) => kids[p as usize].push(n as u32),
            }
        } else if let Some(p) = parent {
            let hidden = &mut nodes[p as usize].hidden;
            hidden.nodes += 1;
            hidden.cost += inclusive;
        }
    }
    let by_cost = |nodes: &[TreeNode], list: &mut Vec<u32>| {
        list.sort_by(|&a, &b| {
            nodes[b as usize]
                .inclusive
                .cmp(&nodes[a as usize].inclusive)
                .then(a.cmp(&b))
        });
    };
    by_cost(&nodes, &mut top);
    for (n, mut list) in kids.into_iter().enumerate() {
        if !list.is_empty() {
            by_cost(&nodes, &mut list);
            nodes[n].shown = list;
        }
    }
    Ok(ProfileTree {
        nodes,
        top,
        root_row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::CallRow;
    use crate::store::{CoverageScope, CoveredInterval};
    use hopper_core::api::Threshold;
    use hopper_core::{ProcessId, Tid};

    const REC: RecordingId = RecordingId([3; 16]);
    const MAIN: u32 = 0;
    const A: u32 = 1;
    const B: u32 = 2;
    const STD: u32 = 3;
    const REC_FN: u32 = 4;
    const E: u32 = 5;
    const G: u32 = 6;
    const NO_LOCATION: u32 = 7;

    /// A call: id, function, parent id, entry tick and exit tick. Frame time equals ticks.
    type Spec = (u64, u32, Option<u64>, u64, Option<u64>);

    fn functions() -> HashMap<u32, (String, Option<SourceLoc>)> {
        let own = |n: &str| (n.to_string(), Some(SourceLoc::new("/p/src/lib.rs", 1)));
        HashMap::from([
            (MAIN, own("main")),
            (A, own("a")),
            (B, own("b")),
            (
                STD,
                (
                    "std::thing".to_string(),
                    Some(SourceLoc::new("/rustc/abc/library/std/src/lib.rs", 1)),
                ),
            ),
            (REC_FN, own("rec")),
            (E, own("e")),
            (G, own("g")),
            (NO_LOCATION, ("anonymous".to_string(), None)),
        ])
    }

    fn stamp(t: u64) -> Stamp {
        Stamp(t, 1, t, 0)
    }

    fn cache_of(specs: &[Spec]) -> CallCache {
        let mut c = CallCache {
            functions: functions(),
            ..Default::default()
        };
        for &(id, fid, parent, enter, exit) in specs {
            let depth = parent.map_or(0, |p| c.rows[c.by_id[&p]].depth + 1);
            c.by_id.insert(id, c.rows.len());
            c.rows.push(CallRow {
                id,
                fid,
                parent,
                process: ProcessId {
                    pid: 1,
                    exec_generation: 0,
                },
                entry: stamp(enter),
                exit: exit.map(stamp),
                depth,
            });
            match parent {
                Some(p) => c.children.entry(p).or_default().push(id),
                None => c.roots.push(id),
            }
        }
        c
    }

    fn request(scope: CodeScope, of_root: f32, of_parent: Option<f32>) -> ProfileRequest {
        ProfileRequest {
            scope,
            threshold: Threshold { of_root, of_parent },
            ..ProfileRequest::new()
        }
    }

    fn show_all() -> ProfileRequest {
        request(CodeScope::All, 0.0, None)
    }

    fn build(c: &CallCache, req: &ProfileRequest) -> ProfileTree {
        build_profile(c, &Coverage::default(), REC, req).unwrap()
    }

    /// The shown nodes at `path` (function names from the top), or panic.
    fn node_at(t: &ProfileTree, c: &CallCache, path: &[&str]) -> ProfileNode {
        let mut parent = None;
        let mut found = None;
        for want in path {
            let kids = t.children(parent).unwrap();
            let n = kids
                .iter()
                .map(|&k| t.node(k, &c.functions))
                .find(|n| n.function == *want)
                .unwrap_or_else(|| panic!("no shown node {want} under {parent:?}"));
            parent = Some(n.id);
            found = Some(n);
        }
        found.unwrap()
    }

    fn names(t: &ProfileTree, c: &CallCache, parent: Option<ProfileNodeId>) -> Vec<String> {
        t.children(parent)
            .unwrap()
            .iter()
            .map(|&k| t.node(k, &c.functions).function)
            .collect()
    }

    #[test]
    fn calls_with_the_same_path_merge_into_one_node() {
        let c = cache_of(&[
            (0, MAIN, None, 0, Some(100)),
            (1, A, Some(0), 10, Some(30)),
            (2, A, Some(0), 40, Some(70)),
            (3, B, Some(0), 80, Some(90)),
        ]);
        let t = build(&c, &show_all());
        let main = node_at(&t, &c, &["main"]);
        assert_eq!((main.calls, main.inclusive, main.self_cost), (1, 100, 40));
        assert_eq!(main.parent, None);
        assert_eq!(main.child_count, 2);
        assert_eq!(names(&t, &c, Some(main.id)), ["a", "b"]);
        let a = node_at(&t, &c, &["main", "a"]);
        assert_eq!((a.calls, a.inclusive, a.self_cost), (2, 50, 50));
        assert_eq!(a.first_call, CallId(1));
        assert_eq!(a.parent, Some(main.id));
        let b = node_at(&t, &c, &["main", "b"]);
        assert_eq!((b.calls, b.inclusive), (1, 10));
    }

    #[test]
    fn own_scope_makes_foreign_calls_transparent() {
        let c = cache_of(&[
            (0, MAIN, None, 0, Some(100)),
            (1, STD, Some(0), 10, Some(60)),
            (2, A, Some(1), 20, Some(40)),
            (3, A, Some(0), 70, Some(80)),
            (4, NO_LOCATION, Some(0), 85, Some(95)),
        ]);
        let all = build(&c, &show_all());
        assert_eq!(
            node_at(&all, &c, &["main", "std::thing", "a"]).inclusive,
            20
        );
        let own = build(&c, &request(CodeScope::Own, 0.0, None));
        let main = node_at(&own, &c, &["main"]);
        // main's own 30, std::thing's 30 and the location-less call's 10 are all main's.
        assert_eq!((main.inclusive, main.self_cost), (100, 30 + 30 + 10));
        assert_eq!(names(&own, &c, Some(main.id)), ["a"]);
        let a = node_at(&own, &c, &["main", "a"]);
        assert_eq!((a.calls, a.inclusive, a.self_cost), (2, 30, 30));
    }

    #[test]
    fn own_scope_with_a_foreign_root_attaches_own_callees_at_the_top() {
        let c = cache_of(&[(0, STD, None, 0, Some(100)), (1, A, Some(0), 10, Some(30))]);
        let t = build(&c, &request(CodeScope::Own, 0.0, None));
        assert_eq!(names(&t, &c, None), ["a"]);
        assert_eq!(node_at(&t, &c, &["a"]).parent, None);
    }

    #[test]
    fn recursion_folds_into_the_first_occurrence() {
        let c = cache_of(&[
            (0, REC_FN, None, 0, Some(100)),
            (1, REC_FN, Some(0), 10, Some(60)),
            (2, REC_FN, Some(1), 20, Some(30)),
            (3, A, Some(0), 70, Some(80)),
        ]);
        let t = build(&c, &show_all());
        assert_eq!(names(&t, &c, None), ["rec"]);
        let rec = node_at(&t, &c, &["rec"]);
        assert_eq!((rec.calls, rec.inclusive, rec.recursion), (3, 100, 2));
        // 40 + 40 + 10 self ticks over the three calls, and a's 10 below.
        assert_eq!(rec.self_cost, 90);
        assert_eq!(names(&t, &c, Some(rec.id)), ["a"]);
    }

    #[test]
    fn mutual_recursion_counts_each_tick_once() {
        // a calls rec, which calls a again: the inner a folds into the outer a.
        let c = cache_of(&[
            (0, A, None, 0, Some(100)),
            (1, REC_FN, Some(0), 10, Some(90)),
            (2, A, Some(1), 20, Some(50)),
        ]);
        let t = build(&c, &show_all());
        let a = node_at(&t, &c, &["a"]);
        assert_eq!(
            (a.calls, a.inclusive, a.recursion, a.self_cost),
            (2, 100, 1, 50)
        );
        let rec = node_at(&t, &c, &["a", "rec"]);
        assert_eq!((rec.calls, rec.inclusive, rec.self_cost), (1, 80, 50));
    }

    fn thresholds_tree() -> CallCache {
        cache_of(&[
            (0, MAIN, None, 0, Some(1000)),
            (1, A, Some(0), 0, Some(600)),
            (2, E, Some(1), 0, Some(100)),
            (3, G, Some(1), 100, Some(130)),
            (4, B, Some(0), 600, Some(1000)),
        ])
    }

    #[test]
    fn the_root_threshold_hides_small_nodes_and_summarises_them() {
        let c = thresholds_tree();
        let t = build(&c, &request(CodeScope::All, 0.3, None));
        let main = node_at(&t, &c, &["main"]);
        assert_eq!(names(&t, &c, Some(main.id)), ["a", "b"]);
        assert_eq!(main.hidden, Hidden::default());
        let a = node_at(&t, &c, &["main", "a"]);
        assert_eq!(
            (a.child_count, a.hidden),
            (
                0,
                Hidden {
                    nodes: 2,
                    cost: 130
                }
            )
        );
    }

    #[test]
    fn the_parent_threshold_keeps_local_structure() {
        let c = thresholds_tree();
        let t = build(&c, &request(CodeScope::All, 0.3, Some(0.1)));
        let a = node_at(&t, &c, &["main", "a"]);
        // e is 16% of a (shown); g is 5% of a and 3% of the root (hidden).
        assert_eq!(names(&t, &c, Some(a.id)), ["e"]);
        assert_eq!(a.hidden, Hidden { nodes: 1, cost: 30 });
    }

    #[test]
    fn pins_show_the_active_call_and_its_ancestors() {
        let c = thresholds_tree();
        let pin = Moment {
            recording: REC,
            frame_time: 110,
            tid: Tid(1),
            ticks: 110,
            sub: 0,
        };
        let req = ProfileRequest {
            pins: vec![pin],
            ..request(CodeScope::All, 0.3, None)
        };
        let t = build(&c, &req);
        let a = node_at(&t, &c, &["main", "a"]);
        assert_eq!(names(&t, &c, Some(a.id)), ["g"]);
        assert!(a.pinned && node_at(&t, &c, &["main"]).pinned);
        assert!(node_at(&t, &c, &["main", "a", "g"]).pinned);
        assert!(!node_at(&t, &c, &["main", "b"]).pinned);
        assert_eq!(
            a.hidden,
            Hidden {
                nodes: 1,
                cost: 100
            }
        );
    }

    #[test]
    fn pins_of_another_recording_or_thread_are_ignored() {
        let c = thresholds_tree();
        let pin = Moment {
            recording: REC,
            frame_time: 110,
            tid: Tid(2),
            ticks: 110,
            sub: 0,
        };
        let other = Moment {
            recording: RecordingId([4; 16]),
            tid: Tid(1),
            ..pin
        };
        let req = ProfileRequest {
            pins: vec![pin, other],
            ..request(CodeScope::All, 0.3, None)
        };
        let t = build(&c, &req);
        assert!(!node_at(&t, &c, &["main"]).pinned);
    }

    #[test]
    fn a_named_root_profiles_only_its_subtree() {
        let c = thresholds_tree();
        let req = ProfileRequest {
            root: Some(CallId(1)),
            ..show_all()
        };
        let t = build(&c, &req);
        assert_eq!(names(&t, &c, None), ["a"]);
        assert_eq!(t.root_row, Some(1));
        assert_eq!(names(&t, &c, Some(node_at(&t, &c, &["a"]).id)), ["e", "g"]);
        let missing = ProfileRequest {
            root: Some(CallId(99)),
            ..show_all()
        };
        assert!(build_profile(&c, &Coverage::default(), REC, &missing).is_err());
    }

    #[test]
    fn calls_that_never_returned_cost_up_to_the_end_of_coverage() {
        let c = cache_of(&[(0, MAIN, None, 0, None), (1, A, Some(0), 10, Some(20))]);
        // Without coverage the latest tick seen below the call is its end.
        let t = build(&c, &show_all());
        assert_eq!(node_at(&t, &c, &["main"]).inclusive, 20);
        let covered = Coverage {
            intervals: vec![CoveredInterval {
                from: None,
                through: Some(stamp(50)),
                scope: CoverageScope::AllThreads,
            }],
            capped: vec![],
        };
        let t = build_profile(&c, &covered, REC, &show_all()).unwrap();
        let main = node_at(&t, &c, &["main"]);
        assert_eq!((main.inclusive, main.self_cost), (50, 40));
    }

    #[test]
    fn unknown_node_ids_are_errors() {
        let c = thresholds_tree();
        let t = build(&c, &show_all());
        assert!(t.children(Some(ProfileNodeId(999))).is_err());
    }

    #[test]
    fn a_million_calls_profile_in_reasonable_time() {
        // main -> 1000 callers (12 functions) -> 999 leaves each (50 functions): 1 000 001 calls.
        let mut c = CallCache::default();
        for fid in 0..100u32 {
            c.functions.insert(
                fid,
                (
                    format!("f{fid}"),
                    Some(SourceLoc::new("/p/src/lib.rs", fid)),
                ),
            );
        }
        let process = ProcessId {
            pid: 1,
            exec_generation: 0,
        };
        let push = |c: &mut CallCache, id: u64, fid: u32, parent: Option<u64>, a: u64, b: u64| {
            c.by_id.insert(id, c.rows.len());
            c.rows.push(CallRow {
                id,
                fid,
                parent,
                process,
                entry: stamp(a),
                exit: Some(stamp(b)),
                depth: u32::from(parent.is_some()) * 2,
            });
            match parent {
                Some(p) => c.children.entry(p).or_default().push(id),
                None => c.roots.push(id),
            }
        };
        let mut id = 0u64;
        push(&mut c, id, 0, None, 0, 10_000_000);
        for caller in 0..1000u64 {
            id += 1;
            let caller_id = id;
            let start = 1 + caller * 9_000;
            push(
                &mut c,
                caller_id,
                1 + (caller % 12) as u32,
                Some(0),
                start,
                start + 8_999,
            );
            for leaf in 0..999u64 {
                id += 1;
                let a = start + leaf * 9;
                push(
                    &mut c,
                    id,
                    20 + (leaf % 50) as u32,
                    Some(caller_id),
                    a,
                    a + 8,
                );
            }
        }
        assert_eq!(c.rows.len(), 1_000_001);
        let started = std::time::Instant::now();
        let t = build(&c, &request(CodeScope::Own, 0.001, Some(0.5)));
        let elapsed = started.elapsed();
        let main = node_at(&t, &c, &["f0"]);
        assert_eq!(main.calls, 1);
        assert_eq!(t.nodes.len(), 1 + 12 + 12 * 50);
        // Each caller function has 1000/12 calls; main's children merge to 12 nodes.
        assert_eq!(t.children(Some(main.id)).unwrap().len(), 12);
        assert!(
            elapsed < std::time::Duration::from_secs(120),
            "profiling a million calls took {elapsed:?}"
        );
    }
}
