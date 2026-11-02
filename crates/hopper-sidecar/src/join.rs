//! The join: from what DWARF and the machine say about one PC to the ownership event the PC
//! carries out.
//!
//! The caller (the engine) passes plain data: the PC's frame as DWARF describes it (physical
//! function symbol, inline chain, line-table row), the frames that called it, and the machine
//! effect it observed. The join
//!
//! 1. finds the innermost frame that is the user's own code: a record of origin
//!    [`FunctionOrigin::Source`], or a macro-generated one (`#[derive(Clone)]`) when the PC is in
//!    it directly. A PC inside std, a std generic codegen'd in the user's crate, or drop glue is
//!    attributed to the user call site that led there. When no frame qualifies, the innermost
//!    frame with any record decides;
//! 2. collects that frame's events at the line-table row (file and line, narrowed by column
//!    when DWARF gives one, and by the inline chain); at a caller frame only the call
//!    terminator's events count;
//! 3. picks one with the machine effect: the source-level places the engine resolved for the
//!    read and written bytes, and their sizes against the events' type sizes.
//!
//! Confidence: [`Confidence::Exact`] when one event matched (StorageDead and Projection events,
//! which write no memory, are not counted against an observed write), [`Confidence::Inferred`] when the
//! machine effect (or, at a call site, the call itself) picked one among several,
//! [`Confidence::Heuristic`] when a move or copy is reported with no machine copy observed
//! (a rename, as in optimised code).

use hopper_core::SourceLoc;
use hopper_core::api::{Confidence, OwnershipKind, OwnershipLabel};

use crate::model::{FunctionOrigin, InlineFrame, MirPosition, OwnershipEvent, files_match};
use crate::sidecar::{EventRef, Sidecar};

/// One level of DWARF inlining: `function` was inlined at `call_site`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DwarfInline {
    /// The inlined function's name (its `DW_AT_abstract_origin` name, linkage or qualified).
    pub function: String,
    pub call_site: SourceLoc,
}

/// One physical frame as DWARF describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    /// The physical function's symbol: mangled linkage name, demangled name or def path.
    pub function: String,
    /// The inline chain at this PC, outermost first (empty when nothing is inlined).
    pub inline_chain: Vec<DwarfInline>,
    /// The line-table row: for the PC's own frame, the row of the PC; for a caller, its call
    /// site.
    pub location: Option<SourceLoc>,
}

impl FrameInfo {
    pub fn new(function: impl Into<String>, location: Option<SourceLoc>) -> FrameInfo {
        FrameInfo {
            function: function.into(),
            inline_chain: Vec::new(),
            location,
        }
    }
}

/// Bytes an instruction (or a `memcpy` the engine treats as one step) read or wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub size: u64,
    /// The source-level place whose DWARF location holds these bytes, when the engine resolved
    /// one (`b`, `c.name`, `n`).
    pub place: Option<String>,
}

impl Access {
    pub fn sized(size: u64) -> Access {
        Access { size, place: None }
    }

    pub fn of(place: impl Into<String>, size: u64) -> Access {
        Access {
            size,
            place: Some(place.into()),
        }
    }
}

/// What the machine did at the PC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MachineEffect {
    pub read: Option<Access>,
    pub written: Option<Access>,
}

/// Everything the join needs to know about one PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcInfo {
    pub frame: FrameInfo,
    /// The calling frames, innermost first; each one's `location` is its call site.
    pub callers: Vec<FrameInfo>,
    /// The observed machine effect; `None` when no machine copy was observed for the value
    /// (the move or copy, if any, is a rename).
    pub effect: Option<MachineEffect>,
}

/// How the join chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Chosen {
        event: EventRef,
        label: OwnershipLabel,
    },
    /// Several candidates, and nothing observed picks one.
    Ambiguous,
    /// The deciding frame has no event at this location.
    NoCandidates,
}

/// The join's answer for one PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Join {
    /// The frame whose events were used: 0 is the PC's own frame, `n` its `n`th caller.
    /// `None` when no frame has a sidecar record.
    pub frame: Option<usize>,
    /// The candidate events at that frame's location, in MIR order.
    pub candidates: Vec<EventRef>,
    pub choice: Choice,
}

