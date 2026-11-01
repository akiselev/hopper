//! Helpers shared by the integration tests: a minimal ELF with a GNU build-id note, and a terse
//! way to write sidecar events.
#![allow(dead_code)]

use hopper_core::api::OwnershipKind;
use hopper_sidecar::{
    ControlPath, FunctionOrigin, FunctionRecord, LineCol, MirLocation, MirPosition, OwnershipEvent,
    Span, TypeInfo,
};

/// A 64-bit little-endian ELF holding only a `.note.gnu.build-id` section.
pub fn elf_with_build_id(id: &[u8]) -> Vec<u8> {
    let mut note = Vec::new();
    note.extend_from_slice(&4u32.to_le_bytes()); // namesz
    note.extend_from_slice(&(id.len() as u32).to_le_bytes()); // descsz
    note.extend_from_slice(&3u32.to_le_bytes()); // NT_GNU_BUILD_ID
    note.extend_from_slice(b"GNU\0");
    note.extend_from_slice(id);
    while note.len() % 4 != 0 {
        note.push(0);
    }
    let shstrtab = b"\0.note.gnu.build-id\0.shstrtab\0";
    let note_off = 64u64;
    let strtab_off = note_off + note.len() as u64;
    let mut shoff = strtab_off + shstrtab.len() as u64;
    shoff = shoff.div_ceil(8) * 8;

    let mut out = Vec::new();
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    out.extend_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // entry
    out.extend_from_slice(&0u64.to_le_bytes()); // phoff
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&64u16.to_le_bytes()); // ehsize
    out.extend_from_slice(&56u16.to_le_bytes()); // phentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // phnum
    out.extend_from_slice(&64u16.to_le_bytes()); // shentsize
    out.extend_from_slice(&3u16.to_le_bytes()); // shnum
    out.extend_from_slice(&2u16.to_le_bytes()); // shstrndx
    assert_eq!(out.len(), 64);
    out.extend_from_slice(&note);
    out.extend_from_slice(shstrtab);
    out.resize(shoff as usize, 0);

    let section = |out: &mut Vec<u8>, name: u32, ty: u32, flags: u64, off: u64, size: u64, align: u64| {
        out.extend_from_slice(&name.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // addr
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // link
        out.extend_from_slice(&0u32.to_le_bytes()); // info
        out.extend_from_slice(&align.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // entsize
    };
    section(&mut out, 0, 0, 0, 0, 0, 0);
    section(&mut out, 1, 7, 2, note_off, note.len() as u64, 4); // SHT_NOTE, SHF_ALLOC
    section(&mut out, 20, 3, 0, strtab_off, shstrtab.len() as u64, 1); // SHT_STRTAB
    out
}

pub const FILE: &str = "/w/worked-example/src/main.rs";

pub fn span(line: u32, col: u32, end_col: u32) -> Span {
    Span { file: FILE.into(), lo: LineCol { line, col }, hi: LineCol { line, col: end_col } }
}

/// An event on `line:col` at MIR `bb[stmt]` (`stmt = None` for the terminator).
#[allow(clippy::too_many_arguments)]
pub fn ev(
    kind: OwnershipKind,
    line: u32,
    col: u32,
    block: u32,
    stmt: Option<u32>,
    src: Option<&str>,
    dst: Option<&str>,
    ty: Option<(&str, u64)>,
) -> OwnershipEvent {
    OwnershipEvent {
        kind,
        span: span(line, col, col + 1),
        inline_chain: Vec::new(),
        location: MirLocation {
            block,
            position: stmt.map_or(MirPosition::Terminator, MirPosition::Statement),
        },
        path: ControlPath::Normal,
        src: src.map(str::to_owned),
        dst: dst.map(str::to_owned),
        ty: ty.map(|(name, size)| TypeInfo { name: name.into(), size: Some(size) }),
        callee: None,
        caveats: Vec::new(),
    }
}

pub fn with_callee(mut e: OwnershipEvent, callee: &str) -> OwnershipEvent {
    e.callee = Some(callee.into());
    e
}

pub fn func(symbol: &str, def_path: &str, origin: FunctionOrigin, line: u32, events: Vec<OwnershipEvent>) -> FunctionRecord {
    FunctionRecord {
        symbol: symbol.into(),
        demangled: def_path.into(),
        def_path: def_path.into(),
        krate: def_path.split("::").next().unwrap_or(def_path).into(),
        def_span: span(line, 1, 2),
        origin,
        events,
    }
}
