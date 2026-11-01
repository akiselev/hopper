//! hopper's value evaluator.
//!
//! Given a [`SymbolSet`], a `&mut dyn Memory`, and one frame's registers, CFA and PC
//! ([`EvalContext`]), the [`Evaluator`] lists the frame's locals and evaluates expressions, each
//! as a [`ValueView`]: a one-line `summary` in rust-gdb's format, the `type_name`, a
//! `child_count`, the [`Place`] the bytes are in, and a [`ValueHandle`] whose `path` of
//! [`PathElem`]s re-selects the value from its root expression, so the engine can rebuild the
//! same value at another moment. Children are paged ([`Evaluator::children`]).
//!
//! The layers, bottom up:
//! - [`location`]: DWARF expressions and location lists (gimli's `Evaluation`).
//! - [`types`]: the type graph (base, struct, union, Rust enum with niches, C enum, pointer,
//!   array, subroutine, typedef and qualifiers).
//! - [`value`]: a type plus a [`ValueLocation`], read through a page-caching [`value::Session`].
//! - [`layouts`]: the data-driven Rust std layout table, keyed on rustc version.
//! - [`view`]: shapes, children, paths and summaries.
//! - [`expr`]: the expression language.

pub mod expr;
pub mod layouts;
pub mod location;
pub mod types;
pub mod value;
pub mod view;

use hopper_core::api::{
    OwnershipState, PageReq, PathElem, TypeKey, ValueHandle, ValueTarget, ValueView,
};
use hopper_core::target::Memory;
use hopper_core::{FrameId, HopperError, Moment, Place, ProcessId, RegisterSet, Result};
use hopper_symbols::{DieRef, FrameScope, ModuleId, StackFrame, SymbolSet};

pub use location::ValueLocation;
pub use types::{TypeCache, TypeId};
pub use value::{Session, Value};

use crate::expr::Expr;
use crate::location::FrameContext;

/// One frame, as the evaluator needs it.
#[derive(Clone, Debug)]
pub struct FrameRef<'a> {
    pub regs: &'a RegisterSet,
    pub cfa: Option<u64>,
    /// The PC to look up scopes, location lists and modules at: the frame's PC for the innermost
    /// frame, one byte before the return address for callers (see
    /// `PhysicalFrame::lookup_pc`).
    pub lookup_pc: u64,
    /// The source frame's DIE (subprogram or inlined subroutine); `None` for the innermost
    /// source frame at `lookup_pc`.
    pub die: Option<DieRef>,
    /// The frame's id, used for register [`Place`]s and value handles.
    pub id: Option<FrameId>,
}

impl<'a> FrameRef<'a> {
    pub fn from_stack(frame: &'a StackFrame) -> FrameRef<'a> {
        FrameRef {
            regs: &frame.regs,
            cfa: frame.cfa,
            lookup_pc: frame.lookup_pc,
            die: frame.die,
            id: Some(frame.frame.id),
        }
    }
}

/// Everything one evaluation reads.
pub struct EvalContext<'a> {
    pub symbols: &'a SymbolSet,
    pub memory: &'a mut dyn Memory,
    pub frame: FrameRef<'a>,
    /// The moment the memory and registers belong to; copied into every handle.
    pub at: Moment,
    pub process: ProcessId,
}

impl std::fmt::Debug for EvalContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalContext").field("frame", &self.frame).field("at", &self.at).finish()
    }
}

/// A value with the facts a caller may need beyond its view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    pub value: Value,
    pub module: ModuleId,
    pub type_key: TypeKey,
    pub place: Option<Place>,
}

/// The evaluator. Holds the parsed-type cache; share one across evaluations and threads.
#[derive(Debug, Default)]
pub struct Evaluator {
    types: TypeCache,
}

/// A frame's scope plus its frame base, ready for variable reads.
struct FrameEnv {
    module: ModuleId,
    scope: Option<FrameScope>,
    frame_base: Option<u64>,
    bias: u64,
    file_pc: u64,
}

impl Evaluator {
    pub fn new() -> Evaluator {
        Evaluator::default()
    }