impl Join {
    pub fn label(&self) -> Option<&OwnershipLabel> {
        match &self.choice {
            Choice::Chosen { label, .. } => Some(label),
            _ => None,
        }
    }

    pub fn event(&self) -> Option<EventRef> {
        match &self.choice {
            Choice::Chosen { event, .. } => Some(*event),
            _ => None,
        }
    }

    /// Whether the engine may prune a backward search on the chosen event (it has no unsafe or
    /// interior-mutability caveat). `false` when nothing was chosen.
    pub fn prunable(&self, sidecar: &Sidecar) -> bool {
        self.event().is_some_and(|e| sidecar.event(e).prunable())
    }
}

impl Sidecar {
    /// Join one PC to its ownership event.
    pub fn join(&self, pc: &PcInfo) -> Join {
        let frames = std::iter::once(&pc.frame).chain(&pc.callers);
        // The innermost frame with a record, as a fallback when no frame is the user's own.
        let mut fallback = None;
        for (depth, frame) in frames.enumerate() {
            let at_call_site = depth > 0;
            let Some((function, candidates)) = self.frame_candidates(frame, at_call_site) else {
                continue;
            };
            let decides = match self.function(function).origin {
                FunctionOrigin::Source => true,
                // A PC directly in a derive's code is labelled there; as a caller it is part of
                // the call that reached it.
                FunctionOrigin::MacroExpansion => !at_call_site,
                FunctionOrigin::Shim | FunctionOrigin::Dependency => false,
            };
            if decides {
                return self.decide(depth, candidates, pc);
            }
            if fallback.is_none() {
                fallback = Some((depth, candidates));
            }
        }
        match fallback {
            Some((depth, candidates)) => self.decide(depth, candidates, pc),
            None => Join {
                frame: None,
                candidates: Vec::new(),
                choice: Choice::NoCandidates,
            },
        }
    }

    fn decide(&self, depth: usize, candidates: Vec<EventRef>, pc: &PcInfo) -> Join {
        let choice = choose(self, &candidates, pc.effect.as_ref(), depth > 0);
        Join {
            frame: Some(depth),
            candidates,
            choice,
        }
    }

    /// The function record a frame resolves to and its candidate events at the frame's
    /// location. `None` when the sidecar has no record for the frame.
    pub(crate) fn frame_candidates(
        &self,
        frame: &FrameInfo,
        at_call_site: bool,
    ) -> Option<(usize, Vec<EventRef>)> {
        let physical = self.function_named(&frame.function);
        if let Some(fi) = physical {
            let found = self.located(
                fi,
                &frame.inline_chain,
                frame.location.as_ref(),
                at_call_site,
            );
            if !found.is_empty() || frame.inline_chain.is_empty() {
                return Some((fi, found));
            }
        }
        // LLVM inlined the innermost function after the sidecar was written: its events are in
        // its own record, with an empty MIR inline chain.
        if let Some(inner) = frame.inline_chain.last()
            && let Some(fi) = self.function_named(&inner.function)
        {
            return Some((
                fi,
                self.located(fi, &[], frame.location.as_ref(), at_call_site),
            ));
        }
        physical.map(|fi| (fi, Vec::new()))
    }

    fn located(
        &self,
        function: usize,
        chain: &[DwarfInline],
        loc: Option<&SourceLoc>,
        at_call_site: bool,
    ) -> Vec<EventRef> {
        let Some(loc) = loc else { return Vec::new() };
        let mut found: Vec<EventRef> = self
            .function_events_at_line(function, &loc.file, loc.line)
            .into_iter()
            .filter(|r| {
                let e = self.event(*r);
                chain_matches(&e.inline_chain, chain)
                    && (!at_call_site || e.location.position == MirPosition::Terminator)
            })
            .collect();
        if let Some(col) = loc.column {
            let exact: Vec<EventRef> = found
                .iter()
                .copied()
                .filter(|r| self.event(*r).span.lo.col == col)
                .collect();
            if !exact.is_empty() {
                found = exact;
            }
        }
        if at_call_site {
            // One call site is one terminator: keep the first terminator's events (MIR order),
            // which is the call the line-table row names when columns did not separate them.
            if let Some(first) = found.first().map(|r| self.event(*r).location) {
                found.retain(|r| self.event(*r).location == first);
            }
        }
        found
    }
}

