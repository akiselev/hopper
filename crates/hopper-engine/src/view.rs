//! The program state at one moment, as queries read it: registers, memory, symbols, the stack and
//! the evaluator, served from the [`crate::cache::MomentCache`] first and from a replica that is at
//! that moment otherwise.

use std::sync::Arc;

use hopper_core::api::{OwnershipState, ValueView};
use hopper_core::target::{Memory, Target};
use hopper_core::{FrameId, HopperError, Moment, RegisterSet, Result, Tid};
use hopper_eval::{EvalContext, FrameRef};
use hopper_symbols::StackFrame;

use crate::cache::{Fetch, MomentMemory};
use crate::symbols::ProcessSymbols;
use crate::Shared;

/// Fetches from the replica when there is one; otherwise notes the miss.
pub struct ViewFetch<'b, 'c> {
    target: Option<&'b mut (dyn Target + 'c)>,
    missed: &'b mut bool,
    at: Moment,
}

impl Fetch for ViewFetch<'_, '_> {
    fn fetch(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        match self.target.as_deref_mut() {
            Some(t) => t.read_memory(addr, buf),
            None => {
                *self.missed = true;
                crate::cache::CacheOnly { at: self.at }.fetch(addr, buf)
            }
        }
    }
}

/// One moment's state.
pub struct MomentView<'a> {
    pub shared: &'a Shared,
    pub at: Moment,
    target: Option<&'a mut dyn Target>,
    /// Something was not in the cache and no replica could supply it.
    pub missed: bool,
}

impl<'a> MomentView<'a> {
    pub fn cache_only(shared: &'a Shared, at: Moment) -> MomentView<'a> {
        MomentView {
            shared,
            at,
            target: None,
            missed: false,
        }
    }

    pub fn on(shared: &'a Shared, at: Moment, target: &'a mut dyn Target) -> MomentView<'a> {
        MomentView {
            shared,
            at,
            target: Some(target),
            missed: false,
        }
    }

    /// The replica itself (for queries that resume it).
    pub fn target(&mut self) -> Result<&mut dyn Target> {
        match self.target.as_deref_mut() {
            Some(t) => Ok(t),
            None => {
                self.missed = true;
                Err(HopperError::Unsupported(format!(
                    "no replica is at moment {}",
                    self.at
                )))
            }
        }
    }

    pub fn memory(&mut self) -> MomentMemory<'_, ViewFetch<'_, 'a>> {
        MomentMemory {
            cache: &self.shared.cache,
            at: self.at,
            source: ViewFetch {
                target: self.target.as_deref_mut(),
                missed: &mut self.missed,
                at: self.at,
            },
        }
    }

    pub fn registers(&mut self, tid: Tid) -> Result<RegisterSet> {
        if let Some(r) = self.shared.cache.registers(self.at, tid) {
            return Ok(r);
        }
        let regs = self.target()?.registers(tid)?;
        self.shared.cache.put_registers(self.at, tid, regs.clone());
        Ok(regs)
    }

    pub fn symbols(&mut self) -> Result<Arc<ProcessSymbols>> {
        let shared = self.shared;
        let mut mem = self.memory();
        shared
            .symbols
            .process(&shared.exe, &shared.auxv, &mut mem as &mut dyn Memory)
    }

    pub fn stack(&mut self, tid: Tid) -> Result<Arc<Vec<StackFrame>>> {
        if let Some(s) = self.shared.cache.stack(self.at, tid) {
            return Ok(s);
        }
        let regs = self.registers(tid)?;
        let syms = self.symbols()?;
        let stack = {
            let mut mem = self.memory();
            syms.set.stack(&mut mem as &mut dyn Memory, &regs, tid)?
        };
        let stack = Arc::new(stack);
        if !self.missed {
            self.shared.cache.put_stack(self.at, tid, stack.clone());
        }
        Ok(stack)
    }

    pub fn frame(&mut self, id: FrameId) -> Result<StackFrame> {
        let stack = self.stack(id.tid)?;
        stack.get(id.depth as usize).cloned().ok_or_else(|| {
            HopperError::NotFound(format!(
                "frame #{} of thread {} at {}",
                id.depth, id.tid, self.at
            ))
        })
    }

    /// Run the evaluator in frame `id`.
    pub fn with_eval<R>(
        &mut self,
        id: FrameId,
        f: impl FnOnce(&hopper_eval::Evaluator, &mut EvalContext<'_>) -> Result<R>,
    ) -> Result<R> {
        let frame = self.frame(id)?;
        let syms = self.symbols()?;
        let shared = self.shared;
        let at = self.at;
        let mut mem = self.memory();
        let mut cx = EvalContext {
            symbols: &syms.set,
            memory: &mut mem,
            frame: FrameRef::from_stack(&frame),
            at,
            process: shared.process,
        };
        f(&shared.evaluator, &mut cx)
    }
}

/// The root variable an expression names, when it is a plain path from one (`a`, `b.name`,
/// `v[0]`): ownership of that variable applies to it.
pub fn root_variable(expr: &str) -> Option<&str> {
    let e = expr.trim();
    let end = e
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(e.len());
    let (root, rest) = e.split_at(end);
    let starts_ok = root
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_');
    let rest_ok = rest.is_empty() || rest.starts_with('.') || rest.starts_with('[');
    (starts_ok && rest_ok).then_some(root)
}

/// Give every view whose root variable has a known state that state.
pub fn apply_ownership(
    views: &mut [ValueView],
    state_of: &dyn Fn(&str) -> Option<OwnershipState>,
) {
    for v in views {
        let root = match &v.handle.target {
            hopper_core::api::ValueTarget::Expr(e) => root_variable(e),
            hopper_core::api::ValueTarget::Place { .. } => None,
        };
        if let Some(s) = root.and_then(state_of) {
            v.ownership = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_variables() {
        assert_eq!(root_variable("a"), Some("a"));
        assert_eq!(root_variable("b.name"), Some("b"));
        assert_eq!(root_variable("v[0].name"), Some("v"));
        assert_eq!(root_variable("*n"), None);
        assert_eq!(root_variable("&b"), None);
        assert_eq!(root_variable("a + 1"), None);
        assert_eq!(root_variable("1"), None);
    }
}