    /// The frame's parameters and locals, in declaration order. A variable that cannot be read
    /// (optimised out, unreadable memory) is still listed, with its reason as the summary.
    pub fn locals(&self, cx: &mut EvalContext<'_>) -> Result<Vec<ValueView>> {
        let env = self.frame_env(cx)?;
        let scope = env
            .scope
            .clone()
            .ok_or_else(|| HopperError::NotFound(format!("no debug info for {:#x}", cx.frame.lookup_pc)))?;
        let mut s = Session::new(cx.symbols, env.module, &self.types, &mut *cx.memory, cx.frame.regs, scope.rustc_version)?;
        let mut out = Vec::new();
        for var in &scope.variables {
            let loaded = load_variable(&mut s, &env, cx.frame.regs, cx.frame.cfa, var.die, var.type_die);
            let shadowed = scope.is_shadowed(var);
            let target = |v: Option<&Value>, s: &Session<'_>| match (shadowed, v) {
                (true, Some(v)) if v.address().is_some() => ValueTarget::Place {
                    place: place_of(v, s, cx.process, cx.frame.id).expect("memory value has a place"),
                    ty: v.ty.id.key(s.module),
                },
                _ => ValueTarget::Expr(var.name.clone()),
            };
            let view = match loaded {
                Ok(v) => {
                    let handle = handle(cx, target(Some(&v), &s), Vec::new());
                    view_of(&mut s, &v, var.name.clone(), handle, cx.process, cx.frame.id)
                }
                Err(e) => {
                    let type_name = var
                        .type_die
                        .and_then(|t| s.ty(&TypeId::Die(t)).ok())
                        .map_or_else(|| "?".to_string(), |t| t.name.clone());
                    ValueView {
                        handle: handle(cx, target(None, &s), Vec::new()),
                        name: var.name.clone(),
                        type_name,
                        summary: view::error_text(&e),
                        child_count: None,
                        place: None,
                        ownership: OwnershipState::Unknown,
                    }
                }
            };
            out.push(view);
        }
        Ok(out)
    }

    /// Evaluate `expr` in the frame.
    pub fn eval(&self, cx: &mut EvalContext<'_>, expr: &str) -> Result<ValueView> {
        let handle = handle(cx, ValueTarget::Expr(expr.to_string()), Vec::new());
        self.view(cx, &handle)
    }

    /// The value a handle names, now (at `cx.at`).
    pub fn view(&self, cx: &mut EvalContext<'_>, h: &ValueHandle) -> Result<ValueView> {
        let (env, rustc) = self.env_for_handle(cx, h)?;
        let mut s = Session::new(cx.symbols, env.module, &self.types, &mut *cx.memory, cx.frame.regs, rustc)?;
        let root = root_value(&mut s, &env, cx.frame.regs, cx.frame.cfa, cx.process, &h.target)?;
        let v = view::select_path(&root, &mut s, &h.path)?;
        let name = display_name(&h.target, &h.path);
        let mut handle = h.clone();
        handle.at = cx.at;
        Ok(view_of(&mut s, &v, name, handle, cx.process, cx.frame.id))
    }

    /// A page of the children of the value a handle names.
    pub fn children(&self, cx: &mut EvalContext<'_>, h: &ValueHandle, page: PageReq) -> Result<Vec<ValueView>> {
        let (env, rustc) = self.env_for_handle(cx, h)?;
        let mut s = Session::new(cx.symbols, env.module, &self.types, &mut *cx.memory, cx.frame.regs, rustc)?;
        let root = root_value(&mut s, &env, cx.frame.regs, cx.frame.cfa, cx.process, &h.target)?;
        let v = view::select_path(&root, &mut s, &h.path)?;
        let parent = display_name(&h.target, &h.path);
        let sh = view::shape(&v, &mut s)?;
        let kids = view::children(&sh, &mut s, &parent, page.offset, page.limit as u64)?;
        let mut out = Vec::with_capacity(kids.len());
        for c in kids {
            let mut path = h.path.clone();
            path.extend(c.path.iter().cloned());
            let mut ch = h.clone();
            ch.at = cx.at;
            ch.path = path;
            out.push(match c.value {
                Ok(cv) => view_of(&mut s, &cv, c.name, ch, cx.process, cx.frame.id),
                Err(e) => ValueView {
                    handle: ch,
                    name: c.name,
                    type_name: "?".into(),
                    summary: view::error_text(&e),
                    child_count: None,
                    place: None,
                    ownership: OwnershipState::Unknown,
                },
            });
        }
        Ok(out)
    }

    /// The raw value a handle names, with its module, type key and place.
    pub fn evaluate(&self, cx: &mut EvalContext<'_>, h: &ValueHandle) -> Result<Evaluated> {
        let (env, rustc) = self.env_for_handle(cx, h)?;
        let mut s = Session::new(cx.symbols, env.module, &self.types, &mut *cx.memory, cx.frame.regs, rustc)?;
        let root = root_value(&mut s, &env, cx.frame.regs, cx.frame.cfa, cx.process, &h.target)?;
        let v = view::select_path(&root, &mut s, &h.path)?;
        let place = place_of(&v, &s, cx.process, cx.frame.id);
        Ok(Evaluated { type_key: v.ty.id.key(env.module), module: env.module, place, value: v })
    }

    /// The bytes of the value a handle names.
    pub fn bytes(&self, cx: &mut EvalContext<'_>, h: &ValueHandle) -> Result<Vec<u8>> {
        let (env, rustc) = self.env_for_handle(cx, h)?;
        let mut s = Session::new(cx.symbols, env.module, &self.types, &mut *cx.memory, cx.frame.regs, rustc)?;
        let root = root_value(&mut s, &env, cx.frame.regs, cx.frame.cfa, cx.process, &h.target)?;
        let v = view::select_path(&root, &mut s, &h.path)?;
        Ok(v.bytes(&mut s)?.to_vec())
    }

    fn frame_env(&self, cx: &mut EvalContext<'_>) -> Result<FrameEnv> {
        let pc = cx.frame.lookup_pc;
        let (module, m) = cx
            .symbols
            .module_at(pc)
            .ok_or_else(|| HopperError::NotFound(format!("no module contains {pc:#x}")))?;
        let bias = m.bias();
        let file_pc = m.to_file(pc);
        let scope = cx.symbols.scope_at(pc, cx.frame.die).ok();
        let frame_base = match &scope {
            Some(sc) => {
                let dwarf = m.binary().dwarf()?;
                let fctx = FrameContext { regs: cx.frame.regs, cfa: cx.frame.cfa, frame_base: None, bias, file_pc };
                location::frame_base(dwarf, sc.subprogram, &fctx, &mut *cx.memory).ok().flatten()
            }
            None => None,
        };
        Ok(FrameEnv { module, scope, frame_base, bias, file_pc })
    }

    fn env_for_handle(
        &self,
        cx: &mut EvalContext<'_>,
        h: &ValueHandle,
    ) -> Result<(FrameEnv, Option<hopper_symbols::RustcVersion>)> {
        if let ValueTarget::Place { ty, .. } = &h.target {
            let (module, _) = TypeId::from_key(*ty).ok_or_else(|| HopperError::Invalid(format!("bad type key {ty:?}")))?;
            let m = cx.symbols.module(module).ok_or_else(|| HopperError::Invalid(format!("no module {module:?}")))?;
            let rustc = m.binary().dwarf().ok().and_then(|d| d.units().iter().find_map(|u| u.rustc_version));
            let env = FrameEnv { module, scope: None, frame_base: None, bias: m.bias(), file_pc: 0 };
            return Ok((env, rustc));
        }
        let env = self.frame_env(cx)?;
        let rustc = env.scope.as_ref().and_then(|s| s.rustc_version).or_else(|| {
            cx.symbols
                .module(env.module)?
                .binary()
                .dwarf()
                .ok()?
                .units()
                .iter()
                .find_map(|u| u.rustc_version)
        });
        Ok((env, rustc))
    }
}

