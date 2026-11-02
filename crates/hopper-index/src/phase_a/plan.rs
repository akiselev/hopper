//! Which addresses phase A breaks on, per module.

use std::collections::{BTreeMap, HashMap};

use hopper_core::SourceLoc;
use hopper_core::source::file_suffix_match;
use hopper_symbols::{ModuleId, SymbolKind, SymbolSet, UnitInfo};

use super::AllocatorFunction;
use crate::config::{FunctionSelection, IndexConfig, SourceSelection, is_toolchain_path};

/// A function with an entry breakpoint (a call-tree node).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FunctionSite {
    pub name: String,
    pub location: Option<SourceLoc>,
    /// The true entry (before the prologue): the CFA there is `rsp + 8`.
    pub entry: u64,
}

/// A source line and the addresses that stand for "execution reached it".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineSite {
    pub location: SourceLoc,
    pub addresses: Vec<u64>,
}

/// Breakpoint sites found in one module.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModulePlan {
    pub functions: Vec<FunctionSite>,
    pub lines: Vec<LineSite>,
    /// (address, kind, symbol name)
    pub allocators: Vec<(u64, AllocatorFunction, String)>,
    /// (address, function name)
    pub anchors: Vec<(u64, String)>,
    pub thread_entries: Vec<u64>,
}

