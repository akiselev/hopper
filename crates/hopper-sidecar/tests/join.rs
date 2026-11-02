//! The join and the local-state API, on a hand-written sidecar shaped like the worked example's
//! (`fixtures/worked-example`; hopper-build's own test checks the same against a real build).

mod common;

use common::{FILE, ev, func, span, with_callee};
use hopper_core::SourceLoc;
use hopper_core::api::{Confidence, OwnershipKind as K, OwnershipState};
use hopper_sidecar::{
    Access, Binding, BuildId, Caveat, Choice, ControlPath, DwarfInline, FrameInfo, FunctionOrigin,
    Header, InlineFrame, LocalTransition, MachineEffect, PcInfo, Sidecar, SidecarData,
    ownership_state,
};

const STD_VEC: &str = "/rustc/library/alloc/src/vec/mod.rs";
const STD_SLICE: &str = "/rustc/library/alloc/src/slice.rs";
const STD_STRING: &str = "/rustc/library/alloc/src/string.rs";

fn at(line: u32, col: u32) -> Option<SourceLoc> {
    Some(SourceLoc {
        file: "/w/worked-example/src/main.rs".into(),
        line,
        column: Some(col),
    })
}

fn std_at(file: &str, line: u32, col: u32) -> Option<SourceLoc> {
    Some(SourceLoc {
        file: file.into(),
        line,
        column: Some(col),
    })
}

fn in_std(mut e: hopper_sidecar::OwnershipEvent, file: &str) -> hopper_sidecar::OwnershipEvent {
    e.span.file = file.into();
    e.caveats = vec![Caveat::Unsafe];
    e
}

