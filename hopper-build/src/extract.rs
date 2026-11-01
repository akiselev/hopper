//! Walking the optimised MIR of every function instance a crate codegens, and turning it into
//! sidecar events.
//!
//! Which bodies: the instances of `collect_and_partition_mono_items`, the same list codegen
//! lowers, each through `instance_mir` (the final optimised MIR) with its generic arguments
//! substituted. That is exact for what the binary contains, including std generics instantiated
//! here (`Vec::<Config>::push`) and drop glue; they are recorded with their origin so the join can
//! skip them in favour of the user's call site.
//!
//! Events per MIR construct:
//! - `Operand::Move` / `Operand::Copy` / constants → Move / Copy / Literal, wherever an operand
//!   is used as a value (assignments, casts, aggregate fields, call arguments);
//! - `Rvalue::Ref` / `RawPtr` → SharedBorrow or MutBorrow, preceded by Projection when the
//!   borrowed place has projections;
//! - `Rvalue::Aggregate` → Aggregate, then one operand event per field into `dst.field`;
//! - `Call` → one operand event per argument, then Clone when the callee is `<T as Clone>::clone`
//!   and CallReturn otherwise;
//! - `Drop` → Drop; `StorageDead` of a user variable → StorageDead.

use std::collections::{HashMap, HashSet};

use hopper_sidecar::{
    Caveat, ControlPath, FunctionOrigin, FunctionRecord, InlineFrame, LineCol, MirLocation, MirPosition,
    OwnershipEvent, Span, TypeInfo,
};
use hopper_core::api::OwnershipKind;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_hir::{self as hir, LangItem};
use rustc_middle::hir::nested_filter;
use rustc_middle::mir::{
    self, AggregateKind, BasicBlock, BorrowKind, Body, Local, Operand, Place, PlaceTy, ProjectionElem,
    RawPtrKind, Rvalue, SourceInfo, SourceScope, StatementKind, TerminatorKind, VarDebugInfoContents,
};
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::print::{with_forced_trimmed_paths, with_no_trimmed_paths};
use rustc_middle::ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypingEnv};
use rustc_span::{BytePos, Pos, RemapPathScopeComponents, Span as RSpan, hygiene};

/// Every function instance this crate codegens, as sidecar records sorted by symbol.
pub fn crate_functions(tcx: TyCtxt<'_>) -> Vec<FunctionRecord> {
    let partitions = tcx.collect_and_partition_mono_items(());
    let mut seen = HashMap::new();
    let mut files = FileNames::default();
    let mut safety = SafetyCache::default();
    let mut out = Vec::new();
    for cgu in partitions.codegen_units {
        for item in cgu.items().keys() {
            let MonoItem::Fn(instance) = *item else { continue };
            let symbol = tcx.symbol_name(instance).name.to_string();
            if seen.insert(symbol.clone(), ()).is_some() {
                continue;
            }
            out.push(FnWalker::new(tcx, instance, symbol, &mut files, &mut safety).walk());
        }
    }
    out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    out
}

// ---------------------------------------------------------------- spans and files

/// File names as the DWARF line table names them: the debuginfo-scope (possibly remapped) path,
/// joined with its working directory when relative. Cached per source file.
#[derive(Default)]
struct FileNames {
    by_start: HashMap<BytePos, String>,
}

impl FileNames {
    fn name(&mut self, sf: &rustc_span::SourceFile) -> String {
        self.by_start
            .entry(sf.start_pos)
            .or_insert_with(|| match &sf.name {
                rustc_span::FileName::Real(real) => {
                    let (wd, name) = real.embeddable_name(RemapPathScopeComponents::DEBUGINFO);
                    if name.is_absolute() || wd.as_os_str().is_empty() {
                        name.to_string_lossy().into_owned()
                    } else {
                        wd.join(name).to_string_lossy().into_owned()
                    }
                }
                other => other.display(RemapPathScopeComponents::DEBUGINFO).to_string(),
            })
            .clone()
    }
}

/// Per-definition `unsafe` regions, computed from HIR.
#[derive(Default)]
struct SafetyCache {
    regions: HashMap<DefId, Option<UnsafeRegions>>,
}

struct UnsafeRegions {
    /// The whole body is an unsafe context (`unsafe fn`).
    whole_body: bool,
    blocks: Vec<RSpan>,
}

