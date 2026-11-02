//! Ownership states for locals, from the sidecar and the lines a frame has executed.
//!
//! The sidecar says which events change a local ([`Sidecar::local_events`]); the engine supplies
//! which of their lines have run in the frame instance before the moment shown. Those come from
//! (in order of preference) the index, line hits observed by the cursor (on a live process every
//! sidecar line is a tracepoint), or an on-demand replay of the frame on a background replica
//! (rr). Without any of them the answer is [`OwnershipState::Unknown`].
//!
//! A line counts as executed when it was hit before the moment and the frame has since moved to
//! another line: the events of the line the frame is on now may not have run yet, so they are
//! left out (the conservative answer).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hopper_core::api::OwnershipState;
use hopper_core::{Moment, Tid};
use hopper_sidecar::{ControlPath, FunctionOrigin, Sidecar, ownership_state};
use hopper_symbols::SymbolSet;

/// What a tracepoint marks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TraceMark {
    /// The function's body begins (its post-prologue address): a new frame instance.
    Entry,
    /// A line that carries ownership events.
    Line(u32),
}

/// A breakpoint address that records a hit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct TracePoint {
    /// The sidecar function record.
    pub function: usize,
    pub mark: TraceMark,
}

/// One execution of a tracepoint.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LineHit {
    pub at: Moment,
    pub tid: Tid,
    /// The frame's CFA when it was hit.
    pub cfa: u64,
    pub function: usize,
    pub mark: TraceMark,
}

/// The tracepoints for every user function of the sidecar: each function's entry, and the first
/// address of each line that carries an event, inside that function.
pub fn tracepoints(sidecar: &Sidecar, syms: &SymbolSet) -> HashMap<u64, TracePoint> {
    let mut out = HashMap::new();
    for (fi, f) in sidecar.functions().iter().enumerate() {
        if f.origin != FunctionOrigin::Source {
            continue;
        }
        let instances: Vec<_> = syms
            .find_functions(&f.demangled)
            .into_iter()
            .filter(|i| i.linkage_name.as_deref().is_none_or(|l| l == f.symbol))
            .filter(|i| i.die().is_some())
            .collect();
        let lines: BTreeSet<(String, u32)> = f
            .events
            .iter()
            .filter(|e| e.path != ControlPath::Unwind)
            .map(|e| (e.span.file.clone(), e.span.lo.line))
            .collect();
        for inst in &instances {
            out.insert(
                syms.breakpoint_address(inst),
                TracePoint {
                    function: fi,
                    mark: TraceMark::Entry,
                },
            );
            for (file, line) in &lines {
                for a in syms.addresses_for_line(file, *line) {
                    if a.location.line == *line && inst.contains(a.address) {
                        out.entry(a.address).or_insert(TracePoint {
                            function: fi,
                            mark: TraceMark::Line(*line),
                        });
                    }
                }
            }
        }
    }
    out
}

/// The hits of one frame instance before `at`, from a log of hits in execution order: those
/// with the frame's thread, CFA and function after the frame's latest entry.
pub fn frame_hits(
    log: &[LineHit],
    at: &Moment,
    tid: Tid,
    cfa: u64,
    function: usize,
) -> Vec<LineHit> {
    let mine: Vec<LineHit> = log
        .iter()
        .filter(|h| h.tid == tid && h.cfa == cfa && h.function == function && h.at.is_before(at))
        .copied()
        .collect();
    match mine.iter().rposition(|h| h.mark == TraceMark::Entry) {
        Some(i) => mine[i..].to_vec(),
        None => mine,
    }
}

