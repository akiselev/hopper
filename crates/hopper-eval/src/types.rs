//! The type graph: DWARF type DIEs parsed once into [`Type`]s, cached per binary.
//!
//! Covers base types, structs, unions, Rust enums (`DW_TAG_variant_part`, including niche
//! layouts where one variant has no discriminant value), C-like enums, pointers and references,
//! arrays, subroutine types, typedefs and the const/volatile modifiers. Types the debug
//! information does not have (a pointer made by `&x`, the type of an integer literal) are
//! synthesised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gimli::AttributeValue;
use hopper_core::api::TypeKey;
use hopper_core::{HopperError, Result};
use hopper_symbols::{Binary, DieRef, DwarfIndex, ModuleId, Reader, UnitInfo};

/// A type, independent of which [`ModuleId`] its binary has in a symbol set.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TypeId {
    /// A type DIE.
    Die(DieRef),
    /// A pointer to another type, made by the evaluator (`&x`).
    PointerTo(Box<TypeId>),
    /// A type no DIE describes.
    Builtin(Builtin),
}

/// Types the evaluator makes up.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Builtin {
    /// Integer literals and integer results.
    I64,
    U64,
    Bool,
    /// The type of something whose type DIE is missing.
    Unknown,
}

const KIND_SHIFT: u32 = 60;
const DEPTH_SHIFT: u32 = 52;
const MODULE_SHIFT: u32 = 40;
const OFFSET_MASK: u64 = (1 << MODULE_SHIFT) - 1;
const MODULE_MASK: u64 = (1 << (DEPTH_SHIFT - MODULE_SHIFT)) - 1;
const DEPTH_MASK: u64 = (1 << (KIND_SHIFT - DEPTH_SHIFT)) - 1;

impl TypeId {
    /// The opaque, stable key the engine API carries. Layout: kind in bits 60..64 (0 a DIE,
    /// 1 a synthesised pointer chain, 2 a builtin), pointer depth in bits 52..60, the module in
    /// bits 40..52, and the `.debug_info` offset (or builtin code) in bits 0..40.
    pub fn key(&self, module: ModuleId) -> TypeKey {
        let m = (module.0 as u64 & MODULE_MASK) << MODULE_SHIFT;
        let mut depth = 0u64;
        let mut cur = self;
        while let TypeId::PointerTo(inner) = cur {
            depth += 1;
            cur = inner;
        }
        let base = match cur {
            TypeId::Die(d) => d.0 & OFFSET_MASK,
            TypeId::Builtin(b) => *b as u64,
            TypeId::PointerTo(_) => unreachable!("unwrapped above"),
        };
        let kind: u64 = match (cur, depth) {
            (TypeId::Builtin(_), 0) => 2,
            (TypeId::Builtin(_), _) => 3,
            (_, 0) => 0,
            _ => 1,
        };
        TypeKey((kind << KIND_SHIFT) | ((depth & DEPTH_MASK) << DEPTH_SHIFT) | m | base)
    }

    /// Decode a key made by [`TypeId::key`].
    pub fn from_key(key: TypeKey) -> Option<(ModuleId, TypeId)> {
        let k = key.0;
        let kind = k >> KIND_SHIFT;
        let depth = (k >> DEPTH_SHIFT) & DEPTH_MASK;
        let module = ModuleId(((k >> MODULE_SHIFT) & MODULE_MASK) as usize);
        let low = k & OFFSET_MASK;
        let base = match kind {
            0 | 1 => TypeId::Die(DieRef(low)),
            2 | 3 => TypeId::Builtin(match low {
                0 => Builtin::I64,
                1 => Builtin::U64,
                2 => Builtin::Bool,
                _ => Builtin::Unknown,
            }),
            _ => return None,
        };
        let mut id = base;
        for _ in 0..depth {
            id = TypeId::PointerTo(Box::new(id));
        }
        Some((module, id))
    }
}

