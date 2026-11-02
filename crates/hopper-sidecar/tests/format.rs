//! The `.hsc` format: round trips, corruption, build-id checks, partial linking, indexes.

mod common;

use common::{FILE, elf_with_build_id, ev, func, span, with_callee};
use hopper_core::HopperError;
use hopper_core::api::OwnershipKind as K;
use hopper_sidecar::link::{PartialSet, link, partial_file_name, write_partial};
use hopper_sidecar::{
    Binding, BuildId, Caveat, ControlPath, CrateUnit, FunctionOrigin, Header, InlineFrame, LineCol,
    Sidecar, SidecarData, Span, TypeInfo, UnitOutput, build_id_of, codec, sidecar_path,
};

fn header(binding: Binding) -> Header {
    Header {
        binding,
        rustc_version: "rustc 1.97.0-nightly (f53b654a8 2026-04-30)".into(),
        crates: vec!["worked_example".into()],
        tool: "hopper-build 0.1.0".into(),
    }
}

fn rich_data() -> SidecarData {
    let mut inlined = ev(
        K::Move,
        1920,
        41,
        4,
        Some(10),
        Some("src"),
        Some("*end"),
        Some(("Config", 32)),
    );
    inlined.span.file = "/rustc/library/core/src/ptr/mod.rs".into();
    inlined.inline_chain = vec![InlineFrame {
        callee: "std::ptr::write::<Config>".into(),
        call_site: span(1045, 13, 40),
    }];
    inlined.caveats = vec![Caveat::Unsafe, Caveat::InteriorMutability];
    inlined.path = ControlPath::Unwind;
    let mut sized_unknown = ev(K::Drop, 19, 1, 9, None, Some("v"), None, None);
    sized_unknown.ty = Some(TypeInfo {
        name: "[u8]".into(),
        size: None,
    });
    sized_unknown.path = ControlPath::DropFlagGuarded;
    let multi_line = hopper_sidecar::OwnershipEvent {
        span: Span {
            file: FILE.into(),
            lo: LineCol { line: 3, col: 10 },
            hi: LineCol { line: 5, col: 2 },
        },
        ..ev(
            K::Aggregate,
            3,
            10,
            1,
            Some(2),
            None,
            Some("return"),
            Some(("Config", 32)),
        )
    };
    SidecarData {
        header: header(Binding::Binary {
            build_id: BuildId(vec![0xbc, 0xc7, 0x46, 0x62]),
        }),
        functions: vec![
            func(
                "_RNvCs7102KwbUtvs_14worked_example4main",
                "worked_example::main",
                FunctionOrigin::Source,
                11,
                vec![
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
                        "<Config as std::clone::Clone>::clone",
                    ),
                    sized_unknown,
                    multi_line,
                ],
            ),
            func(
                "_RNvMsF_push",
                "std::vec::Vec::<Config>::push",
                FunctionOrigin::Dependency,
                1003,
                vec![inlined],
            ),
            func(
                "_RINvdrop",
                "std::ptr::drop_in_place::<Config>",
                FunctionOrigin::Shim,
                809,
                vec![],
            ),
            func(
                "_RNvXclone",
                "<Config as std::clone::Clone>::clone",
                FunctionOrigin::MacroExpansion,
                3,
                vec![],
            ),
        ],
    }
}

#[test]
fn round_trip_is_lossless() {
    let data = rich_data();
    let bytes = codec::encode(&data).unwrap();
    assert_eq!(&bytes[..8], &codec::MAGIC);
    assert_eq!(
        u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        codec::FORMAT_VERSION
    );
    assert_eq!(codec::decode(&bytes).unwrap(), data);
    assert_eq!(codec::decode_header_only(&bytes).unwrap(), data.header);
}

#[test]
fn partial_headers_round_trip() {
    for output in [
        UnitOutput::Library,
        UnitOutput::Linked {
            path: "/t/debug/deps/worked_example-1234".into(),
            upstream: vec![1, u64::MAX, 7],
        },
    ] {
        let data = SidecarData {
            header: header(Binding::Partial(CrateUnit {
                name: "worked_example".into(),
                stable_crate_id: 0x51b8a900ad25aa84,
                output,
            })),
            functions: Vec::new(),
        };
        assert_eq!(codec::decode(&codec::encode(&data).unwrap()).unwrap(), data);
    }
}