fn handle(cx: &EvalContext<'_>, target: ValueTarget, path: Vec<PathElem>) -> ValueHandle {
    ValueHandle { at: cx.at, process: cx.process, frame: cx.frame.id, target, path }
}

fn display_name(target: &ValueTarget, path: &[PathElem]) -> String {
    let mut name = match target {
        ValueTarget::Expr(e) => e.clone(),
        ValueTarget::Place { place, .. } => match place {
            Place::Memory { addr, .. } => format!("{{{addr:#x}}}"),
            Place::Register { reg, .. } => format!("${}", reg.name()),
        },
    };
    let mut iter = path.iter().peekable();
    while let Some(e) = iter.next() {
        name = match e {
            PathElem::Field(f) => {
                if iter.peek().is_none() {
                    f.clone()
                } else {
                    format!("{name}.{f}")
                }
            }
            PathElem::Index(i) => {
                if iter.peek().is_none() {
                    format!("[{i}]")
                } else {
                    format!("{name}[{i}]")
                }
            }
            PathElem::Deref => format!("*{name}"),
            PathElem::Variant(_) => name,
        };
    }
    name
}

fn place_of(v: &Value, s: &Session<'_>, process: ProcessId, frame: Option<FrameId>) -> Option<Place> {
    match &v.loc {
        ValueLocation::Memory { addr } => {
            let len = v.size(s).ok()?;
            Some(Place::Memory { process, addr: *addr, len })
        }
        ValueLocation::Register { reg } => Some(Place::Register { frame: frame?, reg: *reg }),
        ValueLocation::Bytes(_) | ValueLocation::OptimisedOut => None,
    }
}