/// How a base type's bytes are read.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BaseKind {
    Bool,
    /// A Unicode scalar value (`char`).
    Char,
    Signed,
    Unsigned,
    Float,
    /// C `signed char` / `unsigned char`: shown as characters.
    SignedChar,
    UnsignedChar,
}

/// A struct or union member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: TypeId,
    /// Byte offset in the containing value.
    pub offset: u64,
    /// `(bit offset from the start of the byte offset, bit size)` for bit fields.
    pub bits: Option<(u64, u64)>,
    pub artificial: bool,
}

impl Field {
    /// The name users write: `0` for rustc's tuple fields `__0`.
    pub fn display_name(&self) -> &str {
        tuple_index(&self.name).map_or(self.name.as_str(), |_| &self.name[2..])
    }
}

/// `Some(n)` for rustc's tuple field names `__n`.
pub fn tuple_index(name: &str) -> Option<u64> {
    name.strip_prefix("__").and_then(|n| n.parse().ok())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructType {
    pub fields: Vec<Field>,
    /// `DW_TAG_template_type_param`s: `T`, `K`, `V`, ...
    pub params: Vec<(String, TypeId)>,
}

impl StructType {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name || f.display_name() == name)
    }

    pub fn param(&self, name: &str) -> Option<&TypeId> {
        self.params.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    /// Every field is a tuple field (`__0`, `__1`, ...) and there is at least one.
    pub fn is_tuple_like(&self) -> bool {
        !self.fields.is_empty() && self.fields.iter().all(|f| tuple_index(&f.name).is_some())
    }
}

/// The discriminant value that selects a variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Discriminant {
    /// `DW_AT_discr_value`.
    Value(u64),
    /// `DW_AT_discr_list` ranges, inclusive.
    Ranges(Vec<(u64, u64)>),
    /// No value: the variant taken when no other matches (the dataful variant of a niche
    /// layout).
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub name: String,
    pub discriminant: Discriminant,
    /// The variant's payload struct (rustc puts it in a member named after the variant).
    pub ty: TypeId,
    pub offset: u64,
}

/// A Rust enum: a struct with a `DW_TAG_variant_part`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumType {
    /// The member holding the tag (or the niche); `None` for a single-variant enum.
    pub discriminant: Option<Field>,
    pub variants: Vec<Variant>,
    pub params: Vec<(String, TypeId)>,
}

impl EnumType {
    pub fn param(&self, name: &str) -> Option<&TypeId> {
        self.params.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }
}

/// A C-like enum (`DW_TAG_enumeration_type`), also how rustc describes fieldless Rust enums.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CEnumType {
    pub enumerators: Vec<(String, i128)>,
    pub signed: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PointerFlavour {
    Pointer,
    Reference,
    RvalueReference,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Modifier {
    Typedef,
    Const,
    Volatile,
    Restrict,
    Atomic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Base(BaseKind),
    Struct(StructType),
    Union(StructType),
    Enum(EnumType),
    CEnum(CEnumType),
    Pointer { pointee: Option<TypeId>, flavour: PointerFlavour },
    /// `len` is `None` for flexible arrays.
    Array { elem: TypeId, len: Option<u64> },
    Subroutine,
    /// A typedef or a qualifier over `target` (`None`: over `void`).
    Modified { modifier: Modifier, target: Option<TypeId> },
    /// `void`, or a DIE hopper does not understand.
    Void,
}

/// One type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub id: TypeId,
    /// The qualified name (`core::option::Option<u32>`), or a synthesised one (`*mut u8`).
    pub name: String,
    pub size: Option<u64>,
    pub kind: TypeKind,
}

impl Type {
    pub fn builtin(b: Builtin) -> Type {
        let (name, size, kind) = match b {
            Builtin::I64 => ("i64", 8, TypeKind::Base(BaseKind::Signed)),
            Builtin::U64 => ("u64", 8, TypeKind::Base(BaseKind::Unsigned)),
            Builtin::Bool => ("bool", 1, TypeKind::Base(BaseKind::Bool)),
            Builtin::Unknown => ("?", 0, TypeKind::Void),
        };
        Type { id: TypeId::Builtin(b), name: name.into(), size: Some(size), kind }
    }
}

