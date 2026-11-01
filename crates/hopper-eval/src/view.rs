//! How a value is shown: its [`Shape`] (scalar, pointer, struct, enum, array, or a std collection
//! read through the layout table), its children (paged), the [`PathElem`] that selects each child,
//! and a one-line summary in the format rust-gdb prints (`Vec(size=3) = {1, 2, 3}`,
//! `core::option::Option<u32>::Some(5)`, `"hello"`), so hopper's output can be checked against
//! gdb's and reads familiarly.
//!
//! The std specialisations are adapted from BugStalker's `variable/value/specialization`
//! (`mod.rs`, `hashbrown.rs`, `btree.rs`; MIT, see `THIRD_PARTY_NOTICES.md`) and Rust's
//! `gdb_providers.py` (MIT OR Apache-2.0): every read goes through the evaluator's `Session`
//! (hopper's `Memory` trait) rather than ptrace, and field names come from `layouts.rs`.

use std::sync::Arc;

use hopper_core::api::PathElem;
use hopper_core::{HopperError, Result};

use crate::layouts::{Role, StdKind, StdLayout, layout_for};
use crate::location::ValueLocation;
use crate::types::{BaseKind, Builtin, Discriminant, StructType, Type, TypeId, TypeKind, Variant};
use crate::value::{Session, Value};

/// Collections longer than this are cut short in summaries (gdb's default `print elements`).
pub const SUMMARY_ELEMENTS: usize = 200;
/// Summaries stop nesting below this depth (`{...}`).
pub const SUMMARY_DEPTH: usize = 8;
/// Summaries are cut at about this many characters.
pub const SUMMARY_CHARS: usize = 4000;
/// Strings are read up to this many bytes for a summary.
const TEXT_BYTES: u64 = 4096;
/// A B-tree node holds at most 11 keys (`CAPACITY = 2 * B - 1`, B = 6).
const BTREE_CAPACITY: u64 = 11;

/// What a value is, after looking through typedefs and recognising std types.
#[derive(Clone, Debug)]
pub enum Shape {
    /// Integers, floats, bools, chars, C-like enums, `()`.
    Scalar,
    Pointer { pointee: Option<TypeId>, addr: u64 },
    /// Structs, unions and tuples; `base` is the value with its type stripped of typedefs.
    Struct { st: StructType, base: Value },
    /// A Rust enum and its active variant with the variant's payload (`None` when no variant's
    /// discriminant matches: the bytes are not a valid value).
    Enum { active: Option<(Variant, Value)> },
    Array { elem: TypeId, len: u64, base: Value },
    /// `String`, `&str`, `OsString`, `PathBuf`: shown as text.
    Text { kind: StdKind, data: u64, len: u64 },
    /// `Vec`, `&[T]`, `VecDeque` (with its ring: `head` and capacity).
    Seq { kind: StdKind, data: u64, len: u64, elem: TypeId, elem_size: u64, ring: Option<(u64, u64)> },
    /// `HashMap`, `HashSet` (hashbrown): the occupied slots' element addresses.
    Hash { kind: StdKind, slots: Vec<u64>, items: u64, pair: TypeId },
    /// `BTreeMap`, `BTreeSet`: `(key address, value address)` in order.
    BTree { kind: StdKind, entries: Vec<(u64, u64)>, length: u64, key: TypeId, val: TypeId },
    /// `Rc`, `Arc` and their `Weak`s. `value` is `None` for a dangling `Weak`.
    Shared { kind: StdKind, strong: u64, weak: u64, value: Option<Value> },
    /// `Cell`, `RefCell`, `Ref`, `RefMut`.
    Wrapper { kind: StdKind, value: Value, borrow: Option<i64> },
    /// `NonZero<T>`.
    Number(Value),
}

/// One child of a value.
#[derive(Clone, Debug)]
pub struct Child {
    pub name: String,
    /// The path elements that select this child from its parent.
    pub path: Vec<PathElem>,
    pub value: Result<Value>,
}

/// The shape of `v`. A std type whose layout cannot be read (an unknown rustc layout, or
/// garbage memory) falls back to its raw structure.
pub fn shape(v: &Value, s: &mut Session<'_>) -> Result<Shape> {
    let stripped = s.strip(&v.ty)?;
    let v = Value { ty: stripped.clone(), loc: v.loc.clone() };
    if let Some(layout) = layout_for(&stripped.name)
        && let Ok(shape) = std_shape(layout, &v, s)
    {
        return Ok(shape);
    }
    raw_shape(&v, s)
}

fn raw_shape(v: &Value, s: &mut Session<'_>) -> Result<Shape> {
    Ok(match &v.ty.kind {
        TypeKind::Base(_) | TypeKind::CEnum(_) | TypeKind::Void | TypeKind::Subroutine => Shape::Scalar,
        TypeKind::Modified { .. } => Shape::Scalar,
        TypeKind::Pointer { pointee, .. } => Shape::Pointer { pointee: pointee.clone(), addr: v.as_u64(s)? },
        TypeKind::Struct(st) | TypeKind::Union(st) => Shape::Struct { st: st.clone(), base: v.clone() },
        TypeKind::Array { elem, len } => {
            Shape::Array { elem: elem.clone(), len: len.unwrap_or(0), base: v.clone() }
        }
        TypeKind::Enum(e) => {
            let active = active_variant(v, s, e)?;
            Shape::Enum { active }
        }
    })
}