fn worked_example() -> Sidecar {
    let push = "std::vec::Vec::<Config>::push";
    let clone = "<Config as std::clone::Clone>::clone";
    let from = "<std::string::String as std::convert::From<&str>>::from";
    let index = "<std::vec::Vec<Config> as std::ops::Index<usize>>::index";
    let mut unwind_drop_c = ev(
        K::Drop,
        19,
        1,
        15,
        None,
        Some("c"),
        None,
        Some(("Config", 32)),
    );
    unwind_drop_c.path = ControlPath::Unwind;
    let main = vec![
        ev(
            K::Literal,
            12,
            9,
            0,
            Some(0),
            Some("false"),
            Some("_24"),
            Some(("bool", 1)),
        ),
        with_callee(
            ev(
                K::CallReturn,
                12,
                13,
                0,
                None,
                Some("load(..)"),
                Some("a"),
                Some(("Config", 32)),
            ),
            "worked_example::load",
        ),
        ev(
            K::Move,
            13,
            13,
            1,
            Some(0),
            Some("a"),
            Some("b"),
            Some(("Config", 32)),
        ),
        ev(
            K::SharedBorrow,
            14,
            13,
            1,
            Some(1),
            Some("b"),
            Some("&b"),
            Some(("&Config", 8)),
        ),
        with_callee(
            ev(
                K::Move,
                14,
                15,
                1,
                None,
                Some("&b"),
                Some("<Config as Clone>::clone(arg 0)"),
                Some(("&Config", 8)),
            ),
            clone,
        ),
        with_callee(
            ev(
                K::Clone,
                14,
                15,
                1,
                None,
                Some("b"),
                Some("c"),
                Some(("Config", 32)),
            ),
            clone,
        ),
        ev(
            K::MutBorrow,
            16,
            5,
            3,
            Some(0),
            Some("v"),
            Some("&v"),
            Some(("&mut Vec<Config>", 8)),
        ),
        ev(
            K::Literal,
            16,
            12,
            3,
            Some(1),
            Some("false"),
            Some("_24"),
            Some(("bool", 1)),
        ),
        ev(
            K::Move,
            16,
            12,
            3,
            Some(2),
            Some("c"),
            Some("_8"),
            Some(("Config", 32)),
        ),
        with_callee(
            ev(
                K::Move,
                16,
                7,
                3,
                None,
                Some("&v"),
                Some("Vec::<Config>::push(arg 0)"),
                Some(("&mut Vec<Config>", 8)),
            ),
            push,
        ),
        with_callee(
            ev(
                K::Move,
                16,
                7,
                3,
                None,
                Some("c"),
                Some("Vec::<Config>::push(arg 1)"),
                Some(("Config", 32)),
            ),
            push,
        ),
        with_callee(
            ev(
                K::CallReturn,
                16,
                7,
                3,
                None,
                Some("Vec::<Config>::push(..)"),
                Some("_6"),
                Some(("()", 0)),
            ),
            push,
        ),
        ev(
            K::SharedBorrow,
            17,
            14,
            4,
            Some(0),
            Some("v"),
            Some("&v"),
            Some(("&Vec<Config>", 8)),
        ),
        with_callee(
            ev(
                K::Literal,
                17,
                15,
                4,
                None,
                Some("0"),
                Some("index(arg 1)"),
                Some(("usize", 8)),
            ),
            index,
        ),
        with_callee(
            ev(
                K::CallReturn,
                17,
                15,
                4,
                None,
                Some("index(..)"),
                Some("&v[0]"),
                Some(("&Config", 8)),
            ),
            index,
        ),
        ev(
            K::Projection,
            17,
            13,
            5,
            Some(0),
            Some("v[0]"),
            Some("v[0].name"),
            Some(("String", 24)),
        ),
        ev(
            K::SharedBorrow,
            17,
            13,
            5,
            Some(0),
            Some("v[0].name"),
            Some("n"),
            Some(("&String", 8)),
        ),
        ev(
            K::Drop,
            19,
            1,
            9,
            None,
            Some("v"),
            None,
            Some(("Vec<Config>", 24)),
        ),
        ev(
            K::Drop,
            19,
            1,
            10,
            None,
            Some("b"),
            None,
            Some(("Config", 32)),
        ),
        unwind_drop_c,
    ];
    let load = vec![
        with_callee(
            ev(
                K::Literal,
                7,
                16,
                0,
                None,
                Some("\"alpha\""),
                Some("String::from(arg 0)"),
                Some(("&str", 16)),
            ),
            from,
        ),
        with_callee(
            ev(
                K::CallReturn,
                7,
                16,
                0,
                None,
                Some("String::from(..)"),
                Some("name"),
                Some(("String", 24)),
            ),
            from,
        ),
        ev(
            K::Aggregate,
            8,
            5,
            1,
            Some(0),
            None,
            Some("return"),
            Some(("Config", 32)),
        ),
        ev(
            K::Move,
            8,
            5,
            1,
            Some(0),
            Some("name"),
            Some("return.name"),
            Some(("String", 24)),
        ),
        ev(
            K::Literal,
            8,
            5,
            1,
            Some(0),
            Some("3"),
            Some("return.retries"),
            Some(("u32", 4)),
        ),
    ];
    let derive_clone = vec![
        with_callee(
            ev(
                K::Clone,
                4,
                17,
                0,
                None,
                Some("self.name"),
                Some("_2"),
                Some(("String", 24)),
            ),
            "<std::string::String as std::clone::Clone>::clone",
        ),
        ev(
            K::Aggregate,
            3,
            10,
            1,
            Some(2),
            None,
            Some("return"),
            Some(("Config", 32)),
        ),
    ];
    let push_mut = vec![in_std(
        ev(
            K::Move,
            1920,
            41,
            4,
            Some(10),
            Some("src"),
            Some("*end"),
            Some(("Config", 32)),
        ),
        STD_VEC,
    )];
    let push_fn = vec![in_std(
        with_callee(
            ev(
                K::Move,
                1004,
                17,
                0,
                None,
                Some("value"),
                Some("push_mut(arg 1)"),
                Some(("Config", 32)),
            ),
            "push_mut",
        ),
        STD_VEC,
    )];
    let to_vec = vec![in_std(
        ev(
            K::Copy,
            454,
            36,
            4,
            Some(1),
            Some("&*s"),
            Some("&*s"),
            Some(("*const [u8]", 16)),
        ),
        STD_SLICE,
    )];
    let string_from = vec![in_std(
        with_callee(
            ev(
                K::CallReturn,
                2950,
                9,
                0,
                None,
                Some("to_owned(..)"),
                Some("return"),
                Some(("String", 24)),
            ),
            "to_owned",
        ),
        STD_STRING,
    )];
    let functions = vec![
        func(
            "_RNvCs_main",
            "worked_example::main",
            FunctionOrigin::Source,
            11,
            main,
        ),
        func(
            "_RNvCs_load",
            "worked_example::load",
            FunctionOrigin::Source,
            6,
            load,
        ),
        func(
            "_RNvXs_clone",
            "<worked_example::Config as core::clone::Clone>::clone",
            FunctionOrigin::MacroExpansion,
            3,
            derive_clone,
        ),
        func(
            "_RNvMs_push",
            "<alloc::vec::Vec<worked_example::Config>>::push",
            FunctionOrigin::Dependency,
            1003,
            push_fn,
        ),
        func(
            "_RNvMs_push_mut",
            "<alloc::vec::Vec<worked_example::Config>>::push_mut",
            FunctionOrigin::Dependency,
            1035,
            push_mut,
        ),
        func(
            "_RNvXs_to_vec",
            "<u8 as alloc::slice::ConvertVec>::to_vec::<alloc::alloc::Global>",
            FunctionOrigin::Dependency,
            446,
            to_vec,
        ),
        func(
            "_RNvXs_from",
            "<alloc::string::String as core::convert::From<&str>>::from",
            FunctionOrigin::Dependency,
            2948,
            string_from,
        ),
    ];
    Sidecar::new(SidecarData {
        header: Header {
            binding: Binding::Binary {
                build_id: BuildId(vec![1]),
            },
            rustc_version: "rustc 1.97.0-nightly".into(),
            crates: vec!["worked_example".into()],
            tool: "test".into(),
        },
        functions,
    })
}

