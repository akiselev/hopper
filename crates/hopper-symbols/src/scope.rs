//! Variables in scope at a PC: the parameters and locals of one source-level frame, from the
//! lexical blocks that cover the PC, without the variables of inlined calls inside it (those
//! belong to their own frames).

use gimli::AttributeValue;
use hopper_core::{HopperError, Result};

use crate::dwarf::{DieRef, DwarfIndex, Reader, RustcVersion, UnitInfo, die_ranges, sym_err};
use crate::module::{ModuleId, SymbolSet};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum VariableKind {
    Parameter,
    Local,
}

/// One variable visible in a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    pub kind: VariableKind,
    /// The DIE that carries `DW_AT_location` (the concrete DIE, for inlined copies).
    pub die: DieRef,
    /// The variable's type DIE.
    pub type_die: Option<DieRef>,
    pub decl_line: Option<u32>,
    /// Lexical nesting below the frame's function: a deeper variable shadows a shallower one with
    /// the same name.
    pub depth: u32,
}

/// What the evaluator needs to read one frame's variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameScope {
    pub module: ModuleId,
    /// The frame's DIE: a `DW_TAG_subprogram`, or the `DW_TAG_inlined_subroutine` of an inlined
    /// frame.
    pub frame_die: DieRef,
    /// The concrete subprogram the code is in; its `DW_AT_frame_base` is the frame base of every
    /// inlined frame inside it too.
    pub subprogram: DieRef,
    /// The PC the scope was computed for, as a file address in the module.
    pub file_pc: u64,
    pub rustc_version: Option<RustcVersion>,
    /// In declaration order (outer blocks first).
    pub variables: Vec<Variable>,
}

impl FrameScope {
    /// The innermost visible variable named `name`.
    pub fn lookup(&self, name: &str) -> Option<&Variable> {
        self.variables.iter().filter(|v| v.name == name).max_by_key(|v| v.depth)
    }

    /// Whether `v` is hidden by a deeper variable of the same name.
    pub fn is_shadowed(&self, v: &Variable) -> bool {
        self.variables.iter().any(|o| o.name == v.name && o.depth > v.depth)
    }
}

impl SymbolSet {
    /// The scope of the source frame `frame_die` at runtime address `pc`. `frame_die` comes from
    /// [`SymbolSet::frames_at`] (or a stack frame); `None` picks the innermost frame at `pc`.
    pub fn scope_at(&self, pc: u64, frame_die: Option<DieRef>) -> Result<FrameScope> {
        let (module, m) = self
            .module_at(pc)
            .ok_or_else(|| HopperError::NotFound(format!("no module contains {pc:#x}")))?;
        let dwarf = m.binary().dwarf()?;
        let (_, subprogram) = self
            .subprogram_at(pc)
            .ok_or_else(|| HopperError::NotFound(format!("no function with debug info at {pc:#x}")))?;
        let frame_die = match frame_die {
            Some(d) => d,
            None => self.frames_at(pc).into_iter().find_map(|f| f.die).unwrap_or(subprogram),
        };
        let file_pc = m.to_file(pc);
        let unit = dwarf.unit_of(frame_die)?;
        let mut variables = Vec::new();
        collect_scope(dwarf, unit, frame_die, file_pc, &mut variables)?;
        Ok(FrameScope {
            module,
            frame_die,
            subprogram,
            file_pc,
            rustc_version: unit.rustc_version,
            variables,
        })
    }
}

fn collect_scope(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    root: DieRef,
    pc: u64,
    out: &mut Vec<Variable>,
) -> Result<()> {
    let mut tree = unit.unit.entries_tree(Some(unit.unit_offset(root))).map_err(sym_err)?;
    let node = tree.root().map_err(sym_err)?;
    walk(dwarf, unit, node, pc, 0, out)
}

fn walk(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    node: gimli::EntriesTreeNode<'_, '_, Reader>,
    pc: u64,
    depth: u32,
    out: &mut Vec<Variable>,
) -> Result<()> {
    let mut children = node.children();
    while let Some(child) = children.next().map_err(sym_err)? {
        let entry = child.entry();
        match entry.tag() {
            gimli::DW_TAG_formal_parameter | gimli::DW_TAG_variable => {
                if let Some(v) = variable(dwarf, unit, entry, depth) {
                    out.push(v);
                }
            }
            gimli::DW_TAG_lexical_block => {
                let ranges = die_ranges(dwarf.dwarf(), &unit.unit, entry);
                if ranges.is_empty() || ranges.iter().any(|r| r.contains(&pc)) {
                    walk(dwarf, unit, child, pc, depth + 1, out)?;
                }
            }
            // Inlined calls are frames of their own; nested subprograms are other functions.
            _ => {}
        }
    }
    Ok(())
}

fn variable(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    entry: &gimli::DebuggingInformationEntry<Reader>,
    depth: u32,
) -> Option<Variable> {
    let die = unit.die_ref(entry.offset());
    let origin = entry.attr_value(gimli::DW_AT_abstract_origin).and_then(|v| dwarf.attr_ref(unit, v));
    let origin_entry = origin.and_then(|o| dwarf.entry(o).ok());
    let lookup_str = |at| {
        dwarf.attr_str(unit, entry, at).or_else(|| {
            let (ou, oe) = origin_entry.as_ref()?;
            dwarf.attr_str(ou, oe, at)
        })
    };
    let name = lookup_str(gimli::DW_AT_name)?;
    let type_die = entry
        .attr_value(gimli::DW_AT_type)
        .and_then(|v| dwarf.attr_ref(unit, v))
        .or_else(|| {
            let (ou, oe) = origin_entry.as_ref()?;
            oe.attr_value(gimli::DW_AT_type).and_then(|v| dwarf.attr_ref(ou, v))
        });
    let decl_line = entry
        .attr_value(gimli::DW_AT_decl_line)
        .or_else(|| origin_entry.as_ref().and_then(|(_, oe)| oe.attr_value(gimli::DW_AT_decl_line)))
        .and_then(|v: AttributeValue<Reader>| v.udata_value())
        .map(|l| l as u32);
    let kind = if entry.tag() == gimli::DW_TAG_formal_parameter {
        VariableKind::Parameter
    } else {
        VariableKind::Local
    };
    Some(Variable { name, kind, die, type_die, decl_line, depth })
}