#[test]
fn strings_are_interned() {
    let mut data = rich_data();
    let one = codec::encode(&data).unwrap().len();
    let f = data.functions[0].clone();
    for _ in 0..200 {
        data.functions.push(f.clone());
    }
    let many = codec::encode(&data).unwrap().len();
    // 200 more copies of a 4-event function cost far less than 200 times its first copy.
    assert!(many < one * 4, "one {one} many {many}");
}

#[test]
fn corruption_is_detected() {
    let bytes = codec::encode(&rich_data()).unwrap();
    // Truncated compressed body.
    assert!(codec::decode(&bytes[..bytes.len() - 5]).is_err());
    // A flipped byte inside the compressed frame fails zstd's checksum or the parse.
    let mut flipped = bytes.clone();
    let mid = 12 + (bytes.len() - 12) / 2;
    flipped[mid] ^= 0x55;
    assert!(codec::decode(&flipped).is_err());
    // Empty input.
    assert!(matches!(codec::decode(&[]), Err(HopperError::Invalid(_))));
}

#[test]
fn build_id_of_an_elf() {
    let elf = elf_with_build_id(&[
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ]);
    assert_eq!(
        build_id_of(&elf).unwrap().to_hex(),
        "0102030405060708090a0b0c0d0e0f1011121314"
    );
    assert!(build_id_of(b"not an object file").is_err());
}

#[test]
fn open_for_binary_checks_the_build_id() {
    let dir = tempfile::tempdir().unwrap();
    let id = vec![0xbc, 0xc7, 0x46, 0x62];
    let bin = dir.path().join("worked-example");
    std::fs::write(&bin, elf_with_build_id(&id)).unwrap();
    let sc_path = sidecar_path(&bin);
    let sc = Sidecar::new(rich_data());
    sc.write(&sc_path).unwrap();
    assert!(!dir.path().join("worked-example.hsc.tmp").exists());

    let loaded = Sidecar::open_for_binary(&sc_path, &bin).unwrap();
    assert_eq!(loaded.data(), sc.data());

    // Rebuilt binary: different build-id.
    std::fs::write(&bin, elf_with_build_id(&[9, 9, 9, 9])).unwrap();
    match Sidecar::open_for_binary(&sc_path, &bin) {
        Err(HopperError::StaleSidecar(msg)) => assert!(
            msg.contains("bcc74662") && msg.contains("09090909"),
            "{msg}"
        ),
        other => panic!("expected StaleSidecar, got {other:?}"),
    }

    // A partial is never a binary's sidecar.
    let partial = Sidecar::new(SidecarData {
        header: header(Binding::Partial(CrateUnit {
            name: "x".into(),
            stable_crate_id: 1,
            output: UnitOutput::Library,
        })),
        functions: Vec::new(),
    });
    assert!(matches!(
        partial.check_build_id(&BuildId(id)),
        Err(HopperError::StaleSidecar(_))
    ));
}

fn partial(
    name: &str,
    id: u64,
    output: UnitOutput,
    functions: Vec<hopper_sidecar::FunctionRecord>,
) -> SidecarData {
    SidecarData {
        header: Header {
            crates: vec![name.into()],
            ..header(Binding::Partial(CrateUnit {
                name: name.into(),
                stable_crate_id: id,
                output,
            }))
        },
        functions,
    }
}

#[test]
fn link_merges_linked_crates_only() {
    let shared = func(
        "_Rshared",
        "lib::shared::<u8>",
        FunctionOrigin::Dependency,
        1,
        vec![],
    );
    let exe = partial(
        "app",
        10,
        UnitOutput::Linked {
            path: "/t/app".into(),
            upstream: vec![20, 99],
        },
        vec![
            func("_Rmain", "app::main", FunctionOrigin::Source, 1, vec![]),
            shared.clone(),
        ],
    );
    let lib = partial(
        "lib",
        20,
        UnitOutput::Library,
        vec![
            func("_Rlib", "lib::f", FunctionOrigin::Source, 1, vec![]),
            shared,
        ],
    );
    let other = partial(
        "other",
        30,
        UnitOutput::Library,
        vec![func(
            "_Rother",
            "other::g",
            FunctionOrigin::Source,
            1,
            vec![],
        )],
    );
    let linked = link(
        &exe,
        &[lib.clone(), other],
        BuildId(vec![1]),
        "hopper-build test",
    )
    .unwrap();
    assert_eq!(
        linked.header.binding,
        Binding::Binary {
            build_id: BuildId(vec![1])
        }
    );
    assert_eq!(linked.header.crates, ["app", "lib"]);
    assert_eq!(linked.header.tool, "hopper-build test");
    let symbols: Vec<&str> = linked.functions.iter().map(|f| f.symbol.as_str()).collect();
    assert_eq!(symbols, ["_Rlib", "_Rmain", "_Rshared"]);

    // A library is not an executable.
    assert!(link(&lib, &[], BuildId(vec![1]), "t").is_err());

    // Mixed compilers are refused.
    let mut foreign = lib;
    foreign.header.rustc_version = "rustc 1.80.0".into();
    assert!(link(&exe, &[foreign], BuildId(vec![1]), "t").is_err());
}

