//! The `.hsc` byte layout: a fixed prefix (magic, format version), then one zstd frame holding
//! the header, the string table and the function records, all as LEB128 varints. The spec is
//! `docs/formats/hsc.md`; keep the two in step.

use std::collections::HashMap;

use hopper_core::api::OwnershipKind;
use hopper_core::{HopperError, Result};

use crate::model::{
    Binding, BuildId, Caveat, ControlPath, CrateUnit, FunctionOrigin, FunctionRecord, Header,
    InlineFrame, LineCol, MirLocation, MirPosition, OwnershipEvent, SidecarData, Span, TypeInfo,
    UnitOutput,
};

/// The first eight bytes of every `.hsc` file. The high byte and the CR LF / ^Z / LF tail catch
/// text-mode transfers, as PNG's signature does.
pub const MAGIC: [u8; 8] = *b"\x89HSC\r\n\x1a\n";

/// The layout version this crate writes and reads. A reader rejects any other version.
pub const FORMAT_VERSION: u32 = 1;

/// zstd level for written sidecars: sidecars are written once per build and read often.
const ZSTD_LEVEL: i32 = 9;

/// Upper bound on a decompressed body, so a corrupt or hostile file cannot exhaust memory.
const MAX_BODY_BYTES: usize = 1 << 31;

fn corrupt(what: impl Into<String>) -> HopperError {
    HopperError::Invalid(format!("corrupt sidecar: {}", what.into()))
}

// ---------------------------------------------------------------- encoding

/// Encode a sidecar into `.hsc` bytes.
pub fn encode(data: &SidecarData) -> Result<Vec<u8>> {
    let mut strings = StringTable::default();
    let mut records = Vec::new();
    encode_functions(&data.functions, &mut strings, &mut records);

    let mut body = Vec::new();
    encode_header(&data.header, &mut body);
    put_uvar(&mut body, strings.strings.len() as u64);
    for s in &strings.strings {
        put_bytes(&mut body, s.as_bytes());
    }
    body.extend_from_slice(&records);

    let mut encoder = zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL)?;
    encoder.include_checksum(true)?;
    encoder.set_pledged_src_size(Some(body.len() as u64))?;
    std::io::Write::write_all(&mut encoder, &body)?;
    let compressed = encoder.finish()?;

    let mut out = Vec::with_capacity(12 + compressed.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}

#[derive(Default)]
struct StringTable {
    strings: Vec<String>,
    ids: HashMap<String, u64>,
}

impl StringTable {
    fn id(&mut self, s: &str) -> u64 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.strings.len() as u64;
        self.strings.push(s.to_owned());
        self.ids.insert(s.to_owned(), id);
        id
    }

    /// `0` for none, `id + 1` for a string.
    fn opt_id(&mut self, s: Option<&str>) -> u64 {
        s.map_or(0, |s| self.id(s) + 1)
    }
}