/// The variant a Rust enum value holds.
fn active_variant(
    v: &Value,
    s: &mut Session<'_>,
    e: &crate::types::EnumType,
) -> Result<Option<(Variant, Value)>> {
    let chosen = match &e.discriminant {
        None => e.variants.first().cloned(),
        Some(d) => {
            let tag = v.field(s, d)?;
            let size = tag.size(s)?;
            let raw = tag.as_u64(s)?;
            let mask = if size >= 8 { u64::MAX } else { (1u64 << (size * 8)) - 1 };
            let raw = raw & mask;
            let hit = e.variants.iter().find(|var| match &var.discriminant {
                Discriminant::Value(x) => x & mask == raw,
                Discriminant::Ranges(rs) => rs.iter().any(|(lo, hi)| (lo & mask) <= raw && raw <= (hi & mask)),
                Discriminant::Default => false,
            });
            hit.or_else(|| e.variants.iter().find(|var| var.discriminant == Discriminant::Default)).cloned()
        }
    };
    Ok(match chosen {
        Some(var) => {
            let payload = v.slice(s, var.offset, &var.ty)?;
            Some((var, payload))
        }
        None => None,
    })
}

// ---------------------------------------------------------------- std types through the table

/// Follow the first path for `role` that exists, then unwrap newtypes down to a scalar, pointer
/// or enum.
fn role_value(layout: &StdLayout, role: Role, v: &Value, s: &mut Session<'_>) -> Result<Value> {
    let paths = layout.paths(role, s.rustc);
    for path in &paths {
        if let Some(found) = follow(v, s, path)? {
            return unwrap_newtypes(found, s);
        }
    }
    Err(HopperError::Eval(format!("{}: no field for {role:?} (tried {paths:?})", v.ty.name)))
}

/// Like [`role_value`] but stops at the field the path names (no unwrapping).
fn role_field(layout: &StdLayout, role: Role, v: &Value, s: &mut Session<'_>) -> Result<Value> {
    let paths = layout.paths(role, s.rustc);
    for path in &paths {
        if let Some(found) = follow(v, s, path)? {
            return Ok(found);
        }
    }
    Err(HopperError::Eval(format!("{}: no field for {role:?} (tried {paths:?})", v.ty.name)))
}

fn follow(v: &Value, s: &mut Session<'_>, path: &[&str]) -> Result<Option<Value>> {
    let mut cur = v.clone();
    for name in path {
        let ty = s.strip(&cur.ty)?;
        let field = match &ty.kind {
            TypeKind::Struct(st) | TypeKind::Union(st) => st.field(name).cloned(),
            _ => None,
        };
        let Some(f) = field else { return Ok(None) };
        cur = Value { ty, loc: cur.loc }.field(s, &f)?;
    }
    Ok(Some(cur))
}

/// Descend through single-field wrappers (`NonNull`, `Unique`, `Cell`, `UnsafeCell`,
/// `UsizeNoHighBit`, atomics) to the scalar, pointer or enum inside.
fn unwrap_newtypes(v: Value, s: &mut Session<'_>) -> Result<Value> {
    let mut cur = v;
    for _ in 0..16 {
        let ty = s.strip(&cur.ty)?;
        let TypeKind::Struct(st) = &ty.kind else { return Ok(Value { ty, loc: cur.loc }) };
        let sized: Vec<_> = st
            .fields
            .iter()
            .filter(|f| s.size_of(&f.ty).is_ok_and(|n| n > 0))
            .cloned()
            .collect();
        let next = match sized.as_slice() {
            [only] => only.clone(),
            _ => match st.fields.iter().find(|f| matches!(f.name.as_str(), "pointer" | "__0" | "value" | "v")) {
                Some(f) => f.clone(),
                None => return Ok(Value { ty, loc: cur.loc }),
            },
        };
        cur = Value { ty, loc: cur.loc }.field(s, &next)?;
    }
    Ok(cur)
}

fn param(v: &Value, name: &str) -> Option<TypeId> {
    match &v.ty.kind {
        TypeKind::Struct(st) => st.param(name).cloned(),
        TypeKind::Enum(e) => e.param(name).cloned(),
        _ => None,
    }
}

fn pointee_of(v: &Value) -> Option<TypeId> {
    match &v.ty.kind {
        TypeKind::Pointer { pointee, .. } => pointee.clone(),
        _ => None,
    }
}