/// One binary's parsed types; the `Arc` keeps the binary (and so the cache key) alive.
type BinaryTypes = (Arc<Binary>, HashMap<TypeId, Arc<Type>>);

/// Parsed types, per binary. Shared by every evaluation.
#[derive(Debug, Default)]
pub struct TypeCache {
    by_binary: Mutex<HashMap<usize, BinaryTypes>>,
}

impl TypeCache {
    pub fn get(&self, binary: &Arc<Binary>, id: &TypeId) -> Result<Arc<Type>> {
        let key = Arc::as_ptr(binary) as usize;
        {
            let guard = self.by_binary.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((_, types)) = guard.get(&key)
                && let Some(t) = types.get(id)
            {
                return Ok(t.clone());
            }
        }
        let t = Arc::new(self.parse(binary, id)?);
        let mut guard = self.by_binary.lock().unwrap_or_else(|p| p.into_inner());
        let (_, types) = guard.entry(key).or_insert_with(|| (binary.clone(), HashMap::new()));
        types.insert(id.clone(), t.clone());
        Ok(t)
    }

    fn parse(&self, binary: &Arc<Binary>, id: &TypeId) -> Result<Type> {
        match id {
            TypeId::Builtin(b) => Ok(Type::builtin(*b)),
            TypeId::PointerTo(inner) => {
                let pointee = self.get(binary, inner)?;
                Ok(Type {
                    id: id.clone(),
                    name: format!("*mut {}", pointee.name),
                    size: Some(8),
                    kind: TypeKind::Pointer {
                        pointee: Some((**inner).clone()),
                        flavour: PointerFlavour::Pointer,
                    },
                })
            }
            TypeId::Die(die) => {
                let dwarf = binary.dwarf()?;
                parse_die(self, binary, dwarf, *die)
            }
        }
    }

    /// The type with typedefs and qualifiers peeled off.
    pub fn strip(&self, binary: &Arc<Binary>, ty: Arc<Type>) -> Result<Arc<Type>> {
        let mut t = ty;
        for _ in 0..32 {
            match &t.kind {
                TypeKind::Modified { target: Some(target), .. } => t = self.get(binary, target)?,
                _ => return Ok(t),
            }
        }
        Ok(t)
    }

    /// The size of a type, looking through typedefs and qualifiers.
    pub fn size_of(&self, binary: &Arc<Binary>, id: &TypeId) -> Result<u64> {
        let t = self.get(binary, id)?;
        if let Some(s) = t.size {
            return Ok(s);
        }
        let t = self.strip(binary, t)?;
        if let Some(s) = t.size {
            return Ok(s);
        }
        if let TypeKind::Array { elem, len: Some(n) } = &t.kind {
            return Ok(self.size_of(binary, elem)? * n);
        }
        if matches!(t.kind, TypeKind::Pointer { .. }) {
            return Ok(8);
        }
        Err(HopperError::Eval(format!("type {} has no size", t.name)))
    }
}