#[test]
fn partial_set_links_a_hard_linked_binary() {
    let dir = tempfile::tempdir().unwrap();
    let deps = dir.path().join("deps");
    std::fs::create_dir_all(&deps).unwrap();
    let real = deps.join("app-0123");
    std::fs::write(&real, elf_with_build_id(&[7; 20])).unwrap();
    let uplifted = dir.path().join("app");
    std::fs::hard_link(&real, &uplifted).unwrap();

    let partials = dir.path().join("partials");
    let exe = partial(
        "app",
        10,
        UnitOutput::Linked {
            path: real.to_string_lossy().into_owned(),
            upstream: vec![20],
        },
        vec![func(
            "_Rmain",
            "app::main",
            FunctionOrigin::Source,
            1,
            vec![],
        )],
    );
    let lib = partial(
        "lib",
        20,
        UnitOutput::Library,
        vec![func("_Rlib", "lib::f", FunctionOrigin::Source, 1, vec![])],
    );
    let p = write_partial(&partials, &exe).unwrap();
    assert_eq!(
        p.file_name().unwrap().to_str().unwrap(),
        "app-000000000000000a.hsc"
    );
    write_partial(&partials, &lib).unwrap();
    // A non-sidecar file in the directory is ignored.
    std::fs::write(partials.join("junk.hsc"), b"junk").unwrap();

    let set = PartialSet::load_dir(&partials).unwrap();
    assert_eq!(set.partials.len(), 2);
    let written = set
        .link_binary(&uplifted, "hopper-build test")
        .unwrap()
        .unwrap();
    assert_eq!(written, sidecar_path(&uplifted));
    let sc = Sidecar::open_for_binary(&written, &uplifted).unwrap();
    assert_eq!(sc.functions().len(), 2);

    // A binary no partial produced has no sidecar.
    let stranger = dir.path().join("stranger");
    std::fs::write(&stranger, elf_with_build_id(&[8; 20])).unwrap();
    assert_eq!(set.link_binary(&stranger, "t").unwrap(), None);
    // A missing partials directory is an empty set.
    assert!(
        PartialSet::load_dir(&dir.path().join("none"))
            .unwrap()
            .partials
            .is_empty()
    );
    assert_eq!(
        partial_file_name(&CrateUnit {
            name: "lib".into(),
            stable_crate_id: 20,
            output: UnitOutput::Library
        }),
        "lib-0000000000000014.hsc"
    );
}

#[test]
fn indexes_by_name_and_line() {
    let sc = Sidecar::new(rich_data());
    let main = sc
        .function_named("_RNvCs7102KwbUtvs_14worked_example4main")
        .unwrap();
    assert_eq!(sc.function_named("worked_example::main"), Some(main));
    assert_eq!(sc.function(main).def_path, "worked_example::main");
    assert_eq!(sc.function_named("no::such"), None);

    let at13 = sc.events_at_line("main.rs", 13);
    assert_eq!(at13.len(), 1);
    assert_eq!(sc.event(at13[0]).kind, K::Move);
    assert_eq!(sc.events_at_line("src/main.rs", 14).len(), 1);
    assert!(sc.events_at_line("lib.rs", 13).is_empty());
    assert!(sc.events_at_line("xmain.rs", 13).is_empty());
    // A multi-line span is indexed by its first line.
    assert_eq!(sc.events_at_line(FILE, 3).len(), 1);
    assert!(sc.events_at_line(FILE, 4).is_empty());
}