/// A sidecar chain and a DWARF chain agree: same length, each callee's name matches, each
/// call site is on the same line of the same file.
fn chain_matches(sidecar: &[InlineFrame], dwarf: &[DwarfInline]) -> bool {
    sidecar.len() == dwarf.len()
        && sidecar.iter().zip(dwarf).all(|(s, d)| {
            names_match(&s.callee, &d.function)
                && files_match(&s.call_site.file, &d.call_site.file)
                && s.call_site.lo.line == d.call_site.line
        })
}

/// Loose function-name equality: the sidecar has def paths with generic arguments, DWARF has
/// linkage names or qualified names.
pub(crate) fn names_match(def_path: &str, dwarf: &str) -> bool {
    let a = format!("{:#}", rustc_demangle::demangle(def_path));
    let b = format!("{:#}", rustc_demangle::demangle(dwarf));
    let strip = |s: &str| -> String {
        let mut out = String::new();
        let mut depth = 0;
        for ch in s.chars() {
            match ch {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ if depth == 0 => out.push(ch),
                _ => {}
            }
        }
        while out.contains("::::") {
            out = out.replace("::::", "::");
        }
        out.trim_end_matches("::").to_owned()
    };
    let (a, b) = (strip(&a), strip(&b));
    // std re-exports: `std::vec::Vec::push` is `alloc::vec::Vec::push`, so the last two path
    // segments decide when the crate prefixes differ. Call-site lines guard the rest.
    let tail = |s: &str| -> Option<String> {
        let segs: Vec<&str> = s.rsplit("::").take(2).collect();
        (segs.len() == 2).then(|| format!("{}::{}", segs[1], segs[0]))
    };
    a == b
        || a.ends_with(&format!("::{b}"))
        || b.ends_with(&format!("::{a}"))
        || tail(&a).is_some_and(|t| tail(&b).as_ref() == Some(&t))
}