fn view_of(
    s: &mut Session<'_>,
    v: &Value,
    name: String,
    handle: ValueHandle,
    process: ProcessId,
    frame: Option<FrameId>,
) -> ValueView {
    let summary = view::summary(v, s);
    let child_count = view::shape(v, s).ok().and_then(|sh| view::child_count(&sh, s));
    ValueView {
        handle,
        name,
        type_name: v.ty.name.clone(),
        summary,
        child_count,
        place: place_of(v, s, process, frame),
        ownership: OwnershipState::Unknown,
    }
}

fn load_variable(
    s: &mut Session<'_>,
    env: &FrameEnv,
    regs: &RegisterSet,
    cfa: Option<u64>,
    die: DieRef,
    type_die: Option<DieRef>,
) -> Result<Value> {
    let ty = s.ty(&type_die.map_or(TypeId::Builtin(types::Builtin::Unknown), TypeId::Die))?;
    let binary = s.binary.clone();
    let dwarf = binary.dwarf()?;
    let fctx = FrameContext { regs, cfa, frame_base: env.frame_base, bias: env.bias, file_pc: env.file_pc };
    let loc = location::variable_location(dwarf, die, &fctx, s.memory())?;
    if loc == ValueLocation::OptimisedOut {
        return Err(HopperError::OptimisedOut);
    }
    Ok(Value { ty, loc })
}

fn root_value(
    s: &mut Session<'_>,
    env: &FrameEnv,
    regs: &RegisterSet,
    cfa: Option<u64>,
    process: ProcessId,
    target: &ValueTarget,
) -> Result<Value> {
    match target {
        ValueTarget::Expr(text) => {
            let e = expr::parse(text)?;
            eval_expr(s, env, regs, cfa, &e)
        }
        ValueTarget::Place { place, ty } => {
            let (_, id) = TypeId::from_key(*ty).ok_or_else(|| HopperError::Invalid(format!("bad type key {ty:?}")))?;
            match place {
                Place::Memory { process: p, addr, .. } => {
                    if *p != process {
                        return Err(HopperError::Invalid("the place belongs to another process".into()));
                    }
                    s.at(&id, *addr)
                }
                Place::Register { reg, .. } => Ok(Value { ty: s.ty(&id)?, loc: ValueLocation::Register { reg: *reg } }),
            }
        }
    }
}

/// How many times `.field` looks through a pointer, Box, Rc or reference, as Rust's auto-deref.
const AUTO_DEREF: usize = 4;