fn wrote(place: Option<&str>, size: u64) -> Option<Access> {
    Some(Access {
        size,
        place: place.map(str::to_owned),
    })
}

fn effect(read: Option<Access>, written: Option<Access>) -> Option<MachineEffect> {
    Some(MachineEffect { read, written })
}

fn pc(frame: FrameInfo, callers: Vec<FrameInfo>, effect: Option<MachineEffect>) -> PcInfo {
    PcInfo {
        frame,
        callers,
        effect,
    }
}

/// (kind, src, dst, line, confidence) of the chosen label.
fn summary(sc: &Sidecar, info: &PcInfo) -> (K, Option<String>, Option<String>, u32, Confidence) {
    let join = sc.join(info);
    let label = join
        .label()
        .unwrap_or_else(|| panic!("no label: {join:?}"))
        .clone();
    (
        label.kind,
        label.src,
        label.dst,
        label.span.unwrap().line,
        label.confidence,
    )
}

fn s(x: &str) -> Option<String> {
    Some(x.to_owned())
}

#[test]
fn six_hops_of_the_worked_example() {
    let sc = worked_example();

    // Hop 1: the store of `n`'s pointer at line 17: a shared borrow of `v[0].name`. The
    // projection on the same statement writes nothing, so the borrow is the only candidate.
    let hop1 = pc(
        FrameInfo::new("_RNvCs_main", at(17, 13)),
        vec![],
        effect(None, wrote(Some("n"), 8)),
    );
    assert_eq!(
        summary(&sc, &hop1),
        (
            K::SharedBorrow,
            s("v[0].name"),
            s("n"),
            17,
            Confidence::Exact
        )
    );

    // Hop 2: a memcpy inside `Vec::push` (std) into the heap: attributed to the call at line 16,
    // where the 32-byte size picks the move of `c` out of the call's three events.
    let hop2 = pc(
        FrameInfo::new("_RNvMs_push_mut", std_at(STD_VEC, 1920, 41)),
        vec![
            FrameInfo::new("_RNvMs_push", std_at(STD_VEC, 1004, 17)),
            FrameInfo::new("_RNvCs_main", at(16, 7)),
        ],
        effect(wrote(None, 32), wrote(None, 32)),
    );
    let j2 = sc.join(&hop2);
    assert_eq!(j2.frame, Some(2));
    assert_eq!(j2.candidates.len(), 3);
    assert_eq!(
        summary(&sc, &hop2),
        (
            K::Move,
            s("c"),
            s("Vec::<Config>::push(arg 1)"),
            16,
            Confidence::Inferred
        )
    );

    // Hop 3: the 5 heap bytes written by `to_vec` inside `String::clone`, inside the derived
    // `Config::clone` (a macro expansion, part of its caller): the clone call at line 14.
    let hop3 = pc(
        FrameInfo::new("_RNvXs_to_vec", std_at(STD_SLICE, 454, 36)),
        vec![
            FrameInfo::new(
                "<alloc::string::String as core::clone::Clone>::clone",
                std_at(STD_STRING, 2120, 9),
            ),
            FrameInfo::new("_RNvXs_clone", at(4, 17)),
            FrameInfo::new("_RNvCs_main", at(14, 15)),
        ],
        effect(wrote(None, 5), wrote(None, 5)),
    );
    assert_eq!(sc.join(&hop3).frame, Some(3));
    assert_eq!(
        summary(&sc, &hop3),
        (K::Clone, s("b"), s("c"), 14, Confidence::Inferred)
    );

    // Hop 4: the stack copy at line 13: the move of `a` into `b`.
    let hop4 = pc(
        FrameInfo::new("worked_example::main", at(13, 13)),
        vec![],
        effect(wrote(Some("a"), 32), wrote(Some("b"), 32)),
    );
    assert_eq!(
        summary(&sc, &hop4),
        (K::Move, s("a"), s("b"), 13, Confidence::Exact)
    );

    // Hop 5: `a` is the return value of `load()` (line 12), built by the aggregate at line 8 with
    // `name` moved in.
    let hop5 = pc(
        FrameInfo::new("_RNvCs_main", at(12, 13)),
        vec![],
        effect(None, wrote(Some("a"), 32)),
    );
    assert_eq!(
        summary(&sc, &hop5),
        (K::CallReturn, s("load(..)"), s("a"), 12, Confidence::Exact)
    );
    let hop5_inside = pc(
        FrameInfo::new("_RNvCs_load", at(8, 5)),
        vec![FrameInfo::new("_RNvCs_main", at(12, 13))],
        effect(wrote(Some("name"), 24), wrote(None, 24)),
    );
    assert_eq!(
        summary(&sc, &hop5_inside),
        (
            K::Move,
            s("name"),
            s("return.name"),
            8,
            Confidence::Inferred
        )
    );

    // Hop 6: the memcpy from `.rodata` inside `String::from`: the call at line 7, whose return
    // value is `name`.
    let hop6 = pc(
        FrameInfo::new("_RNvXs_to_vec", std_at(STD_SLICE, 454, 36)),
        vec![
            FrameInfo::new("_RNvXs_from", std_at(STD_STRING, 2950, 9)),
            FrameInfo::new("_RNvCs_load", at(7, 16)),
        ],
        effect(wrote(None, 5), wrote(None, 5)),
    );
    assert_eq!(
        summary(&sc, &hop6),
        (
            K::CallReturn,
            s("String::from(..)"),
            s("name"),
            7,
            Confidence::Inferred
        )
    );
}

