//! hopper-symbols against gdb on the fixture programs: unwinding on a core, line and function
//! lookup against gdb's breakpoint addresses, and source locations.
//!
//! Every test skips (passing, with a message) when gdb or the fixtures are unavailable.

use hopper_core::Tid;
use hopper_symbols::corefile::CoreFile;
use hopper_symbols::fixtures::{core_at, fixture_binary, gdb_available, gdb_output};
use hopper_symbols::{Binary, BuildId, Module, SymbolSet, UnwindMethod, disasm};

fn static_symbols(name: &str) -> Option<(std::path::PathBuf, SymbolSet)> {
    let path = fixture_binary(name)?;
    let mut set = SymbolSet::new();
    set.add(Module::unrelocated(Binary::open(&path).expect("open fixture")));
    Some((path, set))
}

/// gdb's `Breakpoint 1 at 0x...` address for `break <location>` on the unstarted binary.
fn gdb_breakpoint_address(binary: &std::path::Path, location: &str) -> Option<u64> {
    let out = gdb_output(binary, None, &[&format!("break {location}")]);
    let rest = &out[out.find("Breakpoint 1 at 0x")? + "Breakpoint 1 at 0x".len()..];
    let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    u64::from_str_radix(&hex, 16).ok()
}

#[test]
fn build_id_matches_readelf_note() {
    let Some(path) = fixture_binary("values") else { return };
    let b = Binary::open(&path).unwrap();
    let id: &BuildId = b.build_id().expect("fixtures have build ids");
    assert_eq!(id.0.len(), 20);
    let Ok(out) = std::process::Command::new("readelf").arg("-n").arg(&path).output() else {
        eprintln!("skipping comparison: readelf missing");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&id.to_hex()), "readelf -n does not show {id}");
}

#[test]
fn line_14_resolves_to_gdbs_breakpoint_address() {
    if !gdb_available() {
        eprintln!("skipping: gdb is not installed");
        return;
    }
    let Some((path, set)) = static_symbols("worked-example") else { return };
    for line in [7u32, 8, 12, 13, 14, 15, 16, 17, 18] {
        let loc = format!("main.rs:{line}");
        let ours = set.addresses_for_line("main.rs", line);
        let gdb = gdb_breakpoint_address(&path, &loc).expect("gdb resolves the line");
        assert_eq!(ours.len(), 1, "{loc}: {ours:?}");
        assert_eq!(ours[0].address, gdb, "{loc}: ours {:#x}, gdb {gdb:#x}", ours[0].address);
        assert_eq!(ours[0].location.line, line);
        assert!(ours[0].location.file.ends_with("fixtures/worked-example/src/main.rs"));
    }
    // A longer suffix works; a non-matching component does not.
    assert_eq!(set.addresses_for_line("src/main.rs", 14).len(), 1);
    assert!(set.addresses_for_line("xmain.rs", 14).is_empty());
}

#[test]
fn function_breakpoints_match_gdb() {
    if !gdb_available() {
        eprintln!("skipping: gdb is not installed");
        return;
    }
    let Some((path, set)) = static_symbols("worked-example") else { return };
    for name in ["worked_example::load", "worked_example::main"] {
        let found = set.find_functions(name);
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        let ours = set.breakpoint_address(&found[0]);
        let gdb = gdb_breakpoint_address(&path, name).expect("gdb resolves the function");
        assert_eq!(ours, gdb, "{name}: ours {ours:#x}, gdb {gdb:#x}");
    }
    // `main` is the program's main subprogram (DW_AT_main_subprogram), not the C `main` shim
    // that gdb picks before the program runs.
    let main = set.find_functions("main");
    assert_eq!(main.len(), 1);
    assert_eq!(main[0].name, "worked_example::main");
    // A bare suffix finds every function of that name, std's included, as gdb's linespecs do.
    let loads = set.find_functions("load");
    assert!(loads.len() > 1);
    assert!(loads.iter().all(|f| f.name.ends_with("::load") || f.name.contains("::load<")));
    let load = loads.iter().find(|f| f.name == "worked_example::load").expect("ours is among them");
    assert!(loads.iter().all(|f| f.entry != 0), "no discarded (address 0) copies");
    assert_eq!(load.decl.as_ref().map(|d| d.line), Some(6));
    assert!(set.find_functions("no_such_function_anywhere").is_empty());
    // Symbol-table only names resolve too.
    assert!(!set.find_functions("std::rt::lang_start_internal").is_empty());
}

#[test]
fn pc_to_location_and_stepping_ranges() {
    let Some((_, set)) = static_symbols("worked-example") else { return };
    let addr = set.addresses_for_line("main.rs", 14)[0].address;
    let loc = set.location(addr).unwrap();
    assert_eq!(loc.line, 14);
    let frames = set.frames_at(addr);
    assert_eq!(frames[0].function.as_deref(), Some("worked_example::main"));
    assert!(!frames[0].inlined);
    let range = set.line_range_at(addr).unwrap();
    assert!(range.range.contains(&addr));
    assert_eq!(range.location.line, 14);
    // Every address in the range is line 14, and the first one past it is not.
    for a in range.range.clone() {
        assert_eq!(set.location(a).map(|l| l.line), Some(14), "{a:#x}");
    }
    assert_ne!(set.location(range.range.end).map(|l| l.line), Some(14));
    let f = set.function_at(addr).unwrap();
    assert_eq!(f.name, "worked_example::main");
    let bounds = set.function_bounds(addr).unwrap();
    assert!(bounds.iter().any(|r| r.contains(&addr)));
}

