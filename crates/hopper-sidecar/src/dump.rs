//! A stable, line-oriented text rendering of a sidecar, for tests, reports and `hsc-dump`.

use std::fmt::Write;

use crate::model::{
    Binding, ControlPath, FunctionOrigin, OwnershipEvent, SidecarData, UnitOutput, kind_name,
};

/// Render a whole sidecar.
pub fn dump(data: &SidecarData) -> String {
    let mut out = String::new();
    let h = &data.header;
    match &h.binding {
        Binding::Binary { build_id } => writeln!(out, "binary build-id {build_id}"),
        Binding::Partial(unit) => match &unit.output {
            UnitOutput::Library => {
                writeln!(
                    out,
                    "partial library {} {:016x}",
                    unit.name, unit.stable_crate_id
                )
            }
            UnitOutput::Linked { path, upstream } => writeln!(
                out,
                "partial executable {} {:016x} -> {path} ({} upstream)",
                unit.name,
                unit.stable_crate_id,
                upstream.len()
            ),
        },
    }
    .unwrap();
    writeln!(out, "rustc {}", h.rustc_version).unwrap();
    writeln!(out, "crates {}", h.crates.join(" ")).unwrap();
    writeln!(out, "tool {}", h.tool).unwrap();
    for f in &data.functions {
        let origin = match f.origin {
            FunctionOrigin::Source => "",
            FunctionOrigin::MacroExpansion => " [macro]",
            FunctionOrigin::Shim => " [shim]",
            FunctionOrigin::Dependency => " [dependency]",
        };
        writeln!(
            out,
            "fn {} ({}) at {}{origin}",
            f.demangled, f.symbol, f.def_span
        )
        .unwrap();
        for e in &f.events {
            writeln!(out, "  {}", event_line(e)).unwrap();
        }
    }
    out
}

/// One event on one line: `13:13 move a -> b : Config (32) bb0[3]`.
pub fn event_line(e: &OwnershipEvent) -> String {
    let mut s = format!("{}:{} {}", e.span.lo.line, e.span.lo.col, kind_name(e.kind));
    if let Some(src) = &e.src {
        write!(s, " {src}").unwrap();
    }
    if let Some(dst) = &e.dst {
        write!(s, " -> {dst}").unwrap();
    }
    if let Some(ty) = &e.ty {
        match ty.size {
            Some(n) => write!(s, " : {} ({n})", ty.name).unwrap(),
            None => write!(s, " : {}", ty.name).unwrap(),
        }
    }
    if let Some(c) = &e.callee {
        write!(s, " via {c}").unwrap();
    }
    write!(s, " {}", e.location).unwrap();
    match e.path {
        ControlPath::Normal => {}
        ControlPath::DropFlagGuarded => s.push_str(" [drop-flag]"),
        ControlPath::Unwind => s.push_str(" [unwind]"),
    }
    for frame in &e.inline_chain {
        write!(
            s,
            " inlined {} at {}:{}",
            frame.callee, frame.call_site.lo.line, frame.call_site.lo.col
        )
        .unwrap();
    }
    for c in &e.caveats {
        write!(s, " !{c:?}").unwrap();
    }
    s
}