#[test]
fn a_move_without_a_machine_copy_is_a_heuristic_rename() {
    let sc = worked_example();
    let release = pc(FrameInfo::new("_RNvCs_main", at(13, 13)), vec![], None);
    assert_eq!(
        summary(&sc, &release),
        (K::Move, s("a"), s("b"), 13, Confidence::Heuristic)
    );
    // Line only (no column): line 16 has several moves, so no unique rename.
    let line_only = pc(
        FrameInfo::new("_RNvCs_main", Some(SourceLoc::new("main.rs", 16))),
        vec![],
        None,
    );
    assert_eq!(sc.join(&line_only).choice, Choice::Ambiguous);
}

#[test]
fn ambiguous_and_empty_answers() {
    let sc = worked_example();
    // Line 16 by line only, a 32-byte write: the statement move into `_8` and the argument move
    // tie on size.
    let tie = pc(
        FrameInfo::new("_RNvCs_main", Some(SourceLoc::new("main.rs", 16))),
        vec![],
        effect(None, wrote(None, 32)),
    );
    assert_eq!(sc.join(&tie).choice, Choice::Ambiguous);
    // A line with no events in a known function: the function decides, with nothing.
    let none = pc(
        FrameInfo::new("_RNvCs_main", at(15, 17)),
        vec![FrameInfo::new("x", at(1, 1))],
        None,
    );
    let j = sc.join(&none);
    assert_eq!((j.frame, j.choice), (Some(0), Choice::NoCandidates));
    // No frame known at all.
    let unknown = pc(FrameInfo::new("libc_memcpy", None), vec![], None);
    assert_eq!(sc.join(&unknown).frame, None);
    // A written place no candidate writes contradicts every candidate.
    let wrong = pc(
        FrameInfo::new("_RNvCs_main", at(13, 13)),
        vec![],
        effect(None, wrote(Some("zzz"), 32)),
    );
    assert_eq!(sc.join(&wrong).choice, Choice::Ambiguous);
}

