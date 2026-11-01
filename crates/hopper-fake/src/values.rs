//! The fake's value model: typed values with the worked example's layouts, their rendering into
//! [`ValueView`]s, and a small expression evaluator (`a`, `v[0].name`, `*n`, `&b.retries`).

use hopper_core::api::{OwnershipState, PathElem};
use hopper_core::{HopperError, Result};

/// Whether a reference is shared (`&T`) or exclusive (`&mut T`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RefKind {
    Shared,
    Mut,
}

/// A value of the worked example, with the layout rustc gives it on x86-64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FakeVal {
    /// Declared, in scope, never written.
    Uninit,
    U32(u32),
    /// `String`: `{ cap, ptr, len }`; `len` is `text.len()`.
    Str {
        cap: u64,
        ptr: u64,
        text: String,
    },
    Struct {
        ty: String,
        fields: Vec<(String, FakeVal)>,
    },
    /// `Vec<T>`: `{ cap, ptr, len }`; `len` is `elems.len()`.
    Vec {
        elem_ty: String,
        cap: u64,
        ptr: u64,
        elems: Vec<FakeVal>,
    },
    Ref {
        kind: RefKind,
        addr: u64,
        target_ty: String,
        target: Box<FakeVal>,
    },
}

pub(crate) const STRING_TY: &str = "alloc::string::String";
pub(crate) const CONFIG_TY: &str = "worked_example::Config";

impl FakeVal {
    pub(crate) fn string(cap: u64, ptr: u64, text: &str) -> FakeVal {
        FakeVal::Str {
            cap,
            ptr,
            text: text.to_string(),
        }
    }

    pub(crate) fn config(name: FakeVal, retries: u32) -> FakeVal {
        FakeVal::Struct {
            ty: CONFIG_TY.to_string(),
            fields: vec![
                ("name".into(), name),
                ("retries".into(), FakeVal::U32(retries)),
            ],
        }
    }

    pub(crate) fn type_name(&self, declared: &str) -> String {
        match self {
            FakeVal::Uninit => declared.to_string(),
            FakeVal::U32(_) => "u32".into(),
            FakeVal::Str { .. } => STRING_TY.into(),
            FakeVal::Struct { ty, .. } => ty.clone(),
            FakeVal::Vec { elem_ty, .. } => format!("alloc::vec::Vec<{elem_ty}>"),
            FakeVal::Ref {
                kind: RefKind::Shared,
                target_ty,
                ..
            } => format!("&{target_ty}"),
            FakeVal::Ref {
                kind: RefKind::Mut,
                target_ty,
                ..
            } => format!("&mut {target_ty}"),
        }
    }

    pub(crate) fn align(&self) -> u64 {
        match self {
            FakeVal::U32(_) => 4,
            FakeVal::Struct { fields, .. } => {
                fields.iter().map(|(_, f)| f.align()).max().unwrap_or(1)
            }
            _ => 8,
        }
    }

    /// Byte size; `None` for `Uninit`, whose size comes from its declaration.
    pub(crate) fn size(&self) -> Option<u64> {
        match self {
            FakeVal::Uninit => None,
            FakeVal::U32(_) => Some(4),
            FakeVal::Str { .. } | FakeVal::Vec { .. } => Some(24),
            FakeVal::Ref { .. } => Some(8),
            FakeVal::Struct { .. } => {
                let (_, end) = self.field_offsets();
                let a = self.align();
                Some(end.div_ceil(a) * a)
            }
        }
    }

    /// Field offsets in declaration order (the fake keeps rustc's order for `Config`, which puts
    /// the `String` first), and the end of the last field.
    fn field_offsets(&self) -> (Vec<u64>, u64) {
        let FakeVal::Struct { fields, .. } = self else {
            return (Vec::new(), 0);
        };
        let mut off = 0u64;
        let mut out = Vec::with_capacity(fields.len());
        for (_, f) in fields {
            let a = f.align();
            off = off.div_ceil(a) * a;
            out.push(off);
            off += f.size().unwrap_or(8);
        }
        (out, off)
    }