fn parse_die(cache: &TypeCache, binary: &Arc<Binary>, dwarf: &DwarfIndex, die: DieRef) -> Result<Type> {
    let (unit, entry) = dwarf.entry(die)?;
    let type_attr = |e: &gimli::DebuggingInformationEntry<Reader>| {
        e.attr_value(gimli::DW_AT_type).and_then(|v| dwarf.attr_ref(unit, v)).map(TypeId::Die)
    };
    let size = entry.attr_value(gimli::DW_AT_byte_size).and_then(|v| v.udata_value());
    let own_name = dwarf.attr_str(unit, &entry, gimli::DW_AT_name);
    let qualified = || dwarf.qualified_name(die).or_else(|| own_name.clone());
    let id = TypeId::Die(die);
    let ty = match entry.tag() {
        gimli::DW_TAG_base_type => {
            let enc = match entry.attr_value(gimli::DW_AT_encoding) {
                Some(AttributeValue::Encoding(e)) => e,
                _ => gimli::DW_ATE_unsigned,
            };
            let kind = match enc {
                gimli::DW_ATE_boolean => BaseKind::Bool,
                gimli::DW_ATE_UTF => BaseKind::Char,
                gimli::DW_ATE_signed => BaseKind::Signed,
                gimli::DW_ATE_float => BaseKind::Float,
                gimli::DW_ATE_signed_char => BaseKind::SignedChar,
                gimli::DW_ATE_unsigned_char => {
                    // rustc describes `char` as DW_ATE_UTF; C's `char` is unsigned_char/signed_char.
                    BaseKind::UnsignedChar
                }
                _ => BaseKind::Unsigned,
            };
            let name = own_name.clone().unwrap_or_else(|| "?".into());
            // Rust's `char` is 4 bytes with DW_ATE_UTF; Rust `u8` is DW_ATE_unsigned.
            Type { id, name, size, kind: TypeKind::Base(kind) }
        }
        gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type | gimli::DW_TAG_union_type => {
            let name = qualified().unwrap_or_else(|| "<anonymous>".into());
            let kind = parse_aggregate(dwarf, unit, die, entry.tag() == gimli::DW_TAG_union_type)?;
            Type { id, name, size, kind }
        }
        gimli::DW_TAG_enumeration_type => {
            let name = qualified().unwrap_or_else(|| "<anonymous enum>".into());
            let underlying = type_attr(&entry);
            let signed = match &underlying {
                Some(u) => matches!(
                    cache.get(binary, u).map(|t| t.kind.clone()),
                    Ok(TypeKind::Base(BaseKind::Signed | BaseKind::SignedChar))
                ),
                None => false,
            };
            let enumerators = parse_enumerators(dwarf, unit, die)?;
            Type { id, name, size, kind: TypeKind::CEnum(CEnumType { enumerators, signed }) }
        }
        gimli::DW_TAG_pointer_type
        | gimli::DW_TAG_reference_type
        | gimli::DW_TAG_rvalue_reference_type
        | gimli::DW_TAG_ptr_to_member_type => {
            let flavour = match entry.tag() {
                gimli::DW_TAG_reference_type => PointerFlavour::Reference,
                gimli::DW_TAG_rvalue_reference_type => PointerFlavour::RvalueReference,
                _ => PointerFlavour::Pointer,
            };
            let pointee = type_attr(&entry);
            let name = match own_name.clone() {
                Some(n) => dwarf.qualified_name(die).unwrap_or(n),
                None => {
                    let inner = match &pointee {
                        Some(p) => cache.get(binary, p)?.name.clone(),
                        None => "()".into(),
                    };
                    match flavour {
                        PointerFlavour::Pointer => format!("*mut {inner}"),
                        _ => format!("&{inner}"),
                    }
                }
            };
            Type { id, name, size: Some(size.unwrap_or(8)), kind: TypeKind::Pointer { pointee, flavour } }
        }
        gimli::DW_TAG_array_type => {
            let elem = type_attr(&entry).unwrap_or(TypeId::Builtin(Builtin::Unknown));
            let len = array_len(dwarf, unit, die)?;
            let elem_ty = cache.get(binary, &elem)?;
            let name = own_name.clone().unwrap_or_else(|| match len {
                Some(n) => format!("[{}; {n}]", elem_ty.name),
                None => format!("[{}]", elem_ty.name),
            });
            let size = match (size, len) {
                (Some(s), _) => Some(s),
                (None, Some(n)) => cache.size_of(binary, &elem).ok().map(|s| s * n),
                _ => None,
            };
            Type { id, name, size, kind: TypeKind::Array { elem, len } }
        }
        gimli::DW_TAG_subroutine_type => Type {
            id,
            name: own_name.clone().unwrap_or_else(|| "fn()".into()),
            size: None,
            kind: TypeKind::Subroutine,
        },
        tag @ (gimli::DW_TAG_typedef
        | gimli::DW_TAG_const_type
        | gimli::DW_TAG_volatile_type
        | gimli::DW_TAG_restrict_type
        | gimli::DW_TAG_atomic_type) => {
            let modifier = match tag {
                gimli::DW_TAG_typedef => Modifier::Typedef,
                gimli::DW_TAG_const_type => Modifier::Const,
                gimli::DW_TAG_volatile_type => Modifier::Volatile,
                gimli::DW_TAG_restrict_type => Modifier::Restrict,
                _ => Modifier::Atomic,
            };
            let target = type_attr(&entry);
            let inner_name = match &target {
                Some(t) => cache.get(binary, t)?.name.clone(),
                None => "void".into(),
            };
            let name = match modifier {
                Modifier::Typedef => qualified().unwrap_or(inner_name),
                Modifier::Const => format!("const {inner_name}"),
                Modifier::Volatile => format!("volatile {inner_name}"),
                Modifier::Restrict => format!("{inner_name} restrict"),
                Modifier::Atomic => format!("_Atomic {inner_name}"),
            };
            let size = match (size, &target) {
                (Some(s), _) => Some(s),
                (None, Some(t)) => cache.size_of(binary, t).ok(),
                _ => None,
            };
            Type { id, name, size, kind: TypeKind::Modified { modifier, target } }
        }
        _ => Type { id, name: own_name.clone().unwrap_or_else(|| "void".into()), size, kind: TypeKind::Void },
    };
    Ok(ty)
}