#[test]
fn std_only_frames_fall_back_to_the_innermost_record() {
    let sc = worked_example();
    // No user frame: the dependency record decides.
    let only_std = pc(
        FrameInfo::new("_RNvMs_push_mut", std_at(STD_VEC, 1920, 41)),
        vec![],
        effect(None, wrote(Some("*end"), 32)),
    );
    let j = sc.join(&only_std);
    assert_eq!(j.frame, Some(0));
    let label = j.label().unwrap();
    assert_eq!((label.kind, label.dst.as_deref()), (K::Move, Some("*end")));
    assert!(!j.prunable(&sc), "std code is flagged unsafe");
    // A PC directly in the derive's own code is labelled there.
    let in_derive = pc(
        FrameInfo::new("_RNvXs_clone", at(3, 10)),
        vec![],
        effect(None, wrote(Some("return"), 32)),
    );
    assert_eq!(sc.join(&in_derive).label().unwrap().kind, K::Aggregate);
}

#[test]
fn inline_chains_select_events() {
    let mut data = worked_example().into_data();
    let main = &mut data.functions[0];
    let mut inlined = ev(
        K::Move,
        1920,
        41,
        20,
        Some(0),
        Some("src"),
        Some("*end"),
        Some(("Config", 32)),
    );
    inlined.span.file = "/rustc/library/core/src/ptr/mod.rs".into();
    inlined.inline_chain = vec![InlineFrame {
        callee: "std::ptr::write::<Config>".into(),
        call_site: span(16, 7, 16),
    }];
    main.events.push(inlined);
    let sc = Sidecar::new(data);

    // MIR inlining: the DWARF chain matches the event's chain.
    let frame = FrameInfo {
        function: "_RNvCs_main".into(),
        inline_chain: vec![DwarfInline {
            function: "core::ptr::write<worked_example::Config>".into(),
            call_site: SourceLoc::new(FILE, 16),
        }],
        location: std_at("/rustc/library/core/src/ptr/mod.rs", 1920, 41),
    };
    let j = sc.join(&pc(frame.clone(), vec![], effect(None, wrote(None, 32))));
    assert_eq!(j.label().unwrap().src.as_deref(), Some("src"));
    assert_eq!(j.label().unwrap().confidence, Confidence::Exact);

    // A different call-site line does not match.
    let mut other = frame;
    other.inline_chain[0].call_site.line = 99;
    assert_eq!(
        sc.join(&pc(other, vec![], effect(None, wrote(None, 32))))
            .choice,
        Choice::NoCandidates
    );

    // LLVM inlining (after MIR): the inlined function's own record answers.
    let llvm = FrameInfo {
        function: "_RNvCs_main".into(),
        inline_chain: vec![DwarfInline {
            function: "_RNvMs_push_mut".into(),
            call_site: SourceLoc::new(FILE, 16),
        }],
        location: std_at(STD_VEC, 1920, 41),
    };
    let j = sc.join(&pc(llvm, vec![], effect(None, wrote(None, 32))));
    assert_eq!(j.label().unwrap().dst.as_deref(), Some("*end"));
}

