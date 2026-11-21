//! The fake's `profile`: the calling-context tree over the scripted calls.
//!
//! The same semantics as hopper-index's builder, written for a few dozen calls (recursive, no
//! caches): merge by path of shown functions, `CodeScope::Own` transparency, recursion folded
//! into the topmost occurrence, thresholds, pins and the hidden summary.

use hopper_core::api::{
    CallId, CallNode, CodeScope, Hidden, ProfileNode, ProfileNodeId, ProfileRequest,
};
use hopper_core::{HopperError, Moment, Result};

struct Node {
    function: String,
    location: Option<hopper_core::SourceLoc>,
    parent: Option<usize>,
    calls: u64,
    inclusive: u64,
    self_cost: u64,
    pinned: bool,
    first_call: CallId,
    recursion: u32,
    active: u32,
    shown: Vec<usize>,
    hidden: Hidden,
}

struct Walk<'a> {
    calls: &'a [CallNode],
    scope: CodeScope,
    pinned_calls: Vec<CallId>,
    /// Where a call that never returned ends.
    end_of_coverage: Moment,
    nodes: Vec<Node>,
}

impl Walk<'_> {
    fn cost(&self, c: &CallNode) -> u64 {
        let end = c.exit.unwrap_or(self.end_of_coverage);
        end.ticks.saturating_sub(c.entry.ticks)
    }

    fn is_own(&self, c: &CallNode) -> bool {
        match self.scope {
            CodeScope::All => true,
            CodeScope::Own => c.location.as_ref().is_some_and(|l| l.is_own_code()),
        }
    }

    fn visit(&mut self, c: &CallNode, parent_target: Option<usize>) -> u64 {
        let cost = self.cost(c);
        let mut target = parent_target;
        let mut counted = None;
        if self.is_own(c) {
            let mut topmost = None;
            let mut cur = parent_target;
            while let Some(n) = cur {
                if self.nodes[n].function == c.function {
                    topmost = Some(n);
                }
                cur = self.nodes[n].parent;
            }
            let n = match topmost {
                Some(n) => n,
                None => self.child_node(parent_target, c),
            };
            let node = &mut self.nodes[n];
            node.calls += 1;
            node.recursion = node.recursion.max(node.active);
            let outermost = node.active == 0;
            node.active += 1;
            if outermost {
                node.inclusive += cost;
            }
            target = Some(n);
            counted = Some(n);
        }
        if let Some(n) = target
            && self.pinned_calls.contains(&c.id)
        {
            self.nodes[n].pinned = true;
        }
        let calls = self.calls;
        let child_cost: u64 = calls
            .iter()
            .filter(|k| k.parent == Some(c.id))
            .map(|k| self.visit(k, target))
            .sum();
        if let Some(n) = target {
            self.nodes[n].self_cost += cost.saturating_sub(child_cost);
        }
        if let Some(n) = counted {
            self.nodes[n].active -= 1;
        }
        cost
    }

    fn child_node(&mut self, parent: Option<usize>, c: &CallNode) -> usize {
        let existing = self
            .nodes
            .iter()
            .position(|n| n.parent == parent && n.function == c.function);
        if let Some(n) = existing {
            return n;
        }
        self.nodes.push(Node {
            function: c.function.clone(),
            location: c.location.clone(),
            parent,
            calls: 0,
            inclusive: 0,
            self_cost: 0,
            pinned: false,
            first_call: c.id,
            recursion: 0,
            active: 0,
            shown: Vec::new(),
            hidden: Hidden::default(),
        });
        self.nodes.len() - 1
    }
}

/// The deepest call of the pin's thread that is active at the pin.
fn active_at(calls: &[CallNode], pin: &Moment) -> Option<CallId> {
    calls
        .iter()
        .filter(|c| {
            c.tid == pin.tid && !pin.is_before(&c.entry) && c.exit.is_none_or(|e| !e.is_before(pin))
        })
        .max_by_key(|c| c.depth)
        .map(|c| c.id)
}