#[test]
fn inline_chain_in_main() {
    // `String::from` in `load` is inlined at -O0 through `#[inline]` std code only sometimes;
    // instead find any address in main with an inlined frame and check the chain is consistent.
    let Some((_, set)) = static_symbols("values") else { return };
    let main = &set.find_functions("values::main")[0];
    let bounds = main.bounds().unwrap();
    let mut seen_inline = false;
    for a in bounds.clone() {
        let frames = set.frames_at(a);
        if frames.len() > 1 {
            seen_inline = true;
            assert!(frames[..frames.len() - 1].iter().all(|f| f.inlined));
            assert!(!frames.last().unwrap().inlined);
            assert_eq!(frames.last().unwrap().function.as_deref(), Some("values::main"));
            // The outer frame's location is the call site in main.rs.
            let call = frames[frames.len() - 1].location.as_ref().unwrap();
            assert!(call.file.ends_with("values/src/main.rs"), "{call}");
            break;
        }
    }
    assert!(seen_inline, "values::main has inlined calls at -O0 (String::from et al.)");
}

#[test]
fn unwinds_core_at_stop_here() {
    let Some((binary, core_path)) = core_at("values", "values::stop_here") else { return };
    let core = CoreFile::open(&core_path).unwrap();
    let set = core.symbol_set();
    let (_, exe) = set.modules().next().unwrap();
    assert_eq!(exe.binary().path(), binary.as_path());
    let thread = &core.threads()[0];
    let mut mem = core.memory();
    let stack = set.stack(&mut mem, &thread.regs, thread.tid).unwrap();
    let names: Vec<String> =
        stack.iter().map(|f| f.frame.function.clone().unwrap_or_else(|| "??".into())).collect();
    assert_eq!(names[0], "values::stop_here", "{names:?}");
    assert_eq!(names[1], "values::main", "{names:?}");
    assert!(
        names[2..].iter().any(|n| n.contains("lang_start")),
        "std's lang_start frames above main: {names:?}"
    );
    assert_eq!(stack[0].frame.location.as_ref().unwrap().line, 25);
    assert_eq!(stack[1].frame.location.as_ref().unwrap().line, 58);
    assert!(stack.iter().enumerate().all(|(i, f)| f.frame.id.depth == i as u32));
    assert!(stack.iter().all(|f| f.frame.id.tid == Tid(thread.tid.0)));

    // gdb's backtrace on the same core: the same pcs for the frames gdb and hopper both see.
    let bt = gdb_output(&binary, Some(&core_path), &["set backtrace past-main on", "bt"]);
    let gdb_pcs: Vec<u64> = bt
        .lines()
        .filter_map(|l| {
            let i = l.find(" 0x")?;
            let hex: String = l[i + 3..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            u64::from_str_radix(&hex, 16).ok()
        })
        .collect();
    let ours_physical: Vec<u64> = {
        let frames = set.unwind(&mut mem, &thread.regs).unwrap();
        assert_eq!(frames[0].method, UnwindMethod::Initial);
        assert!(frames[1..].iter().all(|f| f.method == UnwindMethod::CallFrameInfo));
        frames.iter().map(|f| f.pc).collect()
    };
    assert!(gdb_pcs.len() >= 3, "gdb bt: {bt}");
    for pc in &gdb_pcs {
        assert!(ours_physical.contains(pc), "gdb frame pc {pc:#x} missing from {ours_physical:x?}\n{bt}");
    }
    // The CFA of stop_here is main's stack pointer.
    let physical = set.unwind(&mut mem, &thread.regs).unwrap();
    assert_eq!(physical[0].cfa, physical[1].regs.sp());
}

#[test]
fn disassembles_main_with_symbols() {
    let Some((_, core_path)) = core_at("values", "values::stop_here") else { return };
    let core = CoreFile::open(&core_path).unwrap();
    let set = core.symbol_set();
    let main = &set.find_functions("values::main")[0];
    let mut mem = core.memory();
    let insns = disasm::disassemble(&set, &mut mem, main.entry, 2000).unwrap();
    let call = insns
        .iter()
        .find(|i| i.text.starts_with("call") && i.text.contains("<values::stop_here>"))
        .expect("main calls stop_here");
    assert!(call.location.as_ref().is_some_and(|l| l.line == 58));
    // Instructions are contiguous.
    for w in insns.windows(2) {
        assert_eq!(w[0].addr + w[0].bytes.len() as u64, w[1].addr);
    }
}

#[test]
fn core_threads_of_values() {
    let Some((_, core_path)) = core_at("values", "values::stop_here") else { return };
    let core = CoreFile::open(&core_path).unwrap();
    assert_eq!(core.threads().len(), 1);
    let t = &core.threads()[0];
    let set = core.symbol_set();
    let f = set.function_at(t.regs.pc().unwrap()).unwrap();
    assert_eq!(f.name, "values::stop_here");
    assert!(core.executable().is_some());
    assert!(t.regs.get_bytes(hopper_core::Reg::XMM0).is_some_and(|b| b.len() == 16));
}