fn put_uvar(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_uvar(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn encode_header(h: &Header, out: &mut Vec<u8>) {
    match &h.binding {
        Binding::Binary { build_id } => {
            out.push(0);
            put_bytes(out, &build_id.0);
        }
        Binding::Partial(unit) => {
            out.push(1);
            put_bytes(out, unit.name.as_bytes());
            put_uvar(out, unit.stable_crate_id);
            match &unit.output {
                UnitOutput::Library => out.push(0),
                UnitOutput::Linked { path, upstream } => {
                    out.push(1);
                    put_bytes(out, path.as_bytes());
                    put_uvar(out, upstream.len() as u64);
                    for id in upstream {
                        put_uvar(out, *id);
                    }
                }
            }
        }
    }
    put_bytes(out, h.rustc_version.as_bytes());
    put_uvar(out, h.crates.len() as u64);
    for c in &h.crates {
        put_bytes(out, c.as_bytes());
    }
    put_bytes(out, h.tool.as_bytes());
}

fn encode_functions(functions: &[FunctionRecord], strings: &mut StringTable, out: &mut Vec<u8>) {
    put_uvar(out, functions.len() as u64);
    for f in functions {
        put_uvar(out, strings.id(&f.symbol));
        put_uvar(out, strings.id(&f.demangled));
        put_uvar(out, strings.id(&f.def_path));
        put_uvar(out, strings.id(&f.krate));
        put_span(out, strings, &f.def_span);
        out.push(origin_code(f.origin));
        put_uvar(out, f.events.len() as u64);
        for e in &f.events {
            put_event(out, strings, e);
        }
    }
}

fn put_span(out: &mut Vec<u8>, strings: &mut StringTable, s: &Span) {
    put_uvar(out, strings.id(&s.file));
    put_uvar(out, u64::from(s.lo.line));
    put_uvar(out, u64::from(s.lo.col));
    // The end line as a delta: spans almost never run backwards, and a corrupt one is clamped
    // to the start line on encode rather than wrapping.
    put_uvar(out, u64::from(s.hi.line.saturating_sub(s.lo.line)));
    put_uvar(out, u64::from(s.hi.col));
}

fn put_event(out: &mut Vec<u8>, strings: &mut StringTable, e: &OwnershipEvent) {
    out.push(kind_code(e.kind));
    put_span(out, strings, &e.span);
    put_uvar(out, e.inline_chain.len() as u64);
    for frame in &e.inline_chain {
        put_uvar(out, strings.id(&frame.callee));
        put_span(out, strings, &frame.call_site);
    }
    put_uvar(out, u64::from(e.location.block));
    put_uvar(
        out,
        match e.location.position {
            MirPosition::Terminator => 0,
            MirPosition::Statement(i) => u64::from(i) + 1,
        },
    );
    out.push(match e.path {
        ControlPath::Normal => 0,
        ControlPath::DropFlagGuarded => 1,
        ControlPath::Unwind => 2,
    });
    put_uvar(out, strings.opt_id(e.src.as_deref()));
    put_uvar(out, strings.opt_id(e.dst.as_deref()));
    match &e.ty {
        None => put_uvar(out, 0),
        Some(ty) => {
            put_uvar(out, strings.id(&ty.name) + 1);
            put_uvar(out, ty.size.map_or(0, |s| s + 1));
        }
    }
    put_uvar(out, strings.opt_id(e.callee.as_deref()));
    out.push(e.caveats.iter().fold(0u8, |bits, c| bits | caveat_bit(*c)));
}

fn kind_code(kind: OwnershipKind) -> u8 {
    match kind {
        OwnershipKind::Move => 0,
        OwnershipKind::Copy => 1,
        OwnershipKind::Clone => 2,
        OwnershipKind::SharedBorrow => 3,
        OwnershipKind::MutBorrow => 4,
        OwnershipKind::Projection => 5,
        OwnershipKind::Drop => 6,
        OwnershipKind::Aggregate => 7,
        OwnershipKind::Literal => 8,
        OwnershipKind::CallReturn => 9,
        OwnershipKind::StorageDead => 10,
    }
}

fn kind_from_code(code: u8) -> Result<OwnershipKind> {
    Ok(match code {
        0 => OwnershipKind::Move,
        1 => OwnershipKind::Copy,
        2 => OwnershipKind::Clone,
        3 => OwnershipKind::SharedBorrow,
        4 => OwnershipKind::MutBorrow,
        5 => OwnershipKind::Projection,
        6 => OwnershipKind::Drop,
        7 => OwnershipKind::Aggregate,
        8 => OwnershipKind::Literal,
        9 => OwnershipKind::CallReturn,
        10 => OwnershipKind::StorageDead,
        other => return Err(corrupt(format!("unknown event kind {other}"))),
    })
}

fn origin_code(origin: FunctionOrigin) -> u8 {
    match origin {
        FunctionOrigin::Source => 0,
        FunctionOrigin::MacroExpansion => 1,
        FunctionOrigin::Shim => 2,
        FunctionOrigin::Dependency => 3,
    }
}

fn origin_from_code(code: u8) -> Result<FunctionOrigin> {
    Ok(match code {
        0 => FunctionOrigin::Source,
        1 => FunctionOrigin::MacroExpansion,
        2 => FunctionOrigin::Shim,
        3 => FunctionOrigin::Dependency,
        other => return Err(corrupt(format!("unknown function origin {other}"))),
    })
}

fn caveat_bit(c: Caveat) -> u8 {
    match c {
        Caveat::Unsafe => 1,
        Caveat::InteriorMutability => 2,
    }
}

fn caveats_from_bits(bits: u8) -> Result<Vec<Caveat>> {
    if bits & !3 != 0 {
        return Err(corrupt(format!("unknown caveat bits {bits:#x}")));
    }
    let mut out = Vec::new();
    if bits & 1 != 0 {
        out.push(Caveat::Unsafe);
    }
    if bits & 2 != 0 {
        out.push(Caveat::InteriorMutability);
    }
    Ok(out)
}

// ---------------------------------------------------------------- decoding

/// Decode `.hsc` bytes.
pub fn decode(bytes: &[u8]) -> Result<SidecarData> {
    let body = decompress(bytes)?;
    let mut r = Reader {
        bytes: &body,
        pos: 0,
    };
    let header = decode_header(&mut r)?;
    let count = r.len("string count")?;
    let mut strings = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        strings.push(r.string()?);
    }
    let functions = decode_functions(&mut r, &strings)?;
    if r.pos != body.len() {
        return Err(corrupt(format!("{} trailing bytes", body.len() - r.pos)));
    }
    Ok(SidecarData { header, functions })
}

/// Decode only the header (the body is still decompressed and checksummed).
pub fn decode_header_only(bytes: &[u8]) -> Result<Header> {
    let body = decompress(bytes)?;
    decode_header(&mut Reader {
        bytes: &body,
        pos: 0,
    })
}

fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 12 || bytes[..8] != MAGIC {
        return Err(HopperError::Invalid(
            "not a hopper sidecar (bad magic)".into(),
        ));
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if version != FORMAT_VERSION {
        return Err(HopperError::Invalid(format!(
            "sidecar format version {version}; this reader understands {FORMAT_VERSION}"
        )));
    }
    let mut decoder = zstd::stream::Decoder::new(&bytes[12..])?;
    let mut body = Vec::new();
    let read = std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut decoder, MAX_BODY_BYTES as u64 + 1),
        &mut body,
    )
    .map_err(|e| corrupt(format!("zstd: {e}")))?;
    if read > MAX_BODY_BYTES {
        return Err(corrupt("body larger than 2 GiB"));
    }
    Ok(body)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| corrupt("truncated"))?;
        self.pos += 1;
        Ok(b)
    }

    fn uvar(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            let bits = u64::from(b & 0x7f);
            if shift == 63 && bits > 1 {
                return Err(corrupt("varint overflows u64"));
            }
            v |= bits << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(corrupt("varint longer than 10 bytes"))
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        u32::try_from(self.uvar()?).map_err(|_| corrupt(format!("{what} exceeds u32")))
    }

    /// A count of items that each take at least one byte, so it cannot exceed what is left.
    fn len(&mut self, what: &str) -> Result<usize> {
        let n = self.uvar()?;
        if n > (self.bytes.len() - self.pos) as u64 {
            return Err(corrupt(format!("{what} {n} exceeds the remaining bytes")));
        }
        Ok(n as usize)
    }

    fn bytes(&mut self) -> Result<&[u8]> {
        let n = self.len("byte length")?;
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn string(&mut self) -> Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b.to_vec()).map_err(|_| corrupt("string is not UTF-8"))
    }

    fn str_id(&mut self, strings: &[String]) -> Result<String> {
        let id = self.uvar()?;
        strings
            .get(id as usize)
            .cloned()
            .ok_or_else(|| corrupt(format!("string id {id} out of range")))
    }

    fn opt_str_id(&mut self, strings: &[String]) -> Result<Option<String>> {
        match self.uvar()? {
            0 => Ok(None),
            id => strings
                .get(id as usize - 1)
                .cloned()
                .map(Some)
                .ok_or_else(|| corrupt(format!("string id {} out of range", id - 1))),
        }
    }
}

