//! The source view of a dataflow chain: one user-level ownership event per transfer the program's
//! own code makes.
//!
//! The join ([`Sidecar::join`]) labels one PC with the MIR event its instruction carries out. A
//! chain of machine hops through `v.push(c)` then reads "`c` → `_8`" (the argument temporary),
//! "`_8` → `push(arg 1)`" and stores inside `Vec::push`, none of which is what the source says.
//! The functions here lift those labels to the user's level:
//!
//! - [`Sidecar::user_frame`]: the innermost frame that is the user's own code (std, derives and
//!   shims are part of the call that reached them);
//! - [`Sidecar::call_transfer`]: what a call at a user call site did with the bytes, from where
//!   they are once the call returns (the *landing*): a clone (`c` ← clone of `b`), the call's
//!   return value (`name` ← `String::from(..)`), or, when the callee's write lands in memory
//!   owned by the receiver, the move of the by-value argument into it (`c` → `v[0]` for
//!   `v.push(c)`);
//! - [`Sidecar::returned_value`]: a store into a user function's return place is the caller's
//!   call (`a` ← `load()`), with the callee's events underneath it (the aggregate
//!   `Config { name, .. }` and `name` moved in).
//!
//! Confidence follows the join's: [`Confidence::Exact`] when the landing place picks exactly one
//! event of the call, [`Confidence::Inferred`] when only the call's shape did.

use hopper_core::api::{Confidence, OwnershipKind, OwnershipLabel};

use crate::join::{PcInfo, names_match, place_within};
use crate::model::{FunctionOrigin, OwnershipEvent};
use crate::sidecar::{EventRef, Sidecar};

/// The user's own frame a PC belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UserFrame {
    /// 0 is the PC's own physical frame, `n` its `n`th caller ([`PcInfo::callers`]).
    pub depth: usize,
    /// The frame's function record.
    pub function: usize,
}

/// A user-level event and the events underneath it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEvent {
    pub label: OwnershipLabel,
    /// Outermost first: how a returned value was built, a call's literal arguments.
    pub inner: Vec<OwnershipLabel>,
}

fn label_of(e: &OwnershipEvent, confidence: Confidence) -> OwnershipLabel {
    OwnershipLabel {
        kind: e.kind,
        src: e.src.clone(),
        dst: e.dst.clone(),
        span: Some(e.span.start()),
        confidence,
    }
}

/// The argument index of a call-argument destination (`Vec::<Config>::push(arg 1)` → 1).
fn argument_index(dst: &str) -> Option<usize> {
    let rest = dst.strip_suffix(')')?;
    let (_, n) = rest.rsplit_once("(arg ")?;
    n.parse().ok()
}

/// The place a receiver argument borrows (`&mut v`, `&v` → `v`).
fn borrowed_place(src: &str) -> &str {
    src.strip_prefix("&mut ")
        .or_else(|| src.strip_prefix('&'))
        .unwrap_or(src)
}

/// A place is the return place or a part of it (`return`, `return.name`).
pub fn is_return_place(place: &str) -> bool {
    place_within(place, "return")
}