#[test]
fn local_state_follows_moves_and_drops() {
    let sc = worked_example();
    let main = sc.function_named("worked_example::main").unwrap();

    let a = sc.local_events(main, "a");
    let kinds: Vec<_> = a.iter().map(|e| e.transition.clone()).collect();
    assert_eq!(
        kinds,
        [
            LocalTransition::Initialised {
                from: s("load(..)")
            },
            LocalTransition::MovedOut { to: s("b") }
        ]
    );
    assert_eq!(ownership_state([]), OwnershipState::Live);
    assert_eq!(ownership_state([(&a[0], None)]), OwnershipState::Live);
    match ownership_state(a.iter().map(|e| (e, None))) {
        OwnershipState::MovedOut { to, at, moment } => {
            assert_eq!(to.as_deref(), Some("b"));
            assert_eq!(at.map(|l| (l.line, l.column)), Some((13, Some(13))));
            assert_eq!(moment, None);
        }
        other => panic!("{other:?}"),
    }

    // `c` moves twice at line 16 (into the temporary, then the argument) and its only drop is on
    // the unwind path, which is left out: it stays moved out.
    let c = sc.local_events(main, "c");
    assert!(c.iter().all(|e| e.path != ControlPath::Unwind));
    assert!(matches!(
        ownership_state(c.iter().map(|e| (e, None))),
        OwnershipState::MovedOut { .. }
    ));

    // `b` is dropped at line 19.
    let b = sc.local_events(main, "b");
    match ownership_state(b.iter().map(|e| (e, None))) {
        OwnershipState::Dropped { at, .. } => assert_eq!(at.unwrap().line, 19),
        other => panic!("{other:?}"),
    }

    // Partial moves leave the local live.
    let load = sc.function_named("worked_example::load").unwrap();
    let ret = sc.local_events(load, "return");
    assert!(
        ret.iter()
            .any(|e| matches!(e.transition, LocalTransition::Initialised { .. }))
    );
    assert_eq!(
        ownership_state(ret.iter().map(|e| (e, None))),
        OwnershipState::Live
    );
}

#[test]
fn drop_flag_guarded_drops_skip_moved_values() {
    let mut data = worked_example().into_data();
    let mut guarded = ev(
        K::Drop,
        19,
        1,
        11,
        None,
        Some("a"),
        None,
        Some(("Config", 32)),
    );
    guarded.path = ControlPath::DropFlagGuarded;
    data.functions[0].events.push(guarded);
    let sc = Sidecar::new(data);
    let main = sc.function_named("worked_example::main").unwrap();
    let a = sc.local_events(main, "a");
    assert_eq!(a.len(), 3);
    // Moved, then the guarded drop: the flag was clear, so still moved out.
    assert!(matches!(
        ownership_state(a.iter().map(|e| (e, None))),
        OwnershipState::MovedOut { .. }
    ));
    // Initialised, then the guarded drop (never moved): dropped.
    assert!(matches!(
        ownership_state([(&a[0], None), (&a[2], None)]),
        OwnershipState::Dropped { .. }
    ));
}

#[test]
fn caveats_turn_off_pruning() {
    let mut data = worked_example().into_data();
    data.functions[0].events[2].caveats = vec![Caveat::InteriorMutability];
    let sc = Sidecar::new(data);
    let j = sc.join(&pc(
        FrameInfo::new("_RNvCs_main", at(13, 13)),
        vec![],
        effect(None, wrote(Some("b"), 32)),
    ));
    assert_eq!(j.label().unwrap().kind, K::Move);
    assert!(!j.prunable(&sc));
    let j = sc.join(&pc(
        FrameInfo::new("_RNvCs_main", at(12, 13)),
        vec![],
        effect(None, wrote(Some("a"), 32)),
    ));
    assert!(j.prunable(&sc));
}

/// (kind, src, dst, line, confidence) of a label.
fn of(l: &hopper_core::api::OwnershipLabel) -> (K, Option<String>, Option<String>, u32, Confidence) {
    (
        l.kind,
        l.src.clone(),
        l.dst.clone(),
        l.span.as_ref().unwrap().line,
        l.confidence,
    )
}