fn std_shape(layout: &StdLayout, v: &Value, s: &mut Session<'_>) -> Result<Shape> {
    let kind = layout.kind;
    match kind {
        StdKind::String | StdKind::Str => {
            let data = role_value(layout, Role::DataPtr, v, s)?.as_u64(s)?;
            let len = role_value(layout, Role::Len, v, s)?.as_u64(s)?;
            Ok(Shape::Text { kind, data, len })
        }
        StdKind::OsString | StdKind::PathBuf => {
            let inner = role_field(layout, Role::Inner, v, s)?;
            let inner_ty = s.strip(&inner.ty)?;
            let vec_layout = layout_for(&inner_ty.name)
                .filter(|l| l.kind == StdKind::Vec)
                .ok_or_else(|| HopperError::Eval(format!("{} does not wrap a Vec", v.ty.name)))?;
            let inner = Value { ty: inner_ty, loc: inner.loc };
            let data = role_value(vec_layout, Role::DataPtr, &inner, s)?.as_u64(s)?;
            let len = role_value(vec_layout, Role::Len, &inner, s)?.as_u64(s)?;
            Ok(Shape::Text { kind, data, len })
        }
        StdKind::Slice | StdKind::Vec | StdKind::VecDeque => {
            let data_v = role_value(layout, Role::DataPtr, v, s)?;
            let data = data_v.as_u64(s)?;
            let len = role_value(layout, Role::Len, v, s)?.as_u64(s)?;
            let elem = match kind {
                StdKind::Slice => pointee_of(&data_v),
                _ => param(v, "T"),
            }
            .ok_or_else(|| HopperError::Eval(format!("{}: element type unknown", v.ty.name)))?;
            let elem_size = s.size_of(&elem)?;
            let ring = if kind == StdKind::VecDeque {
                let head = role_value(layout, Role::Head, v, s)?.as_u64(s)?;
                let cap = if elem_size == 0 {
                    u64::MAX
                } else {
                    role_value(layout, Role::Capacity, v, s)?.as_u64(s)?
                };
                if len > cap {
                    return Err(HopperError::Eval("VecDeque longer than its capacity".into()));
                }
                Some((head, cap))
            } else {
                None
            };
            Ok(Shape::Seq { kind, data, len, elem, elem_size, ring })
        }
        StdKind::HashMap | StdKind::HashSet => {
            let table = role_field(layout, Role::Table, v, s)?;
            let table_ty = s.strip(&table.ty)?;
            let pair = match &table_ty.kind {
                TypeKind::Struct(st) => st.param("T").cloned(),
                _ => None,
            }
            .ok_or_else(|| HopperError::Eval("hash table element type unknown".into()))?;
            let pair_size = s.size_of(&pair)?;
            let mask = role_value(layout, Role::BucketMask, v, s)?.as_u64(s)?;
            let ctrl = role_value(layout, Role::Ctrl, v, s)?.as_u64(s)?;
            let items = role_value(layout, Role::Items, v, s)?.as_u64(s)?;
            let mut slots = Vec::new();
            if items > 0 {
                let buckets = mask.checked_add(1).filter(|b| b.is_power_of_two() && *b <= 1 << 24)
                    .ok_or_else(|| HopperError::Eval("implausible hash table size".into()))?;
                let control = s.read(ctrl, buckets)?;
                for (i, c) in control.iter().enumerate() {
                    if c & 0x80 == 0 {
                        slots.push(ctrl.wrapping_sub((i as u64 + 1) * pair_size));
                    }
                }
            }
            Ok(Shape::Hash { kind, slots, items, pair })
        }
        StdKind::BTreeMap | StdKind::BTreeSet => {
            let map = if kind == StdKind::BTreeSet {
                let m = role_field(layout, Role::Inner, v, s)?;
                let ty = s.strip(&m.ty)?;
                Value { ty, loc: m.loc }
            } else {
                v.clone()
            };
            let map_layout = layout_for(&map.ty.name)
                .filter(|l| l.kind == StdKind::BTreeMap)
                .ok_or_else(|| HopperError::Eval("BTreeSet without its map".into()))?;
            let key = param(&map, "K").ok_or_else(|| HopperError::Eval("BTreeMap key type unknown".into()))?;
            let val = param(&map, "V").ok_or_else(|| HopperError::Eval("BTreeMap value type unknown".into()))?;
            let length = role_value(map_layout, Role::Length, &map, s)?.as_u64(s)?;
            let root = role_value(map_layout, Role::Root, &map, s)?;
            let entries = btree_entries(map_layout, &root, length, &key, &val, s)?;
            Ok(Shape::BTree { kind, entries, length, key, val })
        }
        StdKind::Rc | StdKind::Arc | StdKind::RcWeak | StdKind::ArcWeak => {
            let ptr = role_value(layout, Role::SharedPtr, v, s)?;
            let addr = ptr.as_u64(s)?;
            let inner_ty = pointee_of(&ptr).ok_or_else(|| HopperError::Eval("Rc without RcInner".into()))?;
            let weak_kind = matches!(kind, StdKind::RcWeak | StdKind::ArcWeak);
            if weak_kind && (addr == u64::MAX || addr == 0) {
                return Ok(Shape::Shared { kind, strong: 0, weak: 0, value: None });
            }
            let inner = s.at(&inner_ty, addr)?;
            let inner = Value { ty: s.strip(&inner.ty)?, loc: inner.loc };
            let strong = role_value(layout, Role::Strong, &inner, s)?.as_u64(s)?;
            let weak = role_value(layout, Role::Weak, &inner, s)?.as_u64(s)?;
            let value = role_field(layout, Role::SharedValue, &inner, s)?;
            // The implicit weak reference all strong references share is not shown.
            Ok(Shape::Shared { kind, strong, weak: weak.saturating_sub(1), value: Some(value) })
        }
        StdKind::Cell => Ok(Shape::Wrapper { kind, value: role_field(layout, Role::Value, v, s)?, borrow: None }),
        StdKind::RefCell => {
            let value = role_field(layout, Role::Value, v, s)?;
            let borrow = role_value(layout, Role::Borrow, v, s)?.as_i128(s)? as i64;
            Ok(Shape::Wrapper { kind, value, borrow: Some(borrow) })
        }
        StdKind::Ref | StdKind::RefMut => {
            let ptr = role_value(layout, Role::Value, v, s)?;
            let pointee = pointee_of(&ptr).ok_or_else(|| HopperError::Eval("Ref without a pointer".into()))?;
            let value = ptr.pointee(s, &pointee)?;
            let borrow = role_value(layout, Role::Borrow, v, s)?.as_i128(s)? as i64;
            Ok(Shape::Wrapper { kind, value, borrow: Some(borrow) })
        }
        StdKind::NonZero => Ok(Shape::Number(role_value(layout, Role::Number, v, s)?)),
    }
}