fn decode_header(r: &mut Reader<'_>) -> Result<Header> {
    let binding = match r.byte()? {
        0 => Binding::Binary {
            build_id: BuildId(r.bytes()?.to_vec()),
        },
        1 => {
            let name = r.string()?;
            let stable_crate_id = r.uvar()?;
            let output = match r.byte()? {
                0 => UnitOutput::Library,
                1 => {
                    let path = r.string()?;
                    let n = r.len("upstream count")?;
                    let upstream = (0..n).map(|_| r.uvar()).collect::<Result<Vec<_>>>()?;
                    UnitOutput::Linked { path, upstream }
                }
                other => return Err(corrupt(format!("unknown unit output {other}"))),
            };
            Binding::Partial(CrateUnit {
                name,
                stable_crate_id,
                output,
            })
        }
        other => return Err(corrupt(format!("unknown binding {other}"))),
    };
    let rustc_version = r.string()?;
    let n = r.len("crate count")?;
    let crates = (0..n).map(|_| r.string()).collect::<Result<Vec<_>>>()?;
    let tool = r.string()?;
    Ok(Header {
        binding,
        rustc_version,
        crates,
        tool,
    })
}

fn decode_functions(r: &mut Reader<'_>, strings: &[String]) -> Result<Vec<FunctionRecord>> {
    let n = r.len("function count")?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let symbol = r.str_id(strings)?;
        let demangled = r.str_id(strings)?;
        let def_path = r.str_id(strings)?;
        let krate = r.str_id(strings)?;
        let def_span = read_span(r, strings)?;
        let origin = origin_from_code(r.byte()?)?;
        let m = r.len("event count")?;
        let mut events = Vec::with_capacity(m);
        for _ in 0..m {
            events.push(read_event(r, strings)?);
        }
        out.push(FunctionRecord {
            symbol,
            demangled,
            def_path,
            krate,
            def_span,
            origin,
            events,
        });
    }
    Ok(out)
}