    /// The bytes this value occupies in memory (`None` for `Uninit`).
    pub(crate) fn encode(&self) -> Option<Vec<u8>> {
        let le = |x: u64| x.to_le_bytes().to_vec();
        Some(match self {
            FakeVal::Uninit => return None,
            FakeVal::U32(n) => n.to_le_bytes().to_vec(),
            FakeVal::Str { cap, ptr, text } => [le(*cap), le(*ptr), le(text.len() as u64)].concat(),
            FakeVal::Vec {
                cap, ptr, elems, ..
            } => [le(*cap), le(*ptr), le(elems.len() as u64)].concat(),
            FakeVal::Ref { addr, .. } => le(*addr),
            FakeVal::Struct { fields, .. } => {
                let (offsets, _) = self.field_offsets();
                let mut out = vec![0u8; self.size().unwrap_or(0) as usize];
                for ((_, f), off) in fields.iter().zip(offsets) {
                    let bytes = f.encode().unwrap_or_default();
                    out[off as usize..off as usize + bytes.len()].copy_from_slice(&bytes);
                }
                out
            }
        })
    }

    /// One-line rendering.
    pub(crate) fn summary(&self) -> String {
        match self {
            FakeVal::Uninit => "<uninitialised>".into(),
            FakeVal::U32(n) => n.to_string(),
            FakeVal::Str { text, .. } => format!("{text:?}"),
            FakeVal::Struct { ty, fields } => {
                let short = ty.rsplit("::").next().unwrap_or(ty);
                let inner: Vec<String> = fields
                    .iter()
                    .map(|(n, f)| format!("{n}: {}", f.summary()))
                    .collect();
                format!("{short} {{ {} }}", inner.join(", "))
            }
            FakeVal::Vec { elems, .. } => format!("Vec(len={})", elems.len()),
            FakeVal::Ref {
                kind: RefKind::Shared,
                target,
                ..
            } => format!("&{}", target.summary()),
            FakeVal::Ref {
                kind: RefKind::Mut,
                target,
                ..
            } => format!("&mut {}", target.summary()),
        }
    }

    pub(crate) fn child_count(&self) -> Option<u64> {
        match self {
            FakeVal::Struct { fields, .. } => Some(fields.len() as u64),
            FakeVal::Vec { elems, .. } => Some(elems.len() as u64),
            FakeVal::Ref { .. } => Some(1),
            _ => None,
        }
    }
}

/// A value found by evaluation: what it is, where it is and its ownership state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub name: String,
    pub val: FakeVal,
    pub ty: String,
    /// `None` for temporaries such as `&x`.
    pub addr: Option<u64>,
    pub size: u64,
    pub ownership: OwnershipState,
}

impl Resolved {
    fn derived(&self, name: String, val: FakeVal, addr: Option<u64>) -> Resolved {
        let ty = val.type_name("?");
        let size = val.size().unwrap_or(0);
        let ownership = match &self.ownership {
            OwnershipState::Live => OwnershipState::Live,
            other => other.clone(),
        };
        Resolved {
            name,
            val,
            ty,
            addr,
            size,
            ownership,
        }
    }

    /// Every child with the path element that reaches it.
    pub(crate) fn children(&self) -> Vec<(PathElem, Resolved)> {
        match &self.val {
            FakeVal::Struct { fields, .. } => {
                let (offsets, _) = self.val.field_offsets();
                fields
                    .iter()
                    .zip(offsets)
                    .map(|((n, f), off)| {
                        let addr = self.addr.map(|a| a + off);
                        (
                            PathElem::Field(n.clone()),
                            self.derived(n.clone(), f.clone(), addr),
                        )
                    })
                    .collect()
            }
            FakeVal::Vec { ptr, elems, .. } => {
                let stride = elems.first().and_then(FakeVal::size).unwrap_or(0);
                elems
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let addr = Some(ptr + i as u64 * stride);
                        (
                            PathElem::Index(i as u64),
                            self.derived(format!("[{i}]"), e.clone(), addr),
                        )
                    })
                    .collect()
            }
            FakeVal::Ref {
                addr,
                target,
                target_ty,
                ..
            } => {
                let mut child =
                    self.derived(format!("*{}", self.name), (**target).clone(), Some(*addr));
                child.ownership = OwnershipState::Live;
                if matches!(**target, FakeVal::Uninit) {
                    child.ty = target_ty.clone();
                }
                vec![(PathElem::Deref, child)]
            }
            _ => Vec::new(),
        }
    }

    /// Follow one path element. Field access and indexing auto-dereference references, as in
    /// Rust source.
    pub(crate) fn project(&self, elem: &PathElem) -> Result<Resolved> {
        if let (FakeVal::Ref { .. }, PathElem::Field(_) | PathElem::Index(_)) = (&self.val, elem) {
            return self.project(&PathElem::Deref)?.project(elem);
        }
        if matches!(self.val, FakeVal::Uninit) {
            return Err(HopperError::Eval(format!(
                "`{}` is not initialised here",
                self.name
            )));
        }
        self.children()
            .into_iter()
            .find(|(e, _)| e == elem)
            .map(|(_, r)| r)
            .ok_or_else(|| HopperError::Eval(format!("`{}` has no {}", self.name, describe(elem))))
    }

    /// `&self`, as a temporary.
    pub(crate) fn borrow(&self) -> Result<Resolved> {
        let addr = self
            .addr
            .ok_or_else(|| HopperError::Eval(format!("cannot borrow temporary `{}`", self.name)))?;
        let val = FakeVal::Ref {
            kind: RefKind::Shared,
            addr,
            target_ty: self.ty.clone(),
            target: Box::new(self.val.clone()),
        };
        Ok(Resolved {
            name: format!("&{}", self.name),
            ty: val.type_name("?"),
            size: 8,
            val,
            addr: None,
            ownership: OwnershipState::Live,
        })
    }
}