/// The in-order `(key, value)` addresses of a B-tree, from its `Option<NodeRef>` root.
fn btree_entries(
    layout: &StdLayout,
    root: &Value,
    length: u64,
    key: &TypeId,
    val: &TypeId,
    s: &mut Session<'_>,
) -> Result<Vec<(u64, u64)>> {
    if length == 0 {
        return Ok(Vec::new());
    }
    // The root is `Option<NodeRef<..>>`: take the `Some` payload's `__0`.
    let root_ty = s.strip(&root.ty)?;
    let node_ref = match &root_ty.kind {
        TypeKind::Enum(e) => match active_variant(&Value { ty: root_ty.clone(), loc: root.loc.clone() }, s, e)? {
            Some((_, payload)) => {
                let pty = s.strip(&payload.ty)?;
                match &pty.kind {
                    TypeKind::Struct(st) if !st.fields.is_empty() => {
                        let f = st.fields[0].clone();
                        Value { ty: pty, loc: payload.loc }.field(s, &f)?
                    }
                    _ => return Ok(Vec::new()),
                }
            }
            None => return Ok(Vec::new()),
        },
        _ => root.clone(),
    };
    let node_ref = Value { ty: s.strip(&node_ref.ty)?, loc: node_ref.loc };
    let node_ptr = role_value(layout, Role::RootNode, &node_ref, s)?;
    let height = role_value(layout, Role::RootHeight, &node_ref, s)?.as_u64(s)?;
    let leaf_ty = pointee_of(&node_ptr).ok_or_else(|| HopperError::Eval("B-tree node type unknown".into()))?;
    let leaf = s.ty(&leaf_ty)?;
    let leaf = s.strip(&leaf)?;
    let TypeKind::Struct(st) = &leaf.kind else {
        return Err(HopperError::Eval("B-tree leaf is not a struct".into()));
    };
    let field_offset = |role: Role| -> Result<u64> {
        let path = layout.paths(role, None);
        let name = path.first().and_then(|p| p.first()).copied().unwrap_or_default();
        st.field(name).map(|f| f.offset).ok_or_else(|| HopperError::Eval(format!("LeafNode has no {name}")))
    };
    let len_off = field_offset(Role::NodeLen)?;
    let keys_off = field_offset(Role::NodeKeys)?;
    let vals_off = field_offset(Role::NodeVals)?;
    let leaf_size = leaf.size.ok_or_else(|| HopperError::Eval("LeafNode has no size".into()))?;
    // InternalNode { data: LeafNode, edges: [BoxedNode; 12] }: the edges follow the leaf,
    // pointer-aligned.
    let edges_off = leaf_size.div_ceil(8) * 8;
    let key_size = s.size_of(key)?;
    let val_size = s.size_of(val)?;
    let mut out = Vec::new();
    let node = node_ptr.as_u64(s)?;
    let layout_offsets = NodeOffsets { len_off, keys_off, vals_off, edges_off, key_size, val_size };
    walk_btree(s, node, height, &layout_offsets, length, &mut out, 0)?;
    Ok(out)
}

struct NodeOffsets {
    len_off: u64,
    keys_off: u64,
    vals_off: u64,
    edges_off: u64,
    key_size: u64,
    val_size: u64,
}