fn member_offset(entry: &gimli::DebuggingInformationEntry<Reader>) -> u64 {
    match entry.attr_value(gimli::DW_AT_data_member_location) {
        Some(AttributeValue::Exprloc(expr)) => {
            // `DW_OP_plus_uconst n`, the only form compilers emit for plain members.
            let mut r = expr.0.clone();
            use gimli::Reader as _;
            match r.read_u8() {
                Ok(op) if op == gimli::DW_OP_plus_uconst.0 => r.read_uleb128().unwrap_or(0),
                _ => 0,
            }
        }
        Some(v) => v.udata_value().unwrap_or(0),
        None => 0,
    }
}

fn member(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    entry: &gimli::DebuggingInformationEntry<Reader>,
) -> Field {
    let name = dwarf.attr_str(unit, entry, gimli::DW_AT_name).unwrap_or_default();
    let ty = entry
        .attr_value(gimli::DW_AT_type)
        .and_then(|v| dwarf.attr_ref(unit, v))
        .map_or(TypeId::Builtin(Builtin::Unknown), TypeId::Die);
    let mut offset = member_offset(entry);
    let bit_size = entry.attr_value(gimli::DW_AT_bit_size).and_then(|v| v.udata_value());
    let data_bit = entry.attr_value(gimli::DW_AT_data_bit_offset).and_then(|v| v.udata_value());
    let bits = match (bit_size, data_bit) {
        (Some(size), Some(b)) => {
            offset = b / 8;
            Some((b % 8, size))
        }
        (Some(size), None) => Some((0, size)),
        _ => None,
    };
    let artificial = matches!(entry.attr_value(gimli::DW_AT_artificial), Some(AttributeValue::Flag(true)));
    Field { name, ty, offset, bits, artificial }
}

fn discr_value(v: AttributeValue<Reader>) -> Option<u64> {
    match v {
        AttributeValue::Sdata(s) => Some(s as u64),
        AttributeValue::Block(b) => {
            // A block holds a value wider than 64 bits: keep the low 64.
            use gimli::Reader as _;
            let bytes = b.to_slice().ok()?;
            let mut buf = [0u8; 8];
            let n = bytes.len().min(8);
            buf[..n].copy_from_slice(&bytes[..n]);
            Some(u64::from_le_bytes(buf))
        }
        other => other.udata_value(),
    }
}