/// The ownership state of `local` in sidecar function `function`, given the frame instance's
/// hits before the moment (in execution order) and the line the frame is on now.
pub fn local_state(
    sidecar: &Sidecar,
    function: usize,
    local: &str,
    hits: &[LineHit],
    current_line: Option<u32>,
) -> OwnershipState {
    let mut lines: Vec<(u32, Moment)> = hits
        .iter()
        .filter_map(|h| match h.mark {
            TraceMark::Line(l) => Some((l, h.at)),
            TraceMark::Entry => None,
        })
        .collect();
    if lines.last().map(|(l, _)| Some(*l)) == Some(current_line) {
        lines.pop();
    }
    // The latest execution of each line, by its position in the hit order.
    let mut last: BTreeMap<u32, (usize, Moment)> = BTreeMap::new();
    for (i, (l, m)) in lines.iter().enumerate() {
        last.insert(*l, (i, *m));
    }
    let events = sidecar.local_events(function, local);
    let mut executed: Vec<(usize, usize, Moment, &hopper_sidecar::LocalEvent)> = events
        .iter()
        .filter_map(|e| {
            last.get(&e.span.lo.line)
                .map(|(i, m)| (*i, e.event.event, *m, e))
        })
        .collect();
    executed.sort_by_key(|(i, ev, _, _)| (*i, *ev));
    ownership_state(executed.into_iter().map(|(_, _, m, e)| (e, Some(m))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::RecordingId;
    use hopper_core::api::{OwnershipKind as K, OwnershipState};
    use hopper_sidecar::{
        Binding, BuildId, FunctionRecord, Header, LineCol, MirLocation, MirPosition,
        OwnershipEvent, SidecarData, Span,
    };

    fn span(line: u32, col: u32) -> Span {
        Span {
            file: "src/main.rs".into(),
            lo: LineCol { line, col },
            hi: LineCol { line, col: col + 1 },
        }
    }

    fn ev(kind: K, line: u32, src: Option<&str>, dst: Option<&str>, block: u32) -> OwnershipEvent {
        OwnershipEvent {
            kind,
            span: span(line, 9),
            inline_chain: Vec::new(),
            location: MirLocation {
                block,
                position: MirPosition::Statement(0),
            },
            path: ControlPath::Normal,
            src: src.map(Into::into),
            dst: dst.map(Into::into),
            ty: None,
            callee: None,
            caveats: Vec::new(),
        }
    }

    fn sidecar() -> Sidecar {
        Sidecar::new(SidecarData {
            header: Header {
                binding: Binding::Binary {
                    build_id: BuildId(vec![1, 2]),
                },
                rustc_version: "test".into(),
                crates: vec![],
                tool: "test".into(),
            },
            functions: vec![FunctionRecord {
                symbol: "_ZN4main4mainE".into(),
                demangled: "main::main".into(),
                def_path: "main".into(),
                krate: "main".into(),
                def_span: span(11, 1),
                origin: FunctionOrigin::Source,
                events: vec![
                    ev(K::CallReturn, 12, Some("load()"), Some("a"), 0),
                    ev(K::Move, 13, Some("a"), Some("b"), 1),
                    ev(K::Drop, 19, Some("b"), None, 9),
                ],
            }],
        })
    }

    fn m(ft: u64) -> Moment {
        Moment {
            recording: RecordingId([3; 16]),
            frame_time: ft,
            tid: Tid(1),
            ticks: 0,
            sub: 0,
        }
    }

    fn hit(ft: u64, mark: TraceMark, cfa: u64) -> LineHit {
        LineHit {
            at: m(ft),
            tid: Tid(1),
            cfa,
            function: 0,
            mark,
        }
    }

    #[test]
    fn a_is_moved_out_once_line_13_has_run() {
        let sc = sidecar();
        let log = [
            hit(1, TraceMark::Entry, 0x100),
            hit(2, TraceMark::Line(12), 0x100),
            hit(3, TraceMark::Line(13), 0x100),
            hit(4, TraceMark::Line(14), 0x100),
        ];
        // At line 13 itself, the move has not run yet.
        let hits = frame_hits(&log, &m(3), Tid(1), 0x100, 0);
        assert_eq!(
            local_state(&sc, 0, "a", &hits, Some(12)),
            OwnershipState::Live
        );
        let hits = frame_hits(&log, &m(4), Tid(1), 0x100, 0);
        assert_eq!(hits.len(), 3);
        assert_eq!(
            local_state(&sc, 0, "a", &hits, Some(13)),
            OwnershipState::Live
        );
        let hits = frame_hits(&log, &m(5), Tid(1), 0x100, 0);
        match local_state(&sc, 0, "a", &hits, Some(14)) {
            OwnershipState::MovedOut { to, at, moment } => {
                assert_eq!(to.as_deref(), Some("b"));
                assert_eq!(at.map(|l| l.line), Some(13));
                assert_eq!(moment, Some(m(3)));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            local_state(&sc, 0, "b", &hits, Some(14)),
            OwnershipState::Live
        );
    }

    #[test]
    fn hits_of_an_earlier_instance_at_the_same_address_do_not_count() {
        let log = [
            hit(1, TraceMark::Entry, 0x100),
            hit(2, TraceMark::Line(13), 0x100),
            hit(3, TraceMark::Entry, 0x100),
            hit(4, TraceMark::Line(12), 0x100),
            hit(5, TraceMark::Line(12), 0x200),
        ];
        let hits = frame_hits(&log, &m(9), Tid(1), 0x100, 0);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].mark, TraceMark::Line(12));
    }
}