fn read_span(r: &mut Reader<'_>, strings: &[String]) -> Result<Span> {
    let file = r.str_id(strings)?;
    let lo_line = r.u32("line")?;
    let lo_col = r.u32("column")?;
    let delta = r.u32("line delta")?;
    let hi_col = r.u32("column")?;
    let hi_line = lo_line
        .checked_add(delta)
        .ok_or_else(|| corrupt("span end line overflows"))?;
    Ok(Span {
        file,
        lo: LineCol {
            line: lo_line,
            col: lo_col,
        },
        hi: LineCol {
            line: hi_line,
            col: hi_col,
        },
    })
}

fn read_event(r: &mut Reader<'_>, strings: &[String]) -> Result<OwnershipEvent> {
    let kind = kind_from_code(r.byte()?)?;
    let span = read_span(r, strings)?;
    let n = r.len("inline chain length")?;
    let mut inline_chain = Vec::with_capacity(n);
    for _ in 0..n {
        let callee = r.str_id(strings)?;
        let call_site = read_span(r, strings)?;
        inline_chain.push(InlineFrame { callee, call_site });
    }
    let block = r.u32("basic block")?;
    let position = match r.u32("statement index")? {
        0 => MirPosition::Terminator,
        i => MirPosition::Statement(i - 1),
    };
    let path = match r.byte()? {
        0 => ControlPath::Normal,
        1 => ControlPath::DropFlagGuarded,
        2 => ControlPath::Unwind,
        other => return Err(corrupt(format!("unknown control path {other}"))),
    };
    let src = r.opt_str_id(strings)?;
    let dst = r.opt_str_id(strings)?;
    let ty = match r.uvar()? {
        0 => None,
        id => {
            let name = strings
                .get(id as usize - 1)
                .cloned()
                .ok_or_else(|| corrupt("type name id out of range"))?;
            let size = match r.uvar()? {
                0 => None,
                s => Some(s - 1),
            };
            Some(TypeInfo { name, size })
        }
    };
    let callee = r.opt_str_id(strings)?;
    let caveats = caveats_from_bits(r.byte()?)?;
    Ok(OwnershipEvent {
        kind,
        span,
        inline_chain,
        location: MirLocation { block, position },
        path,
        src,
        dst,
        ty,
        callee,
        caveats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_uvar(&mut out, v);
            let mut r = Reader {
                bytes: &out,
                pos: 0,
            };
            assert_eq!(r.uvar().unwrap(), v);
            assert_eq!(r.pos, out.len());
        }
        let mut out = Vec::new();
        put_uvar(&mut out, 300);
        assert_eq!(out, [0xac, 0x02]);
    }

    #[test]
    fn varint_rejects_overflow() {
        let bytes = [0xff; 11];
        assert!(
            Reader {
                bytes: &bytes,
                pos: 0
            }
            .uvar()
            .is_err()
        );
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert!(
            Reader {
                bytes: &bytes,
                pos: 0
            }
            .uvar()
            .is_err()
        );
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        assert!(matches!(
            decode(b"not a sidecar"),
            Err(HopperError::Invalid(_))
        ));
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&99u32.to_le_bytes());
        let err = decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("version 99"), "{err}");
    }

    #[test]
    fn every_kind_has_a_code() {
        use OwnershipKind::*;
        for k in [
            Move,
            Copy,
            Clone,
            SharedBorrow,
            MutBorrow,
            Projection,
            Drop,
            Aggregate,
            Literal,
            CallReturn,
            StorageDead,
        ] {
            assert_eq!(kind_from_code(kind_code(k)).unwrap(), k);
        }
    }
}