fn parse_aggregate(dwarf: &DwarfIndex, unit: &UnitInfo, die: DieRef, is_union: bool) -> Result<TypeKind> {
    let mut tree = unit.unit.entries_tree(Some(unit.unit_offset(die))).map_err(eval_err)?;
    let root = tree.root().map_err(eval_err)?;
    let mut fields = Vec::new();
    let mut params = Vec::new();
    let mut variant_part = None;
    let mut children = root.children();
    while let Some(child) = children.next().map_err(eval_err)? {
        let e = child.entry();
        match e.tag() {
            gimli::DW_TAG_member if e.attr(gimli::DW_AT_external).is_none() => {
                // Static members (DW_AT_external or a declaration) have no offset in the value.
                if matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true))) {
                    continue;
                }
                fields.push(member(dwarf, unit, e));
            }
            gimli::DW_TAG_template_type_parameter => {
                if let (Some(n), Some(t)) = (
                    dwarf.attr_str(unit, e, gimli::DW_AT_name),
                    e.attr_value(gimli::DW_AT_type).and_then(|v| dwarf.attr_ref(unit, v)),
                ) {
                    params.push((n, TypeId::Die(t)));
                }
            }
            gimli::DW_TAG_variant_part => {
                variant_part = Some(parse_variant_part(dwarf, unit, child)?);
            }
            _ => {}
        }
    }
    if let Some((discriminant, variants)) = variant_part {
        return Ok(TypeKind::Enum(EnumType { discriminant, variants, params }));
    }
    let s = StructType { fields, params };
    Ok(if is_union { TypeKind::Union(s) } else { TypeKind::Struct(s) })
}

type VariantPart = (Option<Field>, Vec<Variant>);