/// The user-level events of the worked example's chain: what each call did with the bytes, from
/// where they are once it returns, and a returned value as the caller's call.
#[test]
fn user_level_events_of_the_worked_example() {
    let sc = worked_example();

    // The store inside `Vec::push` lands in `v[0]`, memory the receiver `v` owns: `c` moved in.
    let push = pc(
        FrameInfo::new("_RNvMs_push_mut", std_at(STD_VEC, 1920, 41)),
        vec![
            FrameInfo::new("_RNvMs_push", std_at(STD_VEC, 1004, 17)),
            FrameInfo::new("_RNvCs_main", at(16, 7)),
        ],
        effect(wrote(None, 8), wrote(None, 8)),
    );
    let user = sc.user_frame(&push).expect("main is the user frame");
    assert_eq!(user.depth, 2);
    let moved = sc.call_transfer(&push, user.depth, Some("v[0]")).unwrap();
    assert_eq!(
        of(&moved.label),
        (K::Move, s("c"), s("v[0]"), 16, Confidence::Exact)
    );
    // Landing in the call's destination, or nowhere the call explains.
    assert_eq!(sc.call_transfer(&push, 2, Some("v")), None);
    assert_eq!(sc.call_transfer(&push, 2, Some("b")), None);
    assert_eq!(sc.call_transfer(&push, 2, None), None);

    // Inside the derived clone (a macro expansion): the user frame is main, and the call is the
    // clone of `b` into `c` wherever its bytes landed.
    let clone = pc(
        FrameInfo::new("_RNvXs_to_vec", std_at(STD_SLICE, 454, 36)),
        vec![
            FrameInfo::new(
                "<alloc::string::String as core::clone::Clone>::clone",
                std_at(STD_STRING, 2120, 9),
            ),
            FrameInfo::new("_RNvXs_clone", at(4, 17)),
            FrameInfo::new("_RNvCs_main", at(14, 15)),
        ],
        effect(wrote(None, 5), wrote(None, 5)),
    );
    let user = sc.user_frame(&clone).unwrap();
    assert_eq!(user.depth, 3);
    let cloned = sc.call_transfer(&clone, 3, None).unwrap();
    assert_eq!(
        of(&cloned.label),
        (K::Clone, s("b"), s("c"), 14, Confidence::Exact)
    );

    // String::from's copy lands in `name`'s buffer: the call's return value, with its literal
    // argument underneath.
    let from = pc(
        FrameInfo::new("_RNvXs_to_vec", std_at(STD_SLICE, 454, 36)),
        vec![
            FrameInfo::new("_RNvXs_from", std_at(STD_STRING, 2950, 9)),
            FrameInfo::new("_RNvCs_load", at(7, 16)),
        ],
        effect(wrote(None, 5), wrote(None, 5)),
    );
    let user = sc.user_frame(&from).unwrap();
    assert_eq!(user.depth, 2);
    let built = sc.call_transfer(&from, 2, Some("name.vec[0]")).unwrap();
    assert_eq!(
        of(&built.label),
        (
            K::CallReturn,
            s("String::from(..)"),
            s("name"),
            7,
            Confidence::Exact
        )
    );
    assert_eq!(
        built.inner.iter().map(of).collect::<Vec<_>>(),
        vec![(
            K::Literal,
            s("\"alpha\""),
            s("String::from(arg 0)"),
            7,
            Confidence::Exact
        )]
    );

    // `load`'s store of `name` into its return place is main's `a` ← `load()`, built by the
    // aggregate with `name` moved in.
    let ret = pc(
        FrameInfo::new("_RNvCs_load", at(8, 5)),
        vec![FrameInfo::new("_RNvCs_main", at(12, 13))],
        effect(wrote(Some("name"), 24), wrote(Some("return.name"), 24)),
    );
    let join = sc.join(&ret);
    let callee = sc.user_frame(&ret).unwrap();
    assert_eq!(callee.depth, 0);
    let value = sc
        .returned_value(&ret, callee, join.event(), join.label().unwrap(), Some("a"))
        .unwrap();
    assert_eq!(
        of(&value.label),
        (K::CallReturn, s("load(..)"), s("a"), 12, Confidence::Exact)
    );
    let inner: Vec<_> = value.inner.iter().map(of).collect();
    assert_eq!(
        inner,
        vec![
            (
                K::Aggregate,
                s("Config { name, .. }"),
                s("return"),
                8,
                Confidence::Exact
            ),
            // The join's own grade: the written place picked the move among the aggregate's
            // field operands.
            (K::Move, s("name"), s("return.name"), 8, Confidence::Inferred),
        ]
    );
    // Without a landing the caller's call is still the only one that returns load's value.
    let unlanded = sc
        .returned_value(&ret, callee, join.event(), join.label().unwrap(), None)
        .unwrap();
    assert_eq!(unlanded.label.confidence, Confidence::Inferred);
}