fn describe(elem: &PathElem) -> String {
    match elem {
        PathElem::Field(f) => format!("field `{f}`"),
        PathElem::Index(i) => format!("element [{i}]"),
        PathElem::Deref => "pointee".into(),
        PathElem::Variant(v) => format!("variant `{v}`"),
    }
}

/// A unary prefix operator of a parsed expression.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Prefix {
    Deref,
    Borrow,
}

/// `prefix* ident ( .field | [index] )*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedExpr {
    /// Outermost first (`&*x` is `[Borrow, Deref]`).
    pub prefixes: Vec<Prefix>,
    pub root: String,
    pub path: Vec<PathElem>,
}

pub(crate) fn parse_expr(src: &str) -> Result<ParsedExpr> {
    let bad = |why: &str| HopperError::Eval(format!("cannot parse `{src}`: {why}"));
    let s: Vec<char> = src.trim().chars().filter(|c| !c.is_whitespace()).collect();
    let mut i = 0;
    let mut prefixes = Vec::new();
    while i < s.len() && (s[i] == '*' || s[i] == '&') {
        prefixes.push(if s[i] == '*' {
            Prefix::Deref
        } else {
            Prefix::Borrow
        });
        i += 1;
    }
    let ident = |i: &mut usize| {
        let start = *i;
        while *i < s.len() && (s[*i].is_alphanumeric() || s[*i] == '_') {
            *i += 1;
        }
        s[start..*i].iter().collect::<String>()
    };
    let root = ident(&mut i);
    if root.is_empty() || root.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(bad("expected a variable name"));
    }
    let mut path = Vec::new();
    while i < s.len() {
        match s[i] {
            '.' => {
                i += 1;
                let f = ident(&mut i);
                if f.is_empty() {
                    return Err(bad("expected a field name after `.`"));
                }
                path.push(PathElem::Field(f));
            }
            '[' => {
                i += 1;
                let start = i;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                let n: u64 = s[start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| bad("expected an integer index"))?;
                if i >= s.len() || s[i] != ']' {
                    return Err(bad("expected `]`"));
                }
                i += 1;
                path.push(PathElem::Index(n));
            }
            c => return Err(bad(&format!("unexpected `{c}`"))),
        }
    }
    Ok(ParsedExpr {
        prefixes,
        root,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_layout_matches_rustc() {
        let c = FakeVal::config(FakeVal::string(5, 0x1000, "alpha"), 3);
        assert_eq!(c.size(), Some(32));
        let bytes = c.encode().unwrap();
        assert_eq!(&bytes[0..8], &5u64.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x1000u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &5u64.to_le_bytes());
        assert_eq!(&bytes[24..28], &3u32.to_le_bytes());
        assert_eq!(c.summary(), "Config { name: \"alpha\", retries: 3 }");
    }

    #[test]
    fn parses_paths() {
        let p = parse_expr("&*v[0].name").unwrap();
        assert_eq!(p.prefixes, vec![Prefix::Borrow, Prefix::Deref]);
        assert_eq!(p.root, "v");
        assert_eq!(
            p.path,
            vec![PathElem::Index(0), PathElem::Field("name".into())]
        );
        assert!(parse_expr("v[").is_err());
        assert!(parse_expr("1+2").is_err());
        assert!(parse_expr("").is_err());
    }
}
