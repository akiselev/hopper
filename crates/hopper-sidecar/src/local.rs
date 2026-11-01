//! Local variable state: which events initialise, move out of or drop a local, so the engine
//! can say whether a variable on screen still holds a live value.
//!
//! DWARF keeps a moved-from `String` in scope with a valid-looking location, so a debugger
//! shows it as live. The engine asks [`Sidecar::local_events`] for the events that change a
//! local's state, finds which of their lines have executed before the moment it shows (line
//! hits from the index), and folds them with [`ownership_state`].

use hopper_core::Moment;
use hopper_core::api::{OwnershipKind, OwnershipState};

use crate::model::{ControlPath, MirLocation, Span};
use crate::sidecar::{EventRef, Sidecar};

/// How an event changes a local's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalTransition {
    /// The local receives a whole new value (from a move, copy, call, aggregate, literal or
    /// borrow).
    Initialised { from: Option<String> },
    /// The whole value moved out.
    MovedOut { to: Option<String> },
    /// A part of the value (`place`, for example `a.name`) moved out; the rest stays live.
    PartlyMovedOut { place: String, to: Option<String> },
    /// The value was dropped.
    Dropped,
    /// A part of the value was dropped (drop elaboration after a partial move).
    PartlyDropped { place: String },
    /// The local's storage ended.
    StorageDead,
}

/// One event that changes a local's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEvent {
    pub transition: LocalTransition,
    pub path: ControlPath,
    pub span: Span,
    pub location: MirLocation,
    pub event: EventRef,
}

impl Sidecar {
    /// The events of function `function` that initialise, move out of, drop or end the storage
    /// of the local named `local`, in MIR order. Unwind-only events (cleanup blocks) are left
    /// out: they run only while a panic unwinds.
    ///
    /// Names are source names, so two shadowing `let x` bindings in one function share their
    /// events; the engine separates them by the DWARF variable's lexical scope.
    pub fn local_events(&self, function: usize, local: &str) -> Vec<LocalEvent> {
        let f = self.function(function);
        let mut out = Vec::new();
        for (i, e) in f.events.iter().enumerate() {
            if e.path == ControlPath::Unwind {
                continue;
            }
            let src = e.src.as_deref();
            let dst = e.dst.as_deref();
            let transition = match e.kind {
                OwnershipKind::Move => match src {
                    Some(s) if s == local => Some(LocalTransition::MovedOut { to: e.dst.clone() }),
                    Some(s) if is_part_of(s, local) => Some(LocalTransition::PartlyMovedOut {
                        place: s.to_owned(),
                        to: e.dst.clone(),
                    }),
                    _ => None,
                },
                OwnershipKind::Drop => match src {
                    Some(s) if s == local => Some(LocalTransition::Dropped),
                    Some(s) if is_part_of(s, local) => {
                        Some(LocalTransition::PartlyDropped { place: s.to_owned() })
                    }
                    _ => None,
                },
                OwnershipKind::StorageDead => {
                    (src == Some(local)).then_some(LocalTransition::StorageDead)
                }
                OwnershipKind::Projection => None,
                OwnershipKind::Copy
                | OwnershipKind::Clone
                | OwnershipKind::SharedBorrow
                | OwnershipKind::MutBorrow
                | OwnershipKind::Aggregate
                | OwnershipKind::Literal
                | OwnershipKind::CallReturn => None,
            };
            // Any kind that writes a whole new value into the local re-initialises it; a move
            // can both leave one local and initialise another.
            let init = match e.kind {
                OwnershipKind::Drop | OwnershipKind::StorageDead | OwnershipKind::Projection => None,
                _ => (dst == Some(local)).then(|| LocalTransition::Initialised { from: e.src.clone() }),
            };
            for t in [transition, init].into_iter().flatten() {
                out.push(LocalEvent {
                    transition: t,
                    path: e.path,
                    span: e.span.clone(),
                    location: e.location,
                    event: EventRef { function, event: i },
                });
            }
        }
        out
    }
}

/// `place` is a strict part of `local` (`a.name`, `a[3]`, `(*a).x` written `a.x`).
fn is_part_of(place: &str, local: &str) -> bool {
    place
        .strip_prefix(local)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
}

/// The state after the given events, which must be the local's events that have executed, in
/// execution order, each with the moment it executed when the engine knows it. With no events
/// the local has not been touched by anything the sidecar knows, which is [`OwnershipState::Live`]
/// once the variable is in scope.
pub fn ownership_state<'a>(executed: impl IntoIterator<Item = (&'a LocalEvent, Option<Moment>)>) -> OwnershipState {
    let mut state = OwnershipState::Live;
    for (ev, moment) in executed {
        state = match &ev.transition {
            LocalTransition::Initialised { .. } => OwnershipState::Live,
            LocalTransition::MovedOut { to } => {
                OwnershipState::MovedOut { to: to.clone(), at: Some(ev.span.start()), moment }
            }
            // A drop behind a drop flag runs only while the local still owns its value: after a
            // move out the flag is clear and the drop is skipped.
            LocalTransition::Dropped
                if ev.path == ControlPath::DropFlagGuarded
                    && matches!(state, OwnershipState::MovedOut { .. }) =>
            {
                state
            }
            LocalTransition::Dropped => OwnershipState::Dropped { at: Some(ev.span.start()), moment },
            // A moved-out local's storage ending does not make it "dropped": nothing ran.
            LocalTransition::StorageDead => match state {
                OwnershipState::MovedOut { .. } => state,
                _ => OwnershipState::Dropped { at: Some(ev.span.start()), moment },
            },
            LocalTransition::PartlyMovedOut { .. } | LocalTransition::PartlyDropped { .. } => state,
        };
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_of_is_component_wise() {
        assert!(is_part_of("a.name", "a"));
        assert!(is_part_of("a[0]", "a"));
        assert!(!is_part_of("ab", "a"));
        assert!(!is_part_of("a", "a"));
    }
}