fn eval_expr(s: &mut Session<'_>, env: &FrameEnv, regs: &RegisterSet, cfa: Option<u64>, e: &Expr) -> Result<Value> {
    match e {
        Expr::Name(name) => {
            if let Some(var) = env.scope.as_ref().and_then(|sc| sc.lookup(name)) {
                return load_variable(s, env, regs, cfa, var.die, var.type_die);
            }
            resolve_static(s, env, regs, cfa, name)
        }
        Expr::Int(n) => {
            let v = i64::try_from(*n).map_err(|_| HopperError::Eval(format!("{n} does not fit in 64 bits")))?;
            Ok(s.integer(v))
        }
        Expr::Negate(inner) => {
            let v = eval_expr(s, env, regs, cfa, inner)?;
            let n = integer_of(&v, s)?;
            Ok(s.integer(i64::try_from(-n).map_err(|_| HopperError::Eval("overflow".into()))?))
        }
        Expr::Field(base, field) => {
            let mut cur = eval_expr(s, env, regs, cfa, base)?;
            for _ in 0..=AUTO_DEREF {
                match view::select(&cur, s, &[PathElem::Field(field.clone())]) {
                    Ok((v, _)) => return Ok(v),
                    Err(err) => match view::select(&cur, s, &[PathElem::Deref]) {
                        Ok((inner, _)) => cur = inner,
                        Err(_) => return Err(err),
                    },
                }
            }
            Err(HopperError::Eval(format!("no field {field}")))
        }
        Expr::Index(base, index) => {
            let b = eval_expr(s, env, regs, cfa, base)?;
            let idx = eval_expr(s, env, regs, cfa, index)?;
            let i = integer_of(&idx, s)?;
            if let view::Shape::Pointer { pointee: Some(p), addr } = view::shape(&b, s)? {
                // Raw pointer arithmetic: `p[i]` is `*(p + i)`.
                let size = s.size_of(&p)? as i128;
                let target = (addr as i128 + i * size) as u64;
                return s.at(&p, target);
            }
            let i = u64::try_from(i).map_err(|_| HopperError::Eval(format!("negative index {i}")))?;
            let mut cur = b;
            for _ in 0..=AUTO_DEREF {
                match view::select(&cur, s, &[PathElem::Index(i)]) {
                    Ok((v, _)) => return Ok(v),
                    Err(err) => match view::select(&cur, s, &[PathElem::Deref]) {
                        Ok((inner, _)) => cur = inner,
                        Err(_) => return Err(err),
                    },
                }
            }
            Err(HopperError::Eval(format!("cannot index with {i}")))
        }
        Expr::Deref(inner) => {
            let v = eval_expr(s, env, regs, cfa, inner)?;
            Ok(view::select(&v, s, &[PathElem::Deref])?.0)
        }
        Expr::AddressOf(inner) => {
            let v = eval_expr(s, env, regs, cfa, inner)?;
            let addr = v
                .address()
                .ok_or_else(|| HopperError::Eval("cannot take the address of a value not in memory".into()))?;
            let id = TypeId::PointerTo(Box::new(v.ty.id.clone()));
            s.from_bytes(&id, addr.to_le_bytes().to_vec())
        }
    }
}

fn integer_of(v: &Value, s: &mut Session<'_>) -> Result<i128> {
    let ty = s.strip(&v.ty)?;
    match ty.kind {
        types::TypeKind::Base(_) | types::TypeKind::CEnum(_) => v.as_i128(s),
        _ => Err(HopperError::Eval(format!("{} is not an integer", ty.name))),
    }
}

fn resolve_static(s: &mut Session<'_>, env: &FrameEnv, regs: &RegisterSet, cfa: Option<u64>, name: &str) -> Result<Value> {
    let hits = s.symbols.find_statics(name);
    // Prefer the frame's own module.
    let (module, die) = hits
        .iter()
        .find(|(m, _)| *m == env.module)
        .or_else(|| hits.first())
        .copied()
        .ok_or_else(|| HopperError::NotFound(format!("no variable or static named {name}")))?;
    if module != s.module {
        return Err(HopperError::Unsupported(format!("{name} is a static of another module")));
    }
    let binary = s.binary.clone();
    let dwarf = binary.dwarf()?;
    let (unit, entry) = dwarf.entry(die)?;
    let type_die = entry
        .attr_value(gimli::DW_AT_type)
        .and_then(|v| dwarf.attr_ref(unit, v))
        .or_else(|| {
            let spec = entry.attr_value(gimli::DW_AT_specification).and_then(|v| dwarf.attr_ref(unit, v))?;
            let (su, se) = dwarf.entry(spec).ok()?;
            se.attr_value(gimli::DW_AT_type).and_then(|v| dwarf.attr_ref(su, v))
        });
    load_variable(s, env, regs, cfa, die, type_die)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_follow_paths() {
        let t = ValueTarget::Expr("v".into());
        assert_eq!(display_name(&t, &[]), "v");
        assert_eq!(display_name(&t, &[PathElem::Index(0)]), "[0]");
        assert_eq!(display_name(&t, &[PathElem::Index(0), PathElem::Field("name".into())]), "name");
        assert_eq!(display_name(&t, &[PathElem::Deref]), "*v");
        assert_eq!(
            display_name(&t, &[PathElem::Variant("Some".into()), PathElem::Field("0".into())]),
            "0"
        );
    }
}