impl SafetyCache {
    /// The unsafe regions of `def_id`'s body; `None` when it has no local HIR (another crate's
    /// code), which the caller treats as unsafe.
    fn of(&mut self, tcx: TyCtxt<'_>, def_id: DefId) -> Option<&UnsafeRegions> {
        self.regions
            .entry(def_id)
            .or_insert_with(|| {
                // Closures sit inside their parent's body, and an enclosing `unsafe` block covers
                // them, so read the typeck root's body with nested bodies included.
                let root = tcx.typeck_root_def_id(def_id).as_local()?;
                let body = tcx.hir_maybe_body_owned_by(root)?;
                let whole_body = matches!(tcx.def_kind(root), DefKind::Fn | DefKind::AssocFn)
                    && tcx.hir_node_by_def_id(root).fn_sig().is_some_and(|s| s.header.is_unsafe());
                let mut v = UnsafeBlocks { tcx, blocks: Vec::new() };
                v.visit_body(body);
                Some(UnsafeRegions { whole_body, blocks: v.blocks })
            })
            .as_ref()
    }
}

struct UnsafeBlocks<'tcx> {
    tcx: TyCtxt<'tcx>,
    blocks: Vec<RSpan>,
}

impl<'tcx> Visitor<'tcx> for UnsafeBlocks<'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;

    fn maybe_tcx(&mut self) -> Self::MaybeTyCtxt {
        self.tcx
    }

    fn visit_block(&mut self, b: &'tcx hir::Block<'tcx>) {
        if let hir::BlockCheckMode::UnsafeBlock(_) = b.rules {
            self.blocks.push(b.span);
        }
        intravisit::walk_block(self, b);
    }
}

// ---------------------------------------------------------------- one body