fn parse_variant_part(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    node: gimli::EntriesTreeNode<'_, '_, Reader>,
) -> Result<VariantPart> {
    let discr_ref = node
        .entry()
        .attr_value(gimli::DW_AT_discr)
        .and_then(|v| dwarf.attr_ref(unit, v));
    let mut discriminant = None;
    let mut variants = Vec::new();
    let mut children = node.children();
    while let Some(child) = children.next().map_err(eval_err)? {
        let e = child.entry();
        match e.tag() {
            gimli::DW_TAG_member => {
                let here = unit.die_ref(e.offset());
                if discr_ref.is_none_or(|d| d == here) {
                    discriminant = Some(member(dwarf, unit, e));
                }
            }
            gimli::DW_TAG_variant => {
                let disc = if let Some(v) = e.attr_value(gimli::DW_AT_discr_value) {
                    discr_value(v).map_or(Discriminant::Default, Discriminant::Value)
                } else if let Some(AttributeValue::Block(b)) = e.attr_value(gimli::DW_AT_discr_list) {
                    Discriminant::Ranges(parse_discr_list(b))
                } else {
                    Discriminant::Default
                };
                let mut vchildren = child.children();
                while let Some(vm) = vchildren.next().map_err(eval_err)? {
                    if vm.entry().tag() == gimli::DW_TAG_member {
                        let f = member(dwarf, unit, vm.entry());
                        variants.push(Variant {
                            name: f.name.clone(),
                            discriminant: disc.clone(),
                            ty: f.ty,
                            offset: f.offset,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    // A variant part without DW_AT_discr has no tag member (single-variant enums).
    if discr_ref.is_none() {
        discriminant = None;
    }
    Ok((discriminant, variants))
}

/// `DW_AT_discr_list`: a sequence of `DW_DSC_label value` or `DW_DSC_range lo hi` (ULEB128).
fn parse_discr_list(block: Reader) -> Vec<(u64, u64)> {
    use gimli::Reader as _;
    let mut r = block;
    let mut out = Vec::new();
    while let Ok(kind) = r.read_u8() {
        let Ok(a) = r.read_uleb128() else { break };
        if kind == gimli::DW_DSC_range.0 {
            let Ok(b) = r.read_uleb128() else { break };
            out.push((a, b));
        } else {
            out.push((a, a));
        }
    }
    out
}

fn parse_enumerators(dwarf: &DwarfIndex, unit: &UnitInfo, die: DieRef) -> Result<Vec<(String, i128)>> {
    let mut tree = unit.unit.entries_tree(Some(unit.unit_offset(die))).map_err(eval_err)?;
    let root = tree.root().map_err(eval_err)?;
    let mut out = Vec::new();
    let mut children = root.children();
    while let Some(child) = children.next().map_err(eval_err)? {
        let e = child.entry();
        if e.tag() != gimli::DW_TAG_enumerator {
            continue;
        }
        let name = dwarf.attr_str(unit, e, gimli::DW_AT_name).unwrap_or_default();
        let value = match e.attr_value(gimli::DW_AT_const_value) {
            Some(AttributeValue::Sdata(v)) => v as i128,
            Some(AttributeValue::Udata(v)) => v as i128,
            Some(v) => v.udata_value().map_or(0, |u| u as i128),
            None => 0,
        };
        out.push((name, value));
    }
    Ok(out)
}

fn array_len(dwarf: &DwarfIndex, unit: &UnitInfo, die: DieRef) -> Result<Option<u64>> {
    let mut tree = unit.unit.entries_tree(Some(unit.unit_offset(die))).map_err(eval_err)?;
    let root = tree.root().map_err(eval_err)?;
    let mut total: Option<u64> = None;
    let mut any = false;
    let mut children = root.children();
    while let Some(child) = children.next().map_err(eval_err)? {
        let e = child.entry();
        if e.tag() != gimli::DW_TAG_subrange_type {
            continue;
        }
        any = true;
        let count = if let Some(c) = e.attr_value(gimli::DW_AT_count).and_then(|v| v.udata_value()) {
            Some(c)
        } else {
            let lower = e.attr_value(gimli::DW_AT_lower_bound).and_then(|v| v.udata_value()).unwrap_or(0);
            e.attr_value(gimli::DW_AT_upper_bound)
                .and_then(|v| match v {
                    AttributeValue::Sdata(s) => Some(s as u64),
                    other => other.udata_value(),
                })
                .map(|u| u.wrapping_sub(lower).wrapping_add(1))
        };
        // Several subranges describe a multi-dimensional array; treat it as flat.
        total = match (total, count) {
            (None, c) => c,
            (Some(t), Some(c)) => Some(t * c),
            (Some(_), None) => None,
        };
    }
    let _ = dwarf;
    Ok(if any { total } else { None })
}

fn eval_err(e: gimli::Error) -> HopperError {
    HopperError::Eval(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_keys_round_trip() {
        let m = ModuleId(3);
        for id in [
            TypeId::Die(DieRef(0x1234)),
            TypeId::PointerTo(Box::new(TypeId::Die(DieRef(0xabcdef)))),
            TypeId::PointerTo(Box::new(TypeId::PointerTo(Box::new(TypeId::Die(DieRef(9)))))),
            TypeId::Builtin(Builtin::I64),
            TypeId::PointerTo(Box::new(TypeId::Builtin(Builtin::U64))),
        ] {
            let key = id.key(m);
            assert_eq!(TypeId::from_key(key), Some((m, id.clone())), "{key:?}");
        }
        assert_ne!(TypeId::Die(DieRef(1)).key(ModuleId(0)), TypeId::Die(DieRef(1)).key(ModuleId(1)));
    }

    #[test]
    fn tuple_field_names() {
        assert_eq!(tuple_index("__12"), Some(12));
        assert_eq!(tuple_index("x"), None);
        let f = Field { name: "__0".into(), ty: TypeId::Builtin(Builtin::I64), offset: 0, bits: None, artificial: false };
        assert_eq!(f.display_name(), "0");
    }

    #[test]
    fn discr_lists() {
        // label 3, range 5..=9
        let bytes: Vec<u8> = vec![gimli::DW_DSC_label.0, 3, gimli::DW_DSC_range.0, 5, 9];
        let r = Reader::new(Arc::from(bytes), gimli::RunTimeEndian::Little);
        assert_eq!(parse_discr_list(r), vec![(3, 3), (5, 9)]);
    }
}