/// The directories holding the module's own crates: compilation directories of Rust units
/// that are not toolchain or registry directories.
fn crate_dirs(units: &[UnitInfo]) -> Vec<String> {
    let mut dirs: Vec<String> = units
        .iter()
        .filter(|u| u.is_rust())
        .filter_map(|u| u.comp_dir.clone())
        .filter(|d| !d.is_empty() && !is_toolchain_path(&format!("{d}/")))
        .map(|d| if d.ends_with('/') { d } else { format!("{d}/") })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Whether `path` is selected.
pub(crate) fn file_selected(sel: &SourceSelection, dirs: &[String], path: &str) -> bool {
    match sel {
        SourceSelection::CrateFiles => {
            !is_toolchain_path(path) && dirs.iter().any(|d| path.starts_with(d.as_str()))
        }
        SourceSelection::All => true,
        SourceSelection::Files(fs) => fs.iter().any(|f| file_suffix_match(path, f)),
        SourceSelection::Nothing => false,
    }
}

/// The allocator entry point a symbol names, by its last path segment (`__rustc::__rust_alloc`
/// and a plain `__rust_alloc` alike).
pub fn allocator_function(name: &str) -> Option<AllocatorFunction> {
    let last = name.rsplit("::").next().unwrap_or(name);
    // libc's names are plain C symbols; a Rust function `my::free` is not the allocator.
    let c_symbol = !name.contains("::");
    Some(match last {
        "__rust_alloc" | "__rdl_alloc" | "__rg_alloc" => AllocatorFunction::RustAlloc,
        "__rust_alloc_zeroed" | "__rdl_alloc_zeroed" | "__rg_alloc_zeroed" => {
            AllocatorFunction::RustAllocZeroed
        }
        "__rust_realloc" | "__rdl_realloc" | "__rg_realloc" => AllocatorFunction::RustRealloc,
        "__rust_dealloc" | "__rdl_dealloc" | "__rg_dealloc" => AllocatorFunction::RustDealloc,
        "malloc" if c_symbol => AllocatorFunction::Malloc,
        "calloc" if c_symbol => AllocatorFunction::Calloc,
        "realloc" if c_symbol => AllocatorFunction::Realloc,
        "reallocarray" if c_symbol => AllocatorFunction::ReallocArray,
        "free" if c_symbol => AllocatorFunction::Free,
        "posix_memalign" if c_symbol => AllocatorFunction::PosixMemalign,
        "aligned_alloc" if c_symbol => AllocatorFunction::AlignedAlloc,
        "memalign" if c_symbol => AllocatorFunction::Memalign,
        _ => return None,
    })
}

/// Plan the sites of module `id`. `main` says whether it is the main executable (where
/// [`FunctionSelection::All`] applies).
pub(crate) fn plan_module(
    symbols: &SymbolSet,
    id: ModuleId,
    config: &IndexConfig,
    main: bool,
) -> ModulePlan {
    let mut plan = ModulePlan::default();
    let Some(m) = symbols.module(id) else {
        return plan;
    };
    let binary = m.binary();
    let units: &[UnitInfo] = match binary.dwarf() {
        Ok(d) => d.units(),
        Err(_) => &[],
    };
    let dirs = crate_dirs(units);

    // Function symbols, one per address.
    let mut seen = std::collections::HashSet::new();
    for sym in binary
        .symbols()
        .iter()
        .filter(|s| s.kind == SymbolKind::Function)
    {
        let entry = m.to_runtime(sym.address);
        if sym.address == 0 || !binary.contains_code(sym.address) {
            continue;
        }
        if config.allocator != crate::config::AllocatorTracking::Off
            && let Some(kind) = allocator_function(&sym.name)
            && !plan.allocators.iter().any(|(a, _, _)| *a == entry)
        {
            plan.allocators.push((entry, kind, sym.name.clone()));
        }
        if config.anchors.iter().any(|p| p.matches(&sym.name))
            && !plan.anchors.iter().any(|(a, _)| *a == entry)
        {
            plan.anchors.push((entry, sym.name.clone()));
        }
        if config.thread_entries.iter().any(|p| p.matches(&sym.name))
            && !plan.thread_entries.contains(&entry)
        {
            plan.thread_entries.push(entry);
        }
        if sym.size == 0 || !seen.insert(entry) {
            continue;
        }
        let selected = match &config.functions {
            FunctionSelection::Nothing => false,
            FunctionSelection::All => main,
            FunctionSelection::Matching(ps) => ps.iter().any(|p| p.matches(&sym.name)),
            FunctionSelection::CrateFiles => symbols
                .location(entry)
                .is_some_and(|l| file_selected(&SourceSelection::CrateFiles, &dirs, &l.file)),
        };
        if selected {
            let location = symbols
                .function_at(entry)
                .and_then(|f| f.decl)
                .or_else(|| symbols.location(entry));
            plan.functions.push(FunctionSite {
                name: sym.name.clone(),
                location,
                entry,
            });
        }
    }

    // Statement rows of the selected files.
    if config.lines != SourceSelection::Nothing {
        let mut rows: BTreeMap<(String, u32), Vec<u64>> = BTreeMap::new();
        let mut selected_file: HashMap<String, bool> = HashMap::new();
        for (addr, loc) in symbols.statement_rows(id) {
            let selected = *selected_file
                .entry(loc.file.clone())
                .or_insert_with(|| file_selected(&config.lines, &dirs, &loc.file));
            if selected {
                rows.entry((loc.file, loc.line)).or_default().push(addr);
            }
        }
        for ((file, line), addrs) in rows {
            // gdb's `break file:line`: the lowest address per function, past the prologue when it
            // is the function's entry.
            let mut per_function: BTreeMap<u64, u64> = BTreeMap::new();
            for a in addrs {
                let key = symbols.function_at(a).map_or(a, |f| f.entry);
                let slot = per_function.entry(key).or_insert(a);
                *slot = (*slot).min(a);
            }
            let mut addresses: Vec<u64> = per_function
                .into_values()
                .map(|a| match symbols.function_at(a) {
                    Some(f) if f.entry == a => symbols.breakpoint_address(&f),
                    _ => a,
                })
                .collect();
            addresses.sort_unstable();
            addresses.dedup();
            plan.lines.push(LineSite {
                location: SourceLoc::new(file, line),
                addresses,
            });
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_names() {
        assert_eq!(
            allocator_function("__rustc::__rust_alloc"),
            Some(AllocatorFunction::RustAlloc)
        );
        assert_eq!(
            allocator_function("__rust_dealloc"),
            Some(AllocatorFunction::RustDealloc)
        );
        assert_eq!(
            allocator_function("__rustc::__rdl_realloc"),
            Some(AllocatorFunction::RustRealloc)
        );
        assert_eq!(
            allocator_function("__rg_alloc_zeroed"),
            Some(AllocatorFunction::RustAllocZeroed)
        );
        assert_eq!(
            allocator_function("malloc"),
            Some(AllocatorFunction::Malloc)
        );
        assert_eq!(allocator_function("__libc_malloc"), None);
        assert_eq!(
            allocator_function("__rustc::__rust_alloc_error_handler"),
            None
        );
        assert_eq!(allocator_function("my::free"), None);
        assert_eq!(allocator_function("free"), Some(AllocatorFunction::Free));
    }

    #[test]
    fn crate_file_selection() {
        let dirs = vec!["/home/u/proj/".to_string()];
        let sel = SourceSelection::CrateFiles;
        assert!(file_selected(&sel, &dirs, "/home/u/proj/src/main.rs"));
        assert!(!file_selected(
            &sel,
            &dirs,
            "/rustc/abc/library/std/src/rt.rs"
        ));
        assert!(!file_selected(
            &sel,
            &dirs,
            "/home/u/.cargo/registry/src/x/y.rs"
        ));
        assert!(!file_selected(&sel, &dirs, "/home/u/other/src/main.rs"));
        assert!(file_selected(
            &SourceSelection::Files(vec!["src/main.rs".into()]),
            &dirs,
            "/x/src/main.rs"
        ));
    }
}