/// What a compiler temporary holds, when it is assigned once: used to render places through
/// temporaries the way the source wrote them (`(*_8).name` → `v[0].name`).
#[derive(Clone)]
enum TempValue<'tcx> {
    /// `&place` or `&raw place`.
    Ref(Place<'tcx>),
    /// A copy or move of a place.
    Use(Place<'tcx>),
    /// `Index::index(&base, idx)` / `IndexMut::index_mut`: a reference to `base[idx]`.
    Index(Operand<'tcx>, Operand<'tcx>),
    /// `Deref::deref(&x)` / `DerefMut::deref_mut`: a reference to `*x`.
    Deref(Operand<'tcx>),
    /// A constant.
    Const(String),
}

/// A rendered place: a place expression, or a reference to one.
enum Rendered {
    Place(String),
    Ref(String),
}

impl Rendered {
    fn text(self) -> String {
        match self {
            Rendered::Place(s) => s,
            Rendered::Ref(s) => format!("&{s}"),
        }
    }

    /// The place this value points to when dereferenced.
    fn deref(self) -> String {
        match self {
            Rendered::Ref(s) => s,
            Rendered::Place(s) => format!("*{s}"),
        }
    }
}

/// Facts about a call's callee.
struct Callee {
    /// The def path with generic arguments, no trimmed paths (`std::vec::Vec::<Config>::push`).
    path: String,
    /// The same, with trimmed paths for display (`Vec::<Config>::push`).
    short: String,
    role: CalleeKind,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum CalleeKind {
    Clone,
    Index,
    Deref,
    Other,
}

const MAX_RENDER_DEPTH: u32 = 8;

struct FnWalker<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    instance: Instance<'tcx>,
    symbol: String,
    body: &'tcx Body<'tcx>,
    env: TypingEnv<'tcx>,
    names: HashMap<Local, String>,
    /// Debuginfo names bound to places with projections (closure captures), longest first.
    projected_names: Vec<(Place<'tcx>, String)>,
    temps: HashMap<Local, TempValue<'tcx>>,
    files: &'a mut FileNames,
    safety: &'a mut SafetyCache,
    /// Blocks entered only through a drop-flag test.
    flag_guarded: HashSet<BasicBlock>,
    /// The control path of the block being walked.
    path: ControlPath,
    events: Vec<OwnershipEvent>,
}

impl<'a, 'tcx> FnWalker<'a, 'tcx> {
    fn new(
        tcx: TyCtxt<'tcx>,
        instance: Instance<'tcx>,
        symbol: String,
        files: &'a mut FileNames,
        safety: &'a mut SafetyCache,
    ) -> Self {
        let body = tcx.instance_mir(instance.def);
        let mut names = HashMap::new();
        let mut projected_names = Vec::new();
        for vdi in &body.var_debug_info {
            if let VarDebugInfoContents::Place(p) = vdi.value
                && vdi.composite.is_none()
            {
                if p.projection.is_empty() {
                    names.entry(p.local).or_insert_with(|| vdi.name.to_string());
                } else {
                    projected_names.push((p, vdi.name.to_string()));
                }
            }
        }
        projected_names.sort_by_key(|(p, _)| std::cmp::Reverse(p.projection.len()));
        let mut w = FnWalker {
            tcx,
            instance,
            symbol,
            body,
            env: TypingEnv::fully_monomorphized(),
            names,
            projected_names,
            temps: HashMap::new(),
            files,
            safety,
            flag_guarded: HashSet::new(),
            path: ControlPath::Normal,
            events: Vec::new(),
        };
        w.temps = w.single_assignment_temps();
        w.flag_guarded = w.drop_flag_guarded_blocks();
        w
    }

    /// Drop elaboration guards a drop of a maybe-moved local with a drop flag: a bool temporary
    /// only ever assigned constants, tested by a `switchInt`. The blocks that test leads to run
    /// the drop only while the value is still owned.
    fn drop_flag_guarded_blocks(&self) -> HashSet<BasicBlock> {
        let mut assigned_const: HashMap<Local, bool> = HashMap::new();
        for data in self.body.basic_blocks.iter() {
            for stmt in &data.statements {
                if let StatementKind::Assign(assign) = &stmt.kind
                    && assign.0.projection.is_empty()
                {
                    let only_const = matches!(assign.1, Rvalue::Use(Operand::Constant(_)));
                    let e = assign_const_entry(&mut assigned_const, assign.0.local);
                    *e &= only_const;
                }
            }
        }
        let is_flag = |l: Local| {
            assigned_const.get(&l) == Some(&true)
                && !self.names.contains_key(&l)
                && self.body.local_decls[l].ty.is_bool()
        };
        let mut out = HashSet::new();
        for data in self.body.basic_blocks.iter() {
            if let Some(term) = &data.terminator
                && let TerminatorKind::SwitchInt { discr, targets } = &term.kind
                && let Some(p) = discr.place()
                && p.projection.is_empty()
                && is_flag(p.local)
            {
                for t in targets.all_targets() {
                    if self.body.basic_blocks[*t]
                        .terminator
                        .as_ref()
                        .is_some_and(|t| matches!(t.kind, TerminatorKind::Drop { .. }))
                    {
                        out.insert(*t);
                    }
                }
            }
        }
        out
    }

    fn mono<T: ty::TypeFoldable<TyCtxt<'tcx>>>(&self, v: T) -> T {
        self.instance.instantiate_mir_and_normalize_erasing_regions(self.tcx, self.env, EarlyBinder::bind(v))
    }

    fn origin(&self) -> FunctionOrigin {
        match self.instance.def {
            InstanceKind::Item(def) if !def.is_local() => FunctionOrigin::Dependency,
            InstanceKind::Item(def) if self.tcx.def_span(def).from_expansion() => {
                FunctionOrigin::MacroExpansion
            }
            InstanceKind::Item(_) => FunctionOrigin::Source,
            _ => FunctionOrigin::Shim,
        }
    }

    fn walk(mut self) -> FunctionRecord {
        let tcx = self.tcx;
        for (bb, data) in self.body.basic_blocks.iter_enumerated() {
            self.path = if data.is_cleanup {
                ControlPath::Unwind
            } else if self.flag_guarded.contains(&bb) {
                ControlPath::DropFlagGuarded
            } else {
                ControlPath::Normal
            };
            for (i, stmt) in data.statements.iter().enumerate() {
                let loc = MirLocation { block: bb.as_u32(), position: MirPosition::Statement(i as u32) };
                self.statement(&stmt.kind, stmt.source_info, loc);
            }
            if let Some(term) = &data.terminator {
                let loc = MirLocation { block: bb.as_u32(), position: MirPosition::Terminator };
                self.terminator(&term.kind, term.source_info, loc);
            }
        }
        let def_id = self.instance.def_id();
        let def_span = self.span(tcx.def_span(def_id));
        FunctionRecord {
            demangled: format!("{:#}", rustc_demangle::demangle(&self.symbol)),
            symbol: self.symbol.clone(),
            def_path: with_no_trimmed_paths!(tcx.def_path_str_with_args(def_id, self.instance.args)),
            krate: tcx.crate_name(def_id.krate).to_string(),
            def_span,
            origin: self.origin(),
            events: std::mem::take(&mut self.events),
        }
    }

    // ------------------------------------------------------------ statements

    fn statement(&mut self, kind: &StatementKind<'tcx>, si: SourceInfo, loc: MirLocation) {
        match kind {
            StatementKind::Assign(assign) => {
                let (place, rvalue) = &**assign;
                self.assign(*place, rvalue, si, loc);
            }
            StatementKind::StorageDead(local) => {
                if let Some(name) = self.names.get(local).cloned() {
                    let ty = self.mono(self.body.local_decls[*local].ty);
                    let place = Place::from(*local);
                    self.emit(OwnershipKind::StorageDead, si, loc, Some(name), None, Some(ty), None, &[place]);
                }
            }
            _ => {}
        }
    }

    fn assign(&mut self, place: Place<'tcx>, rvalue: &Rvalue<'tcx>, si: SourceInfo, loc: MirLocation) {
        let dst = self.dst_text(place);
        let dst_ty = self.place_ty(place);
        match rvalue {
            Rvalue::Use(op) | Rvalue::Cast(_, op, _) | Rvalue::WrapUnsafeBinder(op, _) => {
                self.operand(op, dst, si, loc, None, &[place]);
            }
            Rvalue::Repeat(op, _) => self.operand(op, format!("{dst}[..]"), si, loc, None, &[place]),
            Rvalue::Ref(_, kind, borrowed) => {
                let kind = match kind {
                    BorrowKind::Mut { .. } => OwnershipKind::MutBorrow,
                    BorrowKind::Shared | BorrowKind::Fake(_) => OwnershipKind::SharedBorrow,
                };
                self.borrow(kind, *borrowed, place, dst, dst_ty, si, loc);
            }
            Rvalue::RawPtr(kind, borrowed) => {
                let kind = match kind {
                    RawPtrKind::Mut => OwnershipKind::MutBorrow,
                    RawPtrKind::Const | RawPtrKind::FakeForPtrMetadata => OwnershipKind::SharedBorrow,
                };
                self.borrow(kind, *borrowed, place, dst, dst_ty, si, loc);
            }
            Rvalue::CopyForDeref(src) => {
                let s = self.place_text(*src);
                let ty = self.place_ty(*src);
                self.emit(OwnershipKind::Copy, si, loc, Some(s), Some(dst), Some(ty), None, &[*src, place]);
            }
            Rvalue::Aggregate(kind, fields) => {
                self.emit(OwnershipKind::Aggregate, si, loc, None, Some(dst.clone()), Some(dst_ty), None, &[place]);
                for (i, op) in fields.iter_enumerated() {
                    let field_dst = self.aggregate_field(kind, &dst, i.index());
                    self.operand(op, field_dst, si, loc, None, &[place]);
                }
            }
            Rvalue::BinaryOp(..)
            | Rvalue::UnaryOp(..)
            | Rvalue::Discriminant(_)
            | Rvalue::ThreadLocalRef(_) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn borrow(
        &mut self,
        kind: OwnershipKind,
        borrowed: Place<'tcx>,
        place: Place<'tcx>,
        dst: String,
        dst_ty: Ty<'tcx>,
        si: SourceInfo,
        loc: MirLocation,
    ) {
        let src = self.place_text(borrowed);
        if let Some((_, base_elems)) = borrowed.projection.split_last() {
            let base = Place { local: borrowed.local, projection: self.tcx.mk_place_elems(base_elems) };
            let base_text = self.place_text(base);
            let ty = self.place_ty(borrowed);
            self.emit(OwnershipKind::Projection, si, loc, Some(base_text), Some(src.clone()), Some(ty), None, &[borrowed]);
        }
        self.emit(kind, si, loc, Some(src), Some(dst), Some(dst_ty), None, &[borrowed, place]);
    }

    /// The event for an operand used as a value that goes to `dst`.
    fn operand(
        &mut self,
        op: &Operand<'tcx>,
        dst: String,
        si: SourceInfo,
        loc: MirLocation,
        callee: Option<&str>,
        places: &[Place<'tcx>],
    ) {
        let ty = self.mono(op.ty(&self.body.local_decls, self.tcx));
        let callee = callee.map(str::to_owned);
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                // Optimised MIR turns a value's last use into `copy` even for types that are not
                // `Copy` (`Config { name: copy _1, .. }`); in the source that is a move.
                let is_copy_type = self.tcx.type_is_copy_modulo_regions(self.env, ty);
                let kind = match op {
                    Operand::Copy(_) if is_copy_type => OwnershipKind::Copy,
                    _ => OwnershipKind::Move,
                };
                let src = self.place_text(*p);
                let mut all = places.to_vec();
                all.push(*p);
                self.emit(kind, si, loc, Some(src), Some(dst), Some(ty), callee, &all);
            }
            Operand::Constant(c) => {
                let src = const_text(c);
                self.emit(OwnershipKind::Literal, si, loc, Some(src), Some(dst), Some(ty), callee, places);
            }
            Operand::RuntimeChecks(_) => {}
        }
    }

    // ------------------------------------------------------------ terminators

    fn terminator(&mut self, kind: &TerminatorKind<'tcx>, si: SourceInfo, loc: MirLocation) {
        match kind {
            // Codegen gives a call terminator's instructions `fn_span`, not the terminator's span
            // (`codegen_call_terminator`), so the events carry it too.
            TerminatorKind::Call { func, args, destination, fn_span, .. } => {
                let dst = self.place_text(*destination);
                let si = SourceInfo { span: *fn_span, ..si };
                self.call(func, args, Some(*destination), dst, si, loc);
            }
            TerminatorKind::TailCall { func, args, fn_span } => {
                let si = SourceInfo { span: *fn_span, ..si };
                self.call(func, args, None, "return".to_owned(), si, loc);
            }
            TerminatorKind::Drop { place, .. } => {
                let ty = self.place_ty(*place);
                let glue = Instance::resolve_drop_in_place(self.tcx, ty);
                let callee = with_no_trimmed_paths!(self.tcx.def_path_str_with_args(glue.def_id(), glue.args));
                let src = self.place_text(*place);
                self.emit(OwnershipKind::Drop, si, loc, Some(src), None, Some(ty), Some(callee), &[*place]);
            }
            _ => {}
        }
    }

    fn call(
        &mut self,
        func: &Operand<'tcx>,
        args: &[rustc_span::Spanned<Operand<'tcx>>],
        destination: Option<Place<'tcx>>,
        dst: String,
        si: SourceInfo,
        loc: MirLocation,
    ) {
        let callee = self.callee(func);
        let short = callee.as_ref().map_or_else(|| "<fn pointer>".to_owned(), |c| c.short.clone());
        let path = callee.as_ref().map(|c| c.path.clone());
        for (i, arg) in args.iter().enumerate() {
            self.operand(&arg.node, format!("{short}(arg {i})"), si, loc, path.as_deref(), &[]);
        }
        let dst_ty = destination.map(|d| self.place_ty(d));
        let places: Vec<Place<'tcx>> = destination.into_iter().collect();
        match (&callee, args.first()) {
            (Some(c), Some(recv)) if c.role == CalleeKind::Clone => {
                let src = self.operand_rendered(&recv.node, 0).deref();
                let mut all = places.clone();
                if let Some(p) = recv.node.place() {
                    all.push(p);
                }
                self.emit(OwnershipKind::Clone, si, loc, Some(src), Some(dst), dst_ty, path, &all);
            }
            _ => {
                let src = format!("{short}(..)");
                self.emit(OwnershipKind::CallReturn, si, loc, Some(src), Some(dst), dst_ty, path, &places);
            }
        }
    }

    fn callee(&self, func: &Operand<'tcx>) -> Option<Callee> {
        let tcx = self.tcx;
        let fty = self.mono(func.ty(&self.body.local_decls, tcx));
        let ty::FnDef(def_id, args) = *fty.kind() else { return None };
        let path = with_no_trimmed_paths!(tcx.def_path_str_with_args(def_id, args));
        let short = with_forced_trimmed_paths!(tcx.def_path_str_with_args(def_id, args));
        let role = match tcx.trait_of_assoc(def_id) {
            Some(t) if tcx.is_lang_item(t, LangItem::Clone) && tcx.item_name(def_id).as_str() == "clone" => {
                CalleeKind::Clone
            }
            Some(t) if tcx.is_lang_item(t, LangItem::Index) || tcx.is_lang_item(t, LangItem::IndexMut) => {
                CalleeKind::Index
            }
            Some(t) if tcx.is_lang_item(t, LangItem::Deref) || tcx.is_lang_item(t, LangItem::DerefMut) => {
                CalleeKind::Deref
            }
            _ => CalleeKind::Other,
        };
        Some(Callee { path, short, role })
    }

    // ------------------------------------------------------------ emitting

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        kind: OwnershipKind,
        si: SourceInfo,
        location: MirLocation,
        src: Option<String>,
        dst: Option<String>,
        ty: Option<Ty<'tcx>>,
        callee: Option<String>,
        places: &[Place<'tcx>],
    ) {
        let collapsed = hygiene::walk_chain_collapsed(si.span, self.body.span);
        let span = self.span(si.span);
        let inline_chain = self.inline_chain(si.scope);
        let caveats = self.caveats(collapsed, si.scope, places, ty);
        let ty = ty.map(|t| self.type_info(t));
        let path = self.path;
        self.events.push(OwnershipEvent { kind, span, inline_chain, location, path, src, dst, ty, callee, caveats });
    }

    /// A span as codegen gives it to DWARF: macro expansions collapsed into the body's context,
    /// line and byte column 1-based.
    fn span(&mut self, sp: RSpan) -> Span {
        let sp = hygiene::walk_chain_collapsed(sp, self.body.span);
        let sm = self.tcx.sess.source_map();
        let lo = sm.lookup_byte_offset(sp.lo());
        let file = self.files.name(&lo.sf);
        let line_col = |pos: BytePos| -> LineCol {
            match sm.lookup_line(pos) {
                Ok(rustc_span::SourceFileAndLine { sf, line }) => {
                    let line_pos = sf.lines()[line];
                    LineCol { line: line as u32 + 1, col: (sf.relative_position(pos) - line_pos).to_u32() + 1 }
                }
                Err(_) => LineCol { line: 0, col: 0 },
            }
        };
        Span { file, lo: line_col(sp.lo()), hi: line_col(sp.hi()) }
    }

    fn inline_chain(&mut self, scope: SourceScope) -> Vec<InlineFrame> {
        let scopes = &self.body.source_scopes;
        let mut roots = Vec::new();
        let mut cur = if scopes[scope].inlined.is_some() { Some(scope) } else { scopes[scope].inlined_parent_scope };
        while let Some(s) = cur {
            if let Some((callee, call_site)) = scopes[s].inlined {
                roots.push((callee, call_site));
            }
            cur = scopes[s].inlined_parent_scope;
        }
        roots.reverse();
        roots
            .into_iter()
            .map(|(callee, call_site)| {
                let callee = self.mono(callee);
                InlineFrame {
                    callee: with_no_trimmed_paths!(self.tcx.def_path_str_with_args(callee.def_id(), callee.args)),
                    call_site: self.span(call_site),
                }
            })
            .collect()
    }

    /// The definition whose source a statement in `scope` comes from: the innermost MIR-inlined
    /// callee, or this instance's own item.
    fn source_def(&self, scope: SourceScope) -> DefId {
        let scopes = &self.body.source_scopes;
        let root = if scopes[scope].inlined.is_some() { Some(scope) } else { scopes[scope].inlined_parent_scope };
        match root.and_then(|r| scopes[r].inlined) {
            Some((callee, _)) => callee.def_id(),
            None => self.instance.def_id(),
        }
    }

    fn caveats(&mut self, span: RSpan, scope: SourceScope, places: &[Place<'tcx>], ty: Option<Ty<'tcx>>) -> Vec<Caveat> {
        let mut out = Vec::new();
        let def = self.source_def(scope);
        let is_item = matches!(self.instance.def, InstanceKind::Item(_)) || def != self.instance.def_id();
        let is_unsafe = !is_item
            || match self.safety.of(self.tcx, def) {
                Some(r) => r.whole_body || r.blocks.iter().any(|b| b.contains(span)),
                None => true,
            };
        if is_unsafe {
            out.push(Caveat::Unsafe);
        }
        let interior = ty.is_some_and(|t| !t.is_freeze(self.tcx, self.env))
            || places.iter().any(|p| self.place_has_interior_mutability(*p));
        if interior {
            out.push(Caveat::InteriorMutability);
        }
        out
    }

    /// Whether the place, or any place it projects through, has a type that is not `Freeze`.
    fn place_has_interior_mutability(&self, place: Place<'tcx>) -> bool {
        let tcx = self.tcx;
        let mut pt = PlaceTy::from_ty(self.body.local_decls[place.local].ty);
        if !self.mono(pt.ty).is_freeze(tcx, self.env) {
            return true;
        }
        for elem in place.projection {
            pt = pt.projection_ty(tcx, elem);
            if !self.mono(pt.ty).is_freeze(tcx, self.env) {
                return true;
            }
        }
        false
    }

    fn type_info(&self, ty: Ty<'tcx>) -> TypeInfo {
        let name = with_forced_trimmed_paths!(ty.to_string());
        let size = self.tcx.layout_of(self.env.as_query_input(ty)).ok().map(|l| l.size.bytes());
        TypeInfo { name, size }
    }

    fn place_ty(&self, place: Place<'tcx>) -> Ty<'tcx> {
        self.mono(place.ty(&self.body.local_decls, self.tcx).ty)
    }

    // ------------------------------------------------------------ places as source text

    /// Temporaries (locals without a debug name) assigned exactly once, and what they hold.
    fn single_assignment_temps(&self) -> HashMap<Local, TempValue<'tcx>> {
        let mut count: HashMap<Local, u32> = HashMap::new();
        let mut value: HashMap<Local, TempValue<'tcx>> = HashMap::new();
        for data in self.body.basic_blocks.iter() {
            for stmt in &data.statements {
                if let StatementKind::Assign(assign) = &stmt.kind {
                    let (place, rvalue) = &**assign;
                    if !place.projection.is_empty() {
                        continue;
                    }
                    *count.entry(place.local).or_default() += 1;
                    let v = match rvalue {
                        Rvalue::Ref(_, _, p) | Rvalue::RawPtr(_, p) => Some(TempValue::Ref(*p)),
                        Rvalue::CopyForDeref(p) => Some(TempValue::Use(*p)),
                        Rvalue::Use(op) | Rvalue::Cast(_, op, _) => match op {
                            Operand::Copy(p) | Operand::Move(p) => Some(TempValue::Use(*p)),
                            Operand::Constant(c) => Some(TempValue::Const(const_text(c))),
                            Operand::RuntimeChecks(_) => None,
                        },
                        _ => None,
                    };
                    if let Some(v) = v {
                        value.insert(place.local, v);
                    }
                }
            }
            if let Some(term) = &data.terminator
                && let TerminatorKind::Call { func, args, destination, .. } = &term.kind
                && destination.projection.is_empty()
            {
                *count.entry(destination.local).or_default() += 1;
                let v = match (self.callee(func).map(|c| c.role), args.first(), args.get(1)) {
                    (Some(CalleeKind::Index), Some(base), Some(idx)) => {
                        Some(TempValue::Index(base.node.clone(), idx.node.clone()))
                    }
                    (Some(CalleeKind::Deref), Some(base), _) => Some(TempValue::Deref(base.node.clone())),
                    _ => None,
                };
                if let Some(v) = v {
                    value.insert(destination.local, v);
                }
            }
        }
        value.retain(|l, _| count.get(l) == Some(&1) && !self.names.contains_key(l) && *l != mir::RETURN_PLACE);
        value
    }

    /// A place written by an assignment. A temporary that only holds a copy of another place or
    /// a constant is named raw (`_8`): rendering it through its value would read `move c -> c`.
    fn dst_text(&self, place: Place<'tcx>) -> String {
        if place.projection.is_empty()
            && matches!(self.temps.get(&place.local), Some(TempValue::Use(_) | TempValue::Const(_)))
        {
            return format!("{:?}", place.local);
        }
        self.place_text(place)
    }

    fn place_text(&self, place: Place<'tcx>) -> String {
        self.render_place(place, 0).text()
    }

    fn render_local(&self, local: Local, depth: u32) -> Rendered {
        if let Some(name) = self.names.get(&local) {
            return Rendered::Place(name.clone());
        }
        if local == mir::RETURN_PLACE {
            return Rendered::Place("return".to_owned());
        }
        if depth < MAX_RENDER_DEPTH
            && let Some(v) = self.temps.get(&local)
        {
            return match v {
                TempValue::Ref(p) => Rendered::Ref(self.render_place(*p, depth + 1).text()),
                TempValue::Use(p) => self.render_place(*p, depth + 1),
                TempValue::Index(base, idx) => {
                    let base = self.operand_rendered(base, depth + 1).deref();
                    let idx = self.operand_value_text(idx, depth + 1);
                    Rendered::Ref(format!("{base}[{idx}]"))
                }
                TempValue::Deref(base) => {
                    let base = self.operand_rendered(base, depth + 1).deref();
                    Rendered::Ref(format!("*{base}"))
                }
                TempValue::Const(c) => Rendered::Place(c.clone()),
            };
        }
        Rendered::Place(format!("{local:?}"))
    }

    fn operand_rendered(&self, op: &Operand<'tcx>, depth: u32) -> Rendered {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.render_place(*p, depth),
            Operand::Constant(c) => Rendered::Place(const_text(c)),
            Operand::RuntimeChecks(_) => Rendered::Place("<runtime checks>".to_owned()),
        }
    }

    fn operand_value_text(&self, op: &Operand<'tcx>, depth: u32) -> String {
        self.operand_rendered(op, depth).text()
    }

    fn render_place(&self, place: Place<'tcx>, depth: u32) -> Rendered {
        // A debug name bound to a projected place (a closure's capture) names that prefix.
        let mut start = 0;
        let mut acc = None;
        for (p, name) in &self.projected_names {
            if p.local == place.local
                && place.projection.len() >= p.projection.len()
                && place.projection[..p.projection.len()] == p.projection[..]
            {
                start = p.projection.len();
                acc = Some(Rendered::Place(name.clone()));
                break;
            }
        }
        let mut acc = acc.unwrap_or_else(|| self.render_local(place.local, depth));
        let tcx = self.tcx;
        let mut pt = PlaceTy::from_ty(self.body.local_decls[place.local].ty);
        for elem in &place.projection[..start] {
            pt = pt.projection_ty(tcx, *elem);
        }
        for elem in &place.projection[start..] {
            acc = match *elem {
                ProjectionElem::Deref => Rendered::Place(acc.deref()),
                ProjectionElem::Field(f, _) => {
                    let base = acc.text();
                    let field = field_name(tcx, pt, f.index());
                    // `(*self).name` reads as `self.name`, as the source writes it.
                    match base.strip_prefix('*') {
                        Some(inner) if is_simple_path(inner) => Rendered::Place(format!("{inner}.{field}")),
                        _ if is_simple_path(&base) || base.ends_with(']') || base.ends_with(')') => {
                            Rendered::Place(format!("{base}.{field}"))
                        }
                        _ => Rendered::Place(format!("({base}).{field}")),
                    }
                }
                ProjectionElem::Index(i) => {
                    let idx = self.render_local(i, depth + 1).text();
                    Rendered::Place(format!("{}[{idx}]", acc.text()))
                }
                ProjectionElem::ConstantIndex { offset, from_end: false, .. } => {
                    Rendered::Place(format!("{}[{offset}]", acc.text()))
                }
                ProjectionElem::ConstantIndex { offset, from_end: true, .. } => {
                    Rendered::Place(format!("{}[len - {offset}]", acc.text()))
                }
                ProjectionElem::Subslice { from, to, from_end: false } => {
                    Rendered::Place(format!("{}[{from}..{to}]", acc.text()))
                }
                ProjectionElem::Subslice { from, to, from_end: true } => {
                    Rendered::Place(format!("{}[{from}..len - {to}]", acc.text()))
                }
                ProjectionElem::Downcast(name, v) => {
                    let variant = match name {
                        Some(n) => n.to_string(),
                        None => variant_name(tcx, pt, v.index()),
                    };
                    Rendered::Place(format!("({} as {variant})", acc.text()))
                }
                ProjectionElem::OpaqueCast(_) | ProjectionElem::UnwrapUnsafeBinder(_) => acc,
            };
            pt = pt.projection_ty(tcx, *elem);
        }
        acc
    }

    fn aggregate_field(&self, kind: &AggregateKind<'tcx>, dst: &str, index: usize) -> String {
        let tcx = self.tcx;
        match kind {
            AggregateKind::Adt(def_id, variant, ..) => {
                let adt = tcx.adt_def(*def_id);
                let v = adt.variant(*variant);
                let field = v.fields.iter().nth(index).map_or_else(|| index.to_string(), |f| f.name.to_string());
                if adt.is_enum() { format!("({dst} as {}).{field}", v.name) } else { format!("{dst}.{field}") }
            }
            AggregateKind::Array(_) => format!("{dst}[{index}]"),
            AggregateKind::Closure(def_id, _) | AggregateKind::CoroutineClosure(def_id, _) => {
                format!("{dst}.{}", capture_name(tcx, *def_id, index))
            }
            _ => format!("{dst}.{index}"),
        }
    }
}

fn assign_const_entry(map: &mut HashMap<Local, bool>, l: Local) -> &mut bool {
    map.entry(l).or_insert(true)
}

fn is_simple_path(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

fn field_name<'tcx>(tcx: TyCtxt<'tcx>, pt: PlaceTy<'tcx>, index: usize) -> String {
    match pt.ty.kind() {
        ty::Adt(def, _) => {
            let variant = match pt.variant_index {
                Some(v) => Some(def.variant(v)),
                None if !def.is_enum() => Some(def.non_enum_variant()),
                None => None,
            };
            variant
                .and_then(|v| v.fields.iter().nth(index))
                .map_or_else(|| index.to_string(), |f| f.name.to_string())
        }
        ty::Closure(def_id, _) | ty::CoroutineClosure(def_id, _) => capture_name(tcx, *def_id, index),
        _ => index.to_string(),
    }
}

fn variant_name<'tcx>(_tcx: TyCtxt<'tcx>, pt: PlaceTy<'tcx>, index: usize) -> String {
    match pt.ty.kind() {
        ty::Adt(def, _) if def.is_enum() => def
            .variants()
            .iter()
            .nth(index)
            .map_or_else(|| format!("variant {index}"), |v| v.name.to_string()),
        _ => format!("variant {index}"),
    }
}

fn capture_name(tcx: TyCtxt<'_>, closure: DefId, index: usize) -> String {
    closure
        .as_local()
        .and_then(|l| tcx.closure_captures(l).get(index).map(|c| c.to_symbol().to_string()))
        .unwrap_or_else(|| index.to_string())
}

/// A constant's text as the source would write it: `"alpha"`, `3`, `0`.
fn const_text(c: &mir::ConstOperand<'_>) -> String {
    let s = with_no_trimmed_paths!(c.to_string());
    let s = s.strip_prefix("const ").unwrap_or(&s).to_owned();
    strip_int_suffix(&s).unwrap_or(s)
}

/// `3_u32` → `3`, `-1_isize` → `-1`.
fn strip_int_suffix(s: &str) -> Option<String> {
    let (num, suffix) = s.rsplit_once('_')?;
    const SUFFIXES: [&str; 12] =
        ["u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize"];
    let digits = num.strip_prefix('-').unwrap_or(num);
    (SUFFIXES.contains(&suffix) && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| num.to_owned())
}