/// The top-level node indexes and the node list of `req` over `calls`.
fn profile_nodes(
    calls: &[CallNode],
    end_of_coverage: Moment,
    req: &ProfileRequest,
) -> Result<(Vec<usize>, Vec<Node>)> {
    let roots: Vec<&CallNode> = match req.root {
        Some(id) => vec![
            calls
                .iter()
                .find(|c| c.id == id)
                .ok_or_else(|| HopperError::NotFound(format!("call {}", id.0)))?,
        ],
        None => calls.iter().filter(|c| c.parent.is_none()).collect(),
    };
    let roots: Vec<&CallNode> = roots
        .into_iter()
        .filter(|c| req.tid.is_none_or(|t| t == c.tid))
        .collect();
    let mut w = Walk {
        calls,
        scope: req.scope,
        pinned_calls: req
            .pins
            .iter()
            .filter_map(|p| active_at(calls, p))
            .collect(),
        end_of_coverage,
        nodes: Vec::new(),
    };
    let root_cost: u64 = roots.iter().map(|c| w.visit(c, None)).sum();
    for n in (0..w.nodes.len()).rev() {
        if w.nodes[n].pinned
            && let Some(p) = w.nodes[n].parent
        {
            w.nodes[p].pinned = true;
        }
    }
    let of_root = f64::from(req.threshold.of_root) * root_cost as f64;
    let mut visible = vec![false; w.nodes.len()];
    let mut top = Vec::new();
    for n in 0..w.nodes.len() {
        let parent = w.nodes[n].parent;
        let (parent_visible, parent_inclusive) = match parent {
            None => (true, root_cost),
            Some(p) => (visible[p], w.nodes[p].inclusive),
        };
        if !parent_visible {
            continue;
        }
        let inclusive = w.nodes[n].inclusive;
        let passes = inclusive as f64 >= of_root
            || req
                .threshold
                .of_parent
                .is_some_and(|f| inclusive as f64 >= f64::from(f) * parent_inclusive as f64)
            || w.nodes[n].pinned;
        match (passes, parent) {
            (true, None) => top.push(n),
            (true, Some(p)) => w.nodes[p].shown.push(n),
            (false, Some(p)) => {
                w.nodes[p].hidden.nodes += 1;
                w.nodes[p].hidden.cost += inclusive;
            }
            (false, None) => {}
        }
        visible[n] = passes;
    }
    let order = |nodes: &[Node], list: &mut Vec<usize>| {
        list.sort_by(|&a, &b| nodes[b].inclusive.cmp(&nodes[a].inclusive).then(a.cmp(&b)));
    };
    order(&w.nodes, &mut top);
    for n in 0..w.nodes.len() {
        let mut list = std::mem::take(&mut w.nodes[n].shown);
        order(&w.nodes, &mut list);
        w.nodes[n].shown = list;
    }
    Ok((top, w.nodes))
}

/// The shown nodes under `parent` (the top-level ones for `None`) of `req` over `calls`, which
/// are in entry order; `end_of_coverage` is where calls that never returned end.
pub(crate) fn profile_children(
    calls: &[CallNode],
    end_of_coverage: Moment,
    req: &ProfileRequest,
    parent: Option<ProfileNodeId>,
) -> Result<Vec<ProfileNode>> {
    let (top, nodes) = profile_nodes(calls, end_of_coverage, req)?;
    let top = &top;
    let ids: &[usize] = match parent {
        None => top,
        Some(p) => {
            &nodes
                .get(p.0 as usize)
                .ok_or_else(|| HopperError::NotFound(format!("profile node {}", p.0)))?
                .shown
        }
    };
    Ok(ids
        .iter()
        .map(|&i| {
            let n = &nodes[i];
            ProfileNode {
                id: ProfileNodeId(i as u64),
                parent: n.parent.map(|p| ProfileNodeId(p as u64)),
                function: n.function.clone(),
                location: n.location.clone(),
                calls: n.calls,
                inclusive: n.inclusive,
                self_cost: n.self_cost,
                pinned: n.pinned,
                child_count: n.shown.len() as u64,
                hidden: n.hidden,
                first_call: n.first_call,
                recursion: n.recursion,
            }
        })
        .collect())
}