/// A MIR temporary (`_8`): a compiler-made place the source never names.
pub fn is_temporary(place: &str) -> bool {
    place
        .strip_prefix('_')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A type name without its path and generic arguments (`worked_example::Config` → `Config`).
fn short_type(name: &str) -> &str {
    let base = name.split('<').next().unwrap_or(name);
    base.rsplit("::").next().unwrap_or(base)
}

impl Sidecar {
    /// The innermost frame of `pc` whose function is the user's own source (a record of origin
    /// [`FunctionOrigin::Source`]). A derive's code, a std generic and a shim belong to the user
    /// call that reached them.
    pub fn user_frame(&self, pc: &PcInfo) -> Option<UserFrame> {
        std::iter::once(&pc.frame)
            .chain(&pc.callers)
            .enumerate()
            .find_map(|(depth, frame)| {
                let function = self.function_named(&frame.function).or_else(|| {
                    frame
                        .inline_chain
                        .last()
                        .and_then(|i| self.function_named(&i.function))
                })?;
                (self.function(function).origin == FunctionOrigin::Source)
                    .then_some(UserFrame { depth, function })
            })
    }

    /// The events of the call terminator the user frame at `depth` is stopped in.
    fn call_events(&self, pc: &PcInfo, depth: usize) -> Vec<EventRef> {
        let frame = match depth {
            0 => &pc.frame,
            n => match pc.callers.get(n - 1) {
                Some(f) => f,
                None => return Vec::new(),
            },
        };
        self.frame_candidates(frame, true)
            .map(|(_, c)| c)
            .unwrap_or_default()
    }

    /// What the call at user frame `depth` did with the bytes a callee wrote, given where they
    /// are once the call returns: `landing`, a place of that frame (`name`, `c`, `v[0]`), when
    /// the engine found one.
    ///
    /// - A clone call is the clone, whatever its callees wrote.
    /// - Bytes inside the call's destination are its return value.
    /// - Bytes in memory owned by the receiver (argument 0, `&mut v`) are the receiver taking the
    ///   by-value argument: `v.push(c)` moves `c` into `v[0]`.
    ///
    /// `None` when the call's events do not explain the landing.
    pub fn call_transfer(
        &self,
        pc: &PcInfo,
        depth: usize,
        landing: Option<&str>,
    ) -> Option<SourceEvent> {
        let events: Vec<&OwnershipEvent> = self
            .call_events(pc, depth)
            .into_iter()
            .map(|r| self.event(r))
            .collect();
        if let Some(clone) = events.iter().find(|e| e.kind == OwnershipKind::Clone) {
            return Some(SourceEvent {
                label: label_of(clone, Confidence::Exact),
                inner: Vec::new(),
            });
        }
        let landing = landing?;
        let arguments: Vec<(usize, &OwnershipEvent)> = events
            .iter()
            .filter_map(|e| Some((argument_index(e.dst.as_deref()?)?, *e)))
            .collect();
        let literals = || -> Vec<OwnershipLabel> {
            arguments
                .iter()
                .filter(|(_, e)| e.kind == OwnershipKind::Literal)
                .map(|(_, e)| label_of(e, Confidence::Exact))
                .collect()
        };
        if let Some(ret) = events.iter().find(|e| {
            e.kind == OwnershipKind::CallReturn
                && e.dst.as_deref().is_some_and(|d| place_within(landing, d))
        }) {
            return Some(SourceEvent {
                label: label_of(ret, Confidence::Exact),
                inner: literals(),
            });
        }
        let receiver = arguments
            .iter()
            .find(|(i, _)| *i == 0)
            .and_then(|(_, e)| e.src.as_deref())
            .map(borrowed_place)?;
        if !place_within(landing, receiver) || landing == receiver {
            return None;
        }
        let by_value: Vec<&OwnershipEvent> = arguments
            .iter()
            .filter(|(i, e)| *i > 0 && matches!(e.kind, OwnershipKind::Move | OwnershipKind::Copy))
            .map(|(_, e)| *e)
            .collect();
        let (taken, confidence) = match by_value.as_slice() {
            [one] => (*one, Confidence::Exact),
            [first, ..] => (*first, Confidence::Inferred),
            [] => return None,
        };
        Some(SourceEvent {
            label: OwnershipLabel {
                kind: taken.kind,
                src: taken.src.clone(),
                dst: Some(landing.to_owned()),
                span: Some(taken.span.start()),
                confidence,
            },
            inner: Vec::new(),
        })
    }

    /// A store into the return place of the user function `callee` (the join chose `chosen`,
    /// with label `chosen_label`, there) is the caller's call: `a` ← `load()`. `landing` is the
    /// caller's place that holds the bytes after the return, when the engine found one.
    ///
    /// Underneath: the aggregate that built the return place, rendered with the field the bytes
    /// are in (`Config { name, .. }`), then the chosen event itself (`name` → `return.name`).
    /// `None` when no user frame calls `callee` or the caller's call site has no call event for
    /// it.
    pub fn returned_value(
        &self,
        pc: &PcInfo,
        callee: UserFrame,
        chosen: Option<EventRef>,
        chosen_label: &OwnershipLabel,
        landing: Option<&str>,
    ) -> Option<SourceEvent> {
        let caller = self.user_frame(&PcInfo {
            frame: pc.callers.get(callee.depth)?.clone(),
            callers: pc.callers[callee.depth + 1..].to_vec(),
            effect: None,
        })?;
        let depth = callee.depth + 1 + caller.depth;
        let record = self.function(callee.function);
        let events: Vec<&OwnershipEvent> = self
            .call_events(pc, depth)
            .into_iter()
            .map(|r| self.event(r))
            .collect();
        let call = events
            .iter()
            .filter(|e| matches!(e.kind, OwnershipKind::CallReturn | OwnershipKind::Clone))
            .find(|e| {
                e.callee.as_deref().is_none_or(|c| {
                    names_match(c, &record.def_path) || names_match(c, &record.demangled)
                })
            })?;
        let confidence = match (landing, call.dst.as_deref()) {
            (Some(l), Some(d)) if place_within(l, d) => Confidence::Exact,
            _ => Confidence::Inferred,
        };
        let mut inner = Vec::new();
        if let Some(r) = chosen
            && let Some(aggregate) = self.aggregate_building(r)
        {
            inner.push(aggregate);
        }
        inner.push(chosen_label.clone());
        Some(SourceEvent {
            label: label_of(call, confidence),
            inner,
        })
    }

    /// The aggregate that `r` is a field operand of (`Config { name, retries: 3 }` → `return`,
    /// for `name` → `return.name`), with its source rendered as the type and the field `r`
    /// fills; the other fields elided (`Config { name, .. }`).
    fn aggregate_building(&self, r: EventRef) -> Option<OwnershipLabel> {
        let f = self.function(r.function);
        let field_event = &f.events[r.event];
        let dst = field_event.dst.as_deref()?;
        let (base, field) = dst.rsplit_once('.')?;
        // The aggregate precedes its field operands at the same MIR location.
        let aggregate = f.events[..r.event].iter().rev().find(|e| {
            e.kind == OwnershipKind::Aggregate
                && e.location == field_event.location
                && e.dst.as_deref() == Some(base)
        })?;
        let fields = f.events[..]
            .iter()
            .filter(|e| {
                e.location == aggregate.location
                    && e.kind != OwnershipKind::Aggregate
                    && e.dst
                        .as_deref()
                        .and_then(|d| d.strip_prefix(base))
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .count();
        let ty = aggregate
            .ty
            .as_ref()
            .map(|t| short_type(&t.name).to_owned())
            .unwrap_or_default();
        let value = match field_event.src.as_deref() {
            Some(s) if s == field => field.to_owned(),
            Some(s) => format!("{field}: {s}"),
            None => field.to_owned(),
        };
        let rest = if fields > 1 { ", .." } else { "" };
        Some(OwnershipLabel {
            kind: OwnershipKind::Aggregate,
            src: Some(format!("{ty} {{ {value}{rest} }}")),
            dst: aggregate.dst.clone(),
            span: Some(aggregate.span.start()),
            confidence: Confidence::Exact,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_indices_and_receivers() {
        assert_eq!(argument_index("Vec::<Config>::push(arg 1)"), Some(1));
        assert_eq!(argument_index("<Config as Clone>::clone(arg 0)"), Some(0));
        assert_eq!(argument_index("v[0]"), None);
        assert_eq!(borrowed_place("&mut v"), "v");
        assert_eq!(borrowed_place("&v"), "v");
        assert!(is_temporary("_8"));
        assert!(!is_temporary("_"));
        assert!(!is_temporary("x_8"));
        assert!(is_return_place("return.name"));
        assert!(!is_return_place("returned"));
        assert_eq!(short_type("worked_example::Config"), "Config");
        assert_eq!(short_type("Vec<u8>"), "Vec");
    }
}
