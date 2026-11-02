//! Ownership states on a live process built with `cargo hopper-build`: the sidecar's events plus
//! the lines the cursor's tracepoints saw.

mod support;

use hopper_core::api::{Debugger, OwnershipState, StepKind};
use hopper_core::target::Direction;
use hopper_engine::{Session, SessionOptions, SidecarPolicy, SidecarStatus};
use support::*;

#[test]
fn a_is_moved_out_at_line_14() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    let Some(d) = live_binary(&bin) else { return };
    assert!(
        matches!(d.sidecar_status(), SidecarStatus::Loaded(_)),
        "{:?}",
        d.sidecar_status()
    );
    let s13 = run_to_line(&d, "main.rs", 13);
    let locals = d.locals(s13.at, top(&s13)).unwrap();
    // At line 13 the move has not run: `a` is live.
    assert_eq!(by_name(&locals, "a").ownership, OwnershipState::Live);

    let s = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(line_of(&s), Some(14));
    let locals = d.locals(s.at, top(&s)).unwrap();
    match &by_name(&locals, "a").ownership {
        OwnershipState::MovedOut { to, at, moment } => {
            assert_eq!(to.as_deref(), Some("b"));
            assert_eq!(at.as_ref().map(|l| l.line), Some(13));
            let m = moment.expect("the move's moment");
            assert!(s13.at.is_before(&m) || s13.at == m);
            assert!(m.is_before(&s.at));
        }
        other => panic!("a at line 14: {other:?}"),
    }
    assert_eq!(by_name(&locals, "b").ownership, OwnershipState::Live);
    // DWARF still shows the stale bytes; the label is what tells them apart.
    assert_eq!(by_name(&locals, "a").summary, by_name(&locals, "b").summary);
    // An expression rooted at `a` carries its state too.
    let e = d.eval(s.at, top(&s), "a.retries").unwrap();
    assert!(matches!(e.ownership, OwnershipState::MovedOut { .. }));

    // At line 17, `c` has moved into the Vec.
    let s17 = run_to_line(&d, "main.rs", 17);
    let locals = d.locals(s17.at, top(&s17)).unwrap();
    assert!(
        matches!(&by_name(&locals, "c").ownership, OwnershipState::MovedOut { to: Some(_), .. }),
        "{:?}",
        by_name(&locals, "c").ownership
    );
    assert_eq!(by_name(&locals, "v").ownership, OwnershipState::Live);
    // The earlier moment still answers from the cache with its own state.
    let again = d.locals(s.at, top(&s)).unwrap();
    assert!(matches!(by_name(&again, "a").ownership, OwnershipState::MovedOut { .. }));
}

#[test]
fn without_a_sidecar_ownership_is_unknown() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    let options = SessionOptions {
        sidecar: SidecarPolicy::Off,
        ..SessionOptions::default()
    };
    let d = Session::live(&bin, &[], options).unwrap();
    assert_eq!(d.sidecar_status(), &SidecarStatus::Disabled);
    let s = run_to_line(&d, "main.rs", 14);
    let locals = d.locals(s.at, top(&s)).unwrap();
    assert_eq!(by_name(&locals, "a").ownership, OwnershipState::Unknown);
    // The plain fixture build has no sidecar beside it.
    let Some(plain) = live("worked-example") else { return };
    assert_eq!(plain.sidecar_status(), &SidecarStatus::Absent);
}