/// `sub` names `base` or a part of it (`b.name`, `v[0]`, `*b` deref aside).
pub(crate) fn place_within(sub: &str, base: &str) -> bool {
    let sub = sub.trim_start_matches('*');
    let base = base.trim_start_matches('*');
    sub == base
        || sub
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

/// Two places overlap when one is within the other.
fn places_overlap(a: &str, b: &str) -> bool {
    place_within(a, b) || place_within(b, a)
}

/// How well an event explains an observed effect: `None` when it contradicts it, otherwise the
/// number of agreeing facts.
fn effect_score(e: &OwnershipEvent, effect: &MachineEffect) -> Option<u32> {
    let mut score = 0;
    if let Some(w) = &effect.written {
        if let Some(p) = &w.place {
            match &e.dst {
                Some(dst) if places_overlap(dst, p) => score += 2,
                _ => return None,
            }
        }
        if e.ty.as_ref().and_then(|t| t.size) == Some(w.size) {
            score += 1;
        }
    }
    if let Some(r) = &effect.read
        && let Some(p) = &r.place
    {
        match (&e.src, e.kind) {
            (Some(src), _) if places_overlap(src, p) => score += 2,
            // A literal or a call reads nothing the engine can name as a place.
            (_, OwnershipKind::Literal | OwnershipKind::CallReturn | OwnershipKind::Clone) => {}
            _ => return None,
        }
    }
    Some(score)
}

fn is_call_event(kind: OwnershipKind) -> bool {
    matches!(
        kind,
        OwnershipKind::CallReturn | OwnershipKind::Clone | OwnershipKind::Drop
    )
}

fn choose(
    sc: &Sidecar,
    candidates: &[EventRef],
    effect: Option<&MachineEffect>,
    at_call_site: bool,
) -> Choice {
    let chosen = |r: EventRef, confidence: Confidence| {
        let e = sc.event(r);
        Choice::Chosen {
            event: r,
            label: OwnershipLabel {
                kind: e.kind,
                src: e.src.clone(),
                dst: e.dst.clone(),
                span: Some(e.span.start()),
                confidence,
            },
        }
    };
    let kind_of = |r: &EventRef| sc.event(*r).kind;
    let unique = |pred: &dyn Fn(&EventRef) -> bool| -> Option<EventRef> {
        let mut it = candidates.iter().copied().filter(|r| pred(r));
        match (it.next(), it.next()) {
            (Some(one), None) => Some(one),
            _ => None,
        }
    };

    // StorageDead emits no instructions and a Projection only computes an address inside its
    // borrow's statement, so neither explains an observed write.
    let effectful: Vec<EventRef> = match effect {
        Some(MachineEffect {
            written: Some(_), ..
        }) => candidates
            .iter()
            .copied()
            .filter(|r| {
                !matches!(
                    kind_of(r),
                    OwnershipKind::StorageDead | OwnershipKind::Projection
                )
            })
            .collect(),
        _ => candidates.to_vec(),
    };

    match effect {
        None => {
            // No machine copy: a move or copy here is a rename.
            if let [one] = candidates {
                let confidence = match kind_of(one) {
                    OwnershipKind::Move | OwnershipKind::Copy => Confidence::Heuristic,
                    _ => Confidence::Exact,
                };
                return chosen(*one, confidence);
            }
            if let Some(r) = unique(&|r| kind_of(r) == OwnershipKind::Move)
                .or_else(|| unique(&|r| kind_of(r) == OwnershipKind::Copy))
            {
                return chosen(r, Confidence::Heuristic);
            }
            if at_call_site && let Some(r) = unique(&|r| is_call_event(kind_of(r))) {
                return chosen(r, Confidence::Inferred);
            }
            if candidates.is_empty() {
                Choice::NoCandidates
            } else {
                Choice::Ambiguous
            }
        }
        Some(effect) => {
            if effectful.is_empty() {
                return Choice::NoCandidates;
            }
            let scored: Vec<(EventRef, u32)> = effectful
                .iter()
                .filter_map(|r| effect_score(sc.event(*r), effect).map(|s| (*r, s)))
                .collect();
            if let [only] = effectful.as_slice()
                && scored.len() == 1
            {
                return chosen(*only, Confidence::Exact);
            }
            let best = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
            let top: Vec<EventRef> = scored
                .iter()
                .filter(|(_, s)| *s == best && best > 0)
                .map(|(r, _)| *r)
                .collect();
            if let [one] = top.as_slice() {
                return chosen(*one, Confidence::Inferred);
            }
            if at_call_site {
                // The bytes were produced inside the callee: the call itself explains them.
                let pool: &[EventRef] = if top.is_empty() { &effectful } else { &top };
                let mut calls = pool.iter().copied().filter(|r| is_call_event(kind_of(r)));
                if let (Some(r), None) = (calls.next(), calls.next()) {
                    return chosen(r, Confidence::Inferred);
                }
            }
            Choice::Ambiguous
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_overlap_is_component_wise() {
        assert!(places_overlap("b", "b"));
        assert!(places_overlap("b.name", "b"));
        assert!(places_overlap("v", "v[0].name"));
        assert!(places_overlap("*self", "self.name"));
        assert!(!places_overlap("bb", "b"));
        assert!(!places_overlap("b.name", "c"));
    }

    #[test]
    fn function_names_match_loosely() {
        assert!(names_match(
            "std::vec::Vec::<Config>::push",
            "alloc::vec::Vec<T,A>::push"
        ));
        assert!(names_match("worked_example::load", "load"));
        assert!(names_match(
            "worked_example::load",
            "_ZN14worked_example4load17h0123456789abcdefE"
        ));
        assert!(!names_match("worked_example::load", "worked_example::main"));
    }
}