fn walk_btree(
    s: &mut Session<'_>,
    node: u64,
    height: u64,
    o: &NodeOffsets,
    limit: u64,
    out: &mut Vec<(u64, u64)>,
    depth: u32,
) -> Result<()> {
    if depth > 32 || node == 0 {
        return Err(HopperError::Eval("B-tree deeper than any real one".into()));
    }
    let b = s.read(node + o.len_off, 2)?;
    let len = u16::from_le_bytes([b[0], b[1]]) as u64;
    if len > BTREE_CAPACITY {
        return Err(HopperError::Eval("B-tree node longer than its capacity".into()));
    }
    for i in 0..=len {
        if out.len() as u64 >= limit {
            return Ok(());
        }
        if height > 0 {
            let child = s.read_u64(node + o.edges_off + 8 * i)?;
            walk_btree(s, child, height - 1, o, limit, out, depth + 1)?;
        }
        if i < len && (out.len() as u64) < limit {
            out.push((node + o.keys_off + i * o.key_size, node + o.vals_off + i * o.val_size));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- children

fn builtin_u64(v: u64) -> Value {
    Value {
        ty: Arc::new(Type::builtin(Builtin::U64)),
        loc: ValueLocation::Bytes(Arc::from(v.to_le_bytes().to_vec())),
    }
}

fn builtin_i64(v: i64) -> Value {
    Value {
        ty: Arc::new(Type::builtin(Builtin::I64)),
        loc: ValueLocation::Bytes(Arc::from(v.to_le_bytes().to_vec())),
    }
}

/// Whether a pointer to `pointee` can be followed (sized, not `void`, not a function).
fn derefable(s: &Session<'_>, pointee: &Option<TypeId>, addr: u64) -> bool {
    let Some(p) = pointee else { return false };
    if addr == 0 {
        return false;
    }
    let Ok(t) = s.ty(p) else { return false };
    let Ok(t) = s.strip(&t) else { return false };
    !matches!(t.kind, TypeKind::Void | TypeKind::Subroutine) && s.size_of(p).is_ok()
}

/// How many children the value has; `None` for leaves.
pub fn child_count(shape: &Shape, s: &Session<'_>) -> Option<u64> {
    match shape {
        Shape::Scalar | Shape::Text { .. } | Shape::Number(_) => None,
        Shape::Pointer { pointee, addr } => derefable(s, pointee, *addr).then_some(1),
        Shape::Struct { st, .. } => Some(st.fields.len() as u64),
        Shape::Enum { active } => match active {
            Some((_, payload)) => match &payload.ty.kind {
                TypeKind::Struct(st) if !st.fields.is_empty() => Some(st.fields.len() as u64),
                _ => None,
            },
            None => None,
        },
        Shape::Array { len, .. } | Shape::Seq { len, .. } => Some(*len),
        Shape::Hash { slots, .. } => Some(slots.len() as u64),
        Shape::BTree { entries, .. } => Some(entries.len() as u64),
        Shape::Shared { value, .. } => Some(if value.is_some() { 3 } else { 2 }),
        Shape::Wrapper { borrow, .. } => Some(if borrow.is_some() { 2 } else { 1 }),
    }
}

/// The children of a value in `range` (by child index). `parent` is the value's own name, used
/// for the `*name` child of a pointer.
pub fn children(
    shape: &Shape,
    s: &mut Session<'_>,
    parent: &str,
    offset: u64,
    limit: u64,
) -> Result<Vec<Child>> {
    let end = child_count(shape, s).unwrap_or(0).min(offset.saturating_add(limit));
    let mut out = Vec::new();
    for i in offset..end {
        out.push(child_at(shape, s, parent, i)?);
    }
    Ok(out)
}

/// Child number `i`.
pub fn child_at(shape: &Shape, s: &mut Session<'_>, parent: &str, i: u64) -> Result<Child> {
    Ok(match shape {
        Shape::Pointer { pointee, addr } => {
            let p = pointee.clone().ok_or_else(|| HopperError::Eval("pointer to unknown type".into()))?;
            Child { name: format!("*{parent}"), path: vec![PathElem::Deref], value: s.at(&p, *addr) }
        }
        Shape::Struct { .. } | Shape::Enum { .. } => {
            let (owner, st, variant) = struct_of(shape, s)?;
            let f = st
                .fields
                .get(i as usize)
                .cloned()
                .ok_or_else(|| HopperError::Eval(format!("no field {i}")))?;
            let mut path = Vec::new();
            if let Some(v) = &variant {
                path.push(PathElem::Variant(v.clone()));
            }
            path.push(PathElem::Field(f.display_name().to_string()));
            let value = owner.field(s, &f);
            Child { name: f.display_name().to_string(), path, value }
        }
        Shape::Array { elem, len, base } => {
            let value = if i < *len {
                s.size_of(elem).and_then(|size| base.slice(s, i * size, elem))
            } else {
                Err(HopperError::Eval(format!("index {i} out of bounds (len {len})")))
            };
            Child { name: format!("[{i}]"), path: vec![PathElem::Index(i)], value }
        }
        Shape::Seq { .. } => Child {
            name: format!("[{i}]"),
            path: vec![PathElem::Index(i)],
            value: seq_element(shape, s, i),
        },
        Shape::Hash { kind, slots, pair, .. } => {
            let addr = slots[i as usize];
            let entry = s.at(pair, addr)?;
            let (key, val) = tuple_parts(&entry, s)?;
            match kind {
                StdKind::HashSet => Child { name: format!("[{i}]"), path: vec![PathElem::Index(i)], value: Ok(key) },
                _ => {
                    let name = format!("[{}]", summary(&key, s));
                    Child { name, path: vec![PathElem::Index(i)], value: Ok(val) }
                }
            }
        }
        Shape::BTree { kind, entries, key, val, .. } => {
            let (k, v) = entries[i as usize];
            let key_v = s.at(key, k)?;
            match kind {
                StdKind::BTreeSet => Child { name: format!("[{i}]"), path: vec![PathElem::Index(i)], value: Ok(key_v) },
                _ => {
                    let name = format!("[{}]", summary(&key_v, s));
                    Child { name, path: vec![PathElem::Index(i)], value: s.at(val, v) }
                }
            }
        }
        Shape::Shared { strong, weak, value, .. } => {
            let names: Vec<&str> = if value.is_some() { vec!["value", "strong", "weak"] } else { vec!["strong", "weak"] };
            let name = names.get(i as usize).copied().ok_or_else(|| HopperError::Eval(format!("no child {i}")))?;
            let v = match name {
                "value" => Ok(value.clone().expect("checked")),
                "strong" => Ok(builtin_u64(*strong)),
                _ => Ok(builtin_u64(*weak)),
            };
            Child { name: name.to_string(), path: vec![PathElem::Field(name.to_string())], value: v }
        }
        Shape::Wrapper { kind, value, borrow } => {
            let value_name = if matches!(kind, StdKind::Ref | StdKind::RefMut) { "*value" } else { "value" };
            match (i, borrow) {
                (0, _) => Child {
                    name: value_name.to_string(),
                    path: vec![PathElem::Field("value".into())],
                    value: Ok(value.clone()),
                },
                (1, Some(b)) => Child {
                    name: "borrow".into(),
                    path: vec![PathElem::Field("borrow".into())],
                    value: Ok(builtin_i64(*b)),
                },
                _ => return Err(HopperError::Eval(format!("no child {i}"))),
            }
        }
        Shape::Scalar | Shape::Text { .. } | Shape::Number(_) => {
            return Err(HopperError::Eval("a leaf value has no children".into()));
        }
    })
}

/// The struct whose fields are the children: the value itself, or an enum's active payload.
fn struct_of(shape: &Shape, s: &mut Session<'_>) -> Result<(Value, StructType, Option<String>)> {
    match shape {
        Shape::Struct { st, base } => Ok((base.clone(), st.clone(), None)),
        Shape::Enum { active: Some((var, payload)) } => {
            let ty = s.strip(&payload.ty)?;
            match &ty.kind {
                TypeKind::Struct(st) => Ok((Value { ty: ty.clone(), loc: payload.loc.clone() }, st.clone(), Some(var.name.clone()))),
                _ => Err(HopperError::Eval("enum payload is not a struct".into())),
            }
        }
        _ => Err(HopperError::Eval("not a struct".into())),
    }
}

fn seq_element(shape: &Shape, s: &mut Session<'_>, i: u64) -> Result<Value> {
    let Shape::Seq { data, len, elem, elem_size, ring, .. } = shape else { unreachable!("seq shape") };
    if i >= *len {
        return Err(HopperError::Eval(format!("index {i} out of bounds (len {len})")));
    }
    let physical = match ring {
        Some((head, cap)) if *cap != 0 && *cap != u64::MAX => (head + i) % cap,
        _ => i,
    };
    s.at(elem, data + physical * elem_size)
}

/// A pair's `__0` and `__1` (a hash map entry); for a set entry (`(K, ())`) the value is `()`.
fn tuple_parts(v: &Value, s: &mut Session<'_>) -> Result<(Value, Value)> {
    let ty = s.strip(&v.ty)?;
    let TypeKind::Struct(st) = &ty.kind else {
        return Ok((v.clone(), v.clone()));
    };
    let base = Value { ty: ty.clone(), loc: v.loc.clone() };
    let k = st.field("__0").cloned().ok_or_else(|| HopperError::Eval("entry has no key".into()))?;
    let key = base.field(s, &k)?;
    let val = match st.field("__1").cloned() {
        Some(f) => base.field(s, &f)?,
        None => key.clone(),
    };
    Ok((key, val))
}

/// Select the child of `v` that `path` names, consuming one element (two for
/// `Variant` + `Field`). Returns the child and how many elements were used.
pub fn select(v: &Value, s: &mut Session<'_>, path: &[PathElem]) -> Result<(Value, usize)> {
    let Some(first) = path.first() else { return Ok((v.clone(), 0)) };
    let sh = shape(v, s)?;
    let not_found = |what: &str| HopperError::Eval(format!("{} has no {what}", v.ty.name));
    match (first, &sh) {
        (PathElem::Deref, Shape::Pointer { pointee, addr }) => {
            let p = pointee.clone().ok_or_else(|| not_found("pointee type"))?;
            Ok((s.at(&p, *addr)?, 1))
        }
        (PathElem::Deref, Shape::Shared { value: Some(inner), .. })
        | (PathElem::Deref, Shape::Wrapper { value: inner, .. }) => Ok((inner.clone(), 1)),
        (PathElem::Field(name), Shape::Struct { st, base }) => {
            let f = st.field(name).cloned().ok_or_else(|| not_found(&format!("field {name}")))?;
            Ok((base.field(s, &f)?, 1))
        }
        (PathElem::Variant(want), Shape::Enum { active }) => {
            let Some((var, _)) = active else { return Err(not_found("valid variant")) };
            if &var.name != want {
                return Err(HopperError::Eval(format!("variant {want} is not active (it is {})", var.name)));
            }
            let Some(PathElem::Field(name)) = path.get(1) else {
                return Err(HopperError::Invalid("a variant must be followed by a field".into()));
            };
            let (owner, st, _) = struct_of(&sh, s)?;
            let f = st.field(name).cloned().ok_or_else(|| not_found(&format!("field {name}")))?;
            Ok((owner.field(s, &f)?, 2))
        }
        (PathElem::Field(name), Shape::Enum { active: Some(_) }) => {
            let (owner, st, _) = struct_of(&sh, s)?;
            let f = st.field(name).cloned().ok_or_else(|| not_found(&format!("field {name}")))?;
            Ok((owner.field(s, &f)?, 1))
        }
        (PathElem::Index(i), Shape::Array { .. }) => Ok((child_at(&sh, s, "", *i)?.value?, 1)),
        (PathElem::Index(i), Shape::Seq { .. }) => Ok((seq_element(&sh, s, *i)?, 1)),
        (PathElem::Index(i), Shape::Hash { .. } | Shape::BTree { .. }) => {
            let c = child_at(&sh, s, "", *i).map_err(|_| HopperError::Eval(format!("no entry {i}")))?;
            Ok((c.value?, 1))
        }
        (PathElem::Field(name), Shape::Shared { .. } | Shape::Wrapper { .. }) => {
            let n = child_count(&sh, s).unwrap_or(0);
            for i in 0..n {
                let c = child_at(&sh, s, "", i)?;
                if matches!(c.path.first(), Some(PathElem::Field(f)) if f == name) {
                    return Ok((c.value?, 1));
                }
            }
            Err(not_found(&format!("child {name}")))
        }
        (elem, _) => Err(HopperError::Eval(format!("{:?} does not apply to {}", elem, v.ty.name))),
    }
}

/// Follow a whole path.
pub fn select_path(v: &Value, s: &mut Session<'_>, path: &[PathElem]) -> Result<Value> {
    let mut cur = v.clone();
    let mut i = 0;
    while i < path.len() {
        let (next, used) = select(&cur, s, &path[i..])?;
        cur = next;
        i += used.max(1);
    }
    Ok(cur)
}

// ---------------------------------------------------------------- summaries

/// The one-line rendering of a value, as rust-gdb prints it.
pub fn summary(v: &Value, s: &mut Session<'_>) -> String {
    let mut out = String::new();
    render(v, s, 0, &mut out);
    if out.len() > SUMMARY_CHARS {
        let mut cut = SUMMARY_CHARS;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("...");
    }
    out
}

/// Text for a value that could not be read.
pub fn error_text(e: &HopperError) -> String {
    match e {
        HopperError::OptimisedOut => "<optimised out>".into(),
        HopperError::UnreadableMemory { addr, .. } => format!("<unreadable at {addr:#x}>"),
        other => format!("<error: {other}>"),
    }
}

fn render(v: &Value, s: &mut Session<'_>, depth: usize, out: &mut String) {
    if out.len() > SUMMARY_CHARS {
        return;
    }
    if depth > SUMMARY_DEPTH {
        out.push_str("{...}");
        return;
    }
    let sh = match shape(v, s) {
        Ok(sh) => sh,
        Err(e) => {
            out.push_str(&error_text(&e));
            return;
        }
    };
    if let Err(e) = render_shape(v, &sh, s, depth, out) {
        out.push_str(&error_text(&e));
    }
}

fn render_list(items: Vec<Result<Value>>, s: &mut Session<'_>, depth: usize, out: &mut String) {
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        match item {
            Ok(v) => render(&v, s, depth + 1, out),
            Err(e) => out.push_str(&error_text(&e)),
        }
    }
}

fn render_shape(v: &Value, sh: &Shape, s: &mut Session<'_>, depth: usize, out: &mut String) -> Result<()> {
    let ty = s.strip(&v.ty)?;
    match sh {
        Shape::Scalar => out.push_str(&scalar_text(v, &ty, s)?),
        Shape::Pointer { addr, pointee } => {
            out.push_str(&format!("{addr:#x}"));
            let to_fn = pointee
                .as_ref()
                .and_then(|p| s.ty(p).ok())
                .and_then(|t| s.strip(&t).ok())
                .is_some_and(|t| matches!(t.kind, TypeKind::Subroutine));
            if to_fn && let Some(sym) = s.symbols.symbol_at(*addr) {
                out.push_str(&format!(" <{}>", sym.label()));
            }
        }
        Shape::Struct { st, base } => {
            let is_tuple = ty.name.starts_with('(');
            if st.fields.is_empty() {
                out.push_str(&ty.name);
            } else if is_tuple || st.is_tuple_like() {
                if !is_tuple {
                    out.push_str(&ty.name);
                    out.push(' ');
                }
                out.push('(');
                let items = st.fields.iter().map(|f| base.field(s, f)).collect();
                render_list(items, s, depth, out);
                out.push(')');
            } else {
                out.push_str(&ty.name);
                out.push_str(" {");
                render_named_fields(base, st, s, depth, out);
                out.push('}');
            }
        }
        Shape::Enum { active } => match active {
            None => out.push_str(&format!("{}::<invalid discriminant>", ty.name)),
            Some((var, payload)) => {
                out.push_str(&ty.name);
                out.push_str("::");
                out.push_str(&var.name);
                let pty = s.strip(&payload.ty)?;
                if let TypeKind::Struct(st) = &pty.kind
                    && !st.fields.is_empty()
                {
                    let base = Value { ty: pty.clone(), loc: payload.loc.clone() };
                    if st.is_tuple_like() {
                        out.push('(');
                        let items = st.fields.iter().map(|f| base.field(s, f)).collect();
                        render_list(items, s, depth, out);
                        out.push(')');
                    } else {
                        out.push('{');
                        render_named_fields(&base, st, s, depth, out);
                        out.push('}');
                    }
                }
            }
        },
        Shape::Array { elem, len, base } => {
            let size = s.size_of(elem)?;
            out.push('[');
            render_array(base, s, elem, size, *len, depth, out);
            out.push(']');
        }
        Shape::Text { data, len, .. } => {
            let n = (*len).min(TEXT_BYTES);
            let bytes = s.read(*data, n)?;
            let text = String::from_utf8_lossy(&bytes);
            out.push_str(&format!("{text:?}"));
            if *len > n {
                out.push_str("...");
            }
        }
        Shape::Seq { kind, len, .. } => {
            let label = match kind {
                StdKind::Vec => "Vec".to_string(),
                StdKind::VecDeque => "VecDeque".to_string(),
                _ => ty.name.clone(),
            };
            out.push_str(&format!("{label}(size={len})"));
            if *len > 0 {
                out.push_str(" = {");
                let shown = (*len).min(SUMMARY_ELEMENTS as u64);
                let items = (0..shown).map(|i| seq_element(sh, s, i)).collect();
                render_list(items, s, depth, out);
                if *len > shown {
                    out.push_str("...");
                }
                out.push('}');
            }
        }
        Shape::Hash { kind, items, .. } | Shape::BTree { kind, length: items, .. } => {
            render_map(sh, *kind, *items, s, depth, out)?;
        }
        Shape::Shared { kind, strong, weak, value } => {
            let label = match kind {
                StdKind::Rc => "Rc",
                StdKind::Arc => "Arc",
                _ => "Weak",
            };
            match value {
                Some(inner) => {
                    out.push_str(&format!("{label}(strong={strong}, weak={weak}) = {{value = "));
                    render(inner, s, depth + 1, out);
                    out.push_str(&format!(", strong = {strong}, weak = {weak}}}"));
                }
                None => out.push_str(&format!("{label}(dangling)")),
            }
        }
        Shape::Wrapper { kind, value, borrow } => {
            let label = match kind {
                StdKind::Cell => "Cell".to_string(),
                StdKind::RefCell | StdKind::Ref | StdKind::RefMut => {
                    let base = match kind {
                        StdKind::RefCell => "RefCell",
                        StdKind::Ref => "Ref",
                        _ => "RefMut",
                    };
                    match borrow {
                        Some(b) if *b < 0 => format!("{base}(borrow_mut={})", -b),
                        Some(b) => format!("{base}(borrow={b})"),
                        None => base.to_string(),
                    }
                }
                _ => String::new(),
            };
            let value_name = if matches!(kind, StdKind::Ref | StdKind::RefMut) { "*value" } else { "value" };
            out.push_str(&format!("{label} = {{{value_name} = "));
            render(value, s, depth + 1, out);
            if let Some(b) = borrow {
                out.push_str(&format!(", borrow = {b}"));
            }
            out.push('}');
        }
        Shape::Number(n) => render(n, s, depth + 1, out),
    }
    Ok(())
}

fn render_map(sh: &Shape, kind: StdKind, items: u64, s: &mut Session<'_>, depth: usize, out: &mut String) -> Result<()> {
    let label = match kind {
        StdKind::HashMap => "HashMap",
        StdKind::HashSet => "HashSet",
        StdKind::BTreeMap => "BTreeMap",
        _ => "BTreeSet",
    };
    out.push_str(&format!("{label}(size={items})"));
    let n = child_count(sh, s).unwrap_or(0);
    if n == 0 {
        return Ok(());
    }
    out.push_str(" = {");
    let shown = n.min(SUMMARY_ELEMENTS as u64);
    let is_map = matches!(kind, StdKind::HashMap | StdKind::BTreeMap);
    for i in 0..shown {
        if i > 0 {
            out.push_str(", ");
        }
        let c = child_at(sh, s, "", i)?;
        if is_map {
            out.push_str(&c.name);
            out.push_str(" = ");
        }
        match c.value {
            Ok(cv) => render(&cv, s, depth + 1, out),
            Err(e) => out.push_str(&error_text(&e)),
        }
    }
    if n > shown {
        out.push_str("...");
    }
    out.push('}');
    Ok(())
}

fn render_named_fields(base: &Value, st: &StructType, s: &mut Session<'_>, depth: usize, out: &mut String) {
    for (i, f) in st.fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(f.display_name());
        out.push_str(": ");
        match base.field(s, f) {
            Ok(fv) => render(&fv, s, depth + 1, out),
            Err(e) => out.push_str(&error_text(&e)),
        }
    }
}

/// Array elements, collapsing runs of ten or more equal elements as gdb does
/// (`0 <repeats 16 times>`).
fn render_array(v: &Value, s: &mut Session<'_>, elem: &TypeId, size: u64, len: u64, depth: usize, out: &mut String) {
    let shown = len.min(SUMMARY_ELEMENTS as u64);
    let mut texts = Vec::with_capacity(shown as usize);
    for i in 0..shown {
        let mut t = String::new();
        match v.slice(s, i * size, elem) {
            Ok(e) => render(&e, s, depth + 1, &mut t),
            Err(e) => t.push_str(&error_text(&e)),
        }
        texts.push(t);
    }
    let mut i = 0;
    let mut first = true;
    while i < texts.len() {
        let mut j = i + 1;
        while j < texts.len() && texts[j] == texts[i] {
            j += 1;
        }
        let run = j - i;
        if !first {
            out.push_str(", ");
        }
        first = false;
        if run >= 10 {
            out.push_str(&format!("{} <repeats {run} times>", texts[i]));
            i = j;
        } else {
            out.push_str(&texts[i]);
            i += 1;
        }
    }
    if len > shown {
        out.push_str("...");
    }
}

/// A scalar as gdb prints it.
pub fn scalar_text(v: &Value, ty: &Type, s: &mut Session<'_>) -> Result<String> {
    let size = v.size(s)?;
    Ok(match &ty.kind {
        TypeKind::Base(kind) => {
            if size == 0 {
                return Ok("()".into());
            }
            match kind {
                BaseKind::Bool => (if v.as_u64(s)? != 0 { "true" } else { "false" }).to_string(),
                BaseKind::Char | BaseKind::SignedChar | BaseKind::UnsignedChar => {
                    let code = v.as_u64(s)?;
                    let shown = if matches!(kind, BaseKind::SignedChar) { v.as_i128(s)?.to_string() } else { code.to_string() };
                    format!("{shown} {}", char_literal(code as u32))
                }
                BaseKind::Signed => v.as_i128(s)?.to_string(),
                BaseKind::Unsigned => v.as_i128(s)?.to_string(),
                BaseKind::Float => {
                    let b = v.bytes(s)?;
                    match size {
                        4 => f32::from_le_bytes(b[..4].try_into().expect("4 bytes")).to_string(),
                        8 => f64::from_le_bytes(b[..8].try_into().expect("8 bytes")).to_string(),
                        _ => format!("<float{}>", size * 8),
                    }
                }
            }
        }
        TypeKind::CEnum(e) => {
            let x = v.as_i128(s)?;
            match e.enumerators.iter().find(|(_, val)| *val == x) {
                Some((name, _)) => format!("{}::{name}", ty.name),
                None => x.to_string(),
            }
        }
        TypeKind::Void => "()".into(),
        TypeKind::Subroutine => format!("<{}>", ty.name),
        _ => format!("<{}>", ty.name),
    })
}

/// `'a'`, `'\n'`, `'\u{0003bb}'`: gdb's Rust character literal.
pub fn char_literal(code: u32) -> String {
    match code {
        0x27 => "'\\''".into(),
        0x5c => "'\\\\'".into(),
        0x0a => "'\\n'".into(),
        0x09 => "'\\t'".into(),
        0x0d => "'\\r'".into(),
        0x00 => "'\\0'".into(),
        0x20..=0x7e => format!("'{}'", code as u8 as char),
        _ => format!("'\\u{{{code:06x}}}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_literals_match_gdb() {
        assert_eq!(char_literal('a' as u32), "'a'");
        assert_eq!(char_literal('λ' as u32), "'\\u{0003bb}'");
        assert_eq!(char_literal('\n' as u32), "'\\n'");
        assert_eq!(char_literal('\'' as u32), "'\\''");
    }
}
