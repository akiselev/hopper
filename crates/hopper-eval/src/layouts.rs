//! The Rust standard-library layout table.
//!
//! Each row names a std type by a pattern on its qualified name, says what kind of collection or
//! wrapper it is, and gives field paths for each role (data pointer, length, capacity, ...),
//! gated on rustc versions where the layout changed. The interpretation code in `view.rs` reads
//! roles through this table and never spells a std field name itself.
//!
//! Seeded from Rust's own debugger providers (`src/etc/gdb_providers.py`, `lldb_providers.py`,
//! `rust_types.py`; MIT OR Apache-2.0, rustc 1.97.0) and BugStalker's `version_switch!` rules
//! (MIT). See `THIRD_PARTY_NOTICES.md`. Paths are followed, then newtype wrappers
//! (`NonNull { pointer }`, `Unique { pointer, _marker }`, `UsizeNoHighBit(usize)`,
//! `Cell { value }`, `UnsafeCell { value }`) are unwrapped down to a scalar or pointer, so a
//! wrapper added in a future release usually needs no new row. A `*` segment follows a pointer
//! (`BorrowRef.borrow` is a `&Cell<isize>` into the `RefCell`).

use hopper_symbols::RustcVersion;

/// What a std type is, for display and children.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum StdKind {
    String,
    /// `&str`, `&mut str`.
    Str,
    /// `&[T]`, `&mut [T]`, `*const [T]`.
    Slice,
    Vec,
    VecDeque,
    HashMap,
    HashSet,
    BTreeMap,
    BTreeSet,
    Rc,
    Arc,
    /// `alloc::rc::Weak`.
    RcWeak,
    /// `alloc::sync::Weak`.
    ArcWeak,
    Cell,
    RefCell,
    Ref,
    RefMut,
    /// `NonZero<T>`: shown as its number.
    NonZero,
    OsString,
    PathBuf,
}

/// A role a field path plays in a std type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Pointer to the first element (or byte).
    DataPtr,
    Len,
    Capacity,
    /// VecDeque's physical index of the first element.
    Head,
    /// hashbrown's `RawTable<T>`; its `T` parameter is the stored element.
    Table,
    BucketMask,
    /// hashbrown's control bytes; elements are stored before them, slot `i` at
    /// `ctrl - (i + 1) * size_of::<T>()`.
    Ctrl,
    Items,
    /// BTreeMap's `Option<NodeRef>` root.
    Root,
    /// BTreeMap's element count.
    Length,
    /// The inner BTreeMap of a BTreeSet, the inner `String`-like buffer of OsString/PathBuf.
    Inner,
    /// `NodeRef.node`, `NodeRef.height`.
    RootNode,
    RootHeight,
    /// `LeafNode.len`, `.keys`, `.vals`.
    NodeLen,
    NodeKeys,
    NodeVals,
    /// Rc/Arc/Weak: pointer to the shared allocation (`RcInner`/`ArcInner`).
    SharedPtr,
    /// Inside `RcInner`/`ArcInner`.
    Strong,
    Weak,
    SharedValue,
    /// Cell/RefCell: the wrapped value. Ref/RefMut: the pointer to it.
    Value,
    /// RefCell/Ref: the borrow counter (negative while mutably borrowed).
    Borrow,
    /// NonZero's number.
    Number,
}

/// The rustc versions a rule applies to: `[since, until)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    pub since: Option<(u32, u32)>,
    pub until: Option<(u32, u32)>,
}

impl Versions {
    pub const ALL: Versions = Versions { since: None, until: None };

    pub const fn since(major: u32, minor: u32) -> Versions {
        Versions { since: Some((major, minor)), until: None }
    }

    pub const fn until(major: u32, minor: u32) -> Versions {
        Versions { since: None, until: Some((major, minor)) }
    }

    pub fn contains(&self, v: RustcVersion) -> bool {
        let v = (v.major, v.minor);
        self.since.is_none_or(|s| v >= s) && self.until.is_none_or(|u| v < u)
    }
}

/// A field path for a role.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub role: Role,
    pub path: &'static [&'static str],
    pub rustc: Versions,
}

const fn rule(role: Role, path: &'static [&'static str]) -> Rule {
    Rule { role, path, rustc: Versions::ALL }
}

const fn rule_in(role: Role, path: &'static [&'static str], rustc: Versions) -> Rule {
    Rule { role, path, rustc }
}

/// How a row recognises a type by its qualified name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TypePattern {
    /// `<root>::(<module>::)+<name>`, followed by `<...>` when `generic`. The same shape as
    /// `rust_types.py`'s `^(alloc::([a-z_]+::)+)Vec<.+>$`.
    Path { root: &'static str, name: &'static str, generic: bool },
    /// `&str`, `&mut str` and the raw pointer forms.
    StrRef,
    /// `&[T]`, `&mut [T]`, `*const [T]`, `*mut [T]`.
    SliceRef,
}

impl TypePattern {
    pub fn matches(&self, name: &str) -> bool {
        match *self {
            TypePattern::StrRef => matches!(name, "&str" | "&mut str" | "*const str" | "*mut str"),
            TypePattern::SliceRef => ["&[", "&mut [", "*const [", "*mut ["]
                .iter()
                .any(|p| name.starts_with(p) && name.ends_with(']') && !name.contains("; ")),
            TypePattern::Path { root, name: want, generic } => {
                let Some(rest) = name.strip_prefix(root).and_then(|r| r.strip_prefix("::")) else {
                    return false;
                };
                // One or more lowercase module segments.
                let mut rest = rest;
                let mut modules = 0;
                while let Some(i) = rest.find("::") {
                    let seg = &rest[..i];
                    if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                        break;
                    }
                    modules += 1;
                    rest = &rest[i + 2..];
                }
                if modules == 0 {
                    return false;
                }
                match rest.strip_prefix(want) {
                    Some("") => !generic,
                    Some(tail) => generic && tail.starts_with('<') && tail.ends_with('>'),
                    None => false,
                }
            }
        }
    }
}

/// One row of the table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StdLayout {
    pub kind: StdKind,
    pub pattern: TypePattern,
    pub rules: &'static [Rule],
}

impl StdLayout {
    /// The paths for `role`, the ones that apply to `rustc` first (all of them, in table order,
    /// when the version is unknown). Callers try them in order.
    pub fn paths(&self, role: Role, rustc: Option<RustcVersion>) -> Vec<&'static [&'static str]> {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        for r in self.rules.iter().filter(|r| r.role == role) {
            match rustc {
                Some(v) if !r.rustc.contains(v) => rest.push(r.path),
                _ => first.push(r.path),
            }
        }
        first.extend(rest);
        first
    }
}

use Role::*;

/// RawVec's pointer and capacity moved into `RawVecInner` in rustc 1.83; the capacity became the
/// `Cap`/`UsizeNoHighBit` newtype in 1.76 (unwrapped automatically).
const VEC_RULES: &[Rule] = &[
    rule_in(DataPtr, &["buf", "inner", "ptr"], Versions::since(1, 83)),
    rule_in(DataPtr, &["buf", "ptr"], Versions::until(1, 83)),
    rule(Len, &["len"]),
    rule_in(Capacity, &["buf", "inner", "cap"], Versions::since(1, 83)),
    rule_in(Capacity, &["buf", "cap"], Versions::until(1, 83)),
];

const STRING_RULES: &[Rule] = &[
    rule_in(DataPtr, &["vec", "buf", "inner", "ptr"], Versions::since(1, 83)),
    rule_in(DataPtr, &["vec", "buf", "ptr"], Versions::until(1, 83)),
    rule(Len, &["vec", "len"]),
];

/// The Rust standard-library layouts hopper understands, matched in order.
pub static STD_LAYOUTS: &[StdLayout] = &[
    StdLayout {
        kind: StdKind::String,
        pattern: TypePattern::Path { root: "alloc", name: "String", generic: false },
        rules: STRING_RULES,
    },
    StdLayout {
        kind: StdKind::Str,
        pattern: TypePattern::StrRef,
        rules: &[rule(DataPtr, &["data_ptr"]), rule(Len, &["length"])],
    },
    StdLayout {
        kind: StdKind::Slice,
        pattern: TypePattern::SliceRef,
        rules: &[rule(DataPtr, &["data_ptr"]), rule(Len, &["length"])],
    },
    StdLayout {
        kind: StdKind::Vec,
        pattern: TypePattern::Path { root: "alloc", name: "Vec", generic: true },
        rules: VEC_RULES,
    },
    StdLayout {
        kind: StdKind::VecDeque,
        pattern: TypePattern::Path { root: "alloc", name: "VecDeque", generic: true },
        rules: &[
            // `head` became the `WrappedIndex` newtype after rustc 1.95 (gdb_providers.py:
            // "BACKCOMPAT: rust 1.95").
            rule_in(Head, &["head", "__0"], Versions::since(1, 96)),
            rule_in(Head, &["head"], Versions::until(1, 96)),
            rule(Len, &["len"]),
            rule_in(DataPtr, &["buf", "inner", "ptr"], Versions::since(1, 83)),
            rule_in(DataPtr, &["buf", "ptr"], Versions::until(1, 83)),
            rule_in(Capacity, &["buf", "inner", "cap"], Versions::since(1, 83)),
            rule_in(Capacity, &["buf", "cap"], Versions::until(1, 83)),
        ],
    },
    StdLayout {
        kind: StdKind::HashMap,
        pattern: TypePattern::Path { root: "std", name: "HashMap", generic: true },
        rules: &[
            rule(Table, &["base", "table"]),
            rule(BucketMask, &["base", "table", "table", "bucket_mask"]),
            rule(Ctrl, &["base", "table", "table", "ctrl"]),
            rule(Items, &["base", "table", "table", "items"]),
        ],
    },
    StdLayout {
        kind: StdKind::HashSet,
        pattern: TypePattern::Path { root: "std", name: "HashSet", generic: true },
        rules: &[
            rule(Table, &["base", "map", "table"]),
            rule(BucketMask, &["base", "map", "table", "table", "bucket_mask"]),
            rule(Ctrl, &["base", "map", "table", "table", "ctrl"]),
            rule(Items, &["base", "map", "table", "table", "items"]),
        ],
    },
    StdLayout {
        kind: StdKind::BTreeMap,
        pattern: TypePattern::Path { root: "alloc", name: "BTreeMap", generic: true },
        rules: &[
            rule(Root, &["root"]),
            rule(Length, &["length"]),
            rule(RootNode, &["node"]),
            rule(RootHeight, &["height"]),
            rule(NodeLen, &["len"]),
            rule(NodeKeys, &["keys"]),
            rule(NodeVals, &["vals"]),
        ],
    },
    StdLayout {
        kind: StdKind::BTreeSet,
        pattern: TypePattern::Path { root: "alloc", name: "BTreeSet", generic: true },
        rules: &[rule(Inner, &["map"])],
    },
    StdLayout {
        kind: StdKind::Rc,
        pattern: TypePattern::Path { root: "alloc", name: "Rc", generic: true },
        rules: &[
            rule(SharedPtr, &["ptr"]),
            rule(Strong, &["strong"]),
            rule(Weak, &["weak"]),
            rule(SharedValue, &["value"]),
        ],
    },
    StdLayout {
        kind: StdKind::Arc,
        pattern: TypePattern::Path { root: "alloc", name: "Arc", generic: true },
        rules: &[
            rule(SharedPtr, &["ptr"]),
            rule(Strong, &["strong"]),
            rule(Weak, &["weak"]),
            rule(SharedValue, &["data"]),
        ],
    },
    StdLayout {
        kind: StdKind::RcWeak,
        pattern: TypePattern::Path { root: "alloc", name: "Weak", generic: true },
        rules: &[
            rule(SharedPtr, &["ptr"]),
            rule(Strong, &["strong"]),
            rule(Weak, &["weak"]),
            rule(SharedValue, &["value"]),
            rule(SharedValue, &["data"]),
        ],
    },
    StdLayout {
        kind: StdKind::RefCell,
        pattern: TypePattern::Path { root: "core", name: "RefCell", generic: true },
        rules: &[rule(Value, &["value", "value"]), rule(Borrow, &["borrow"])],
    },
    StdLayout {
        kind: StdKind::Cell,
        pattern: TypePattern::Path { root: "core", name: "Cell", generic: true },
        rules: &[rule(Value, &["value", "value"])],
    },
    StdLayout {
        kind: StdKind::Ref,
        pattern: TypePattern::Path { root: "core", name: "Ref", generic: true },
        rules: &[rule(Value, &["value"]), rule(Borrow, &["borrow", "borrow", "*"])],
    },
    StdLayout {
        kind: StdKind::RefMut,
        pattern: TypePattern::Path { root: "core", name: "RefMut", generic: true },
        rules: &[rule(Value, &["value"]), rule(Borrow, &["borrow", "borrow", "*"])],
    },
    StdLayout {
        kind: StdKind::NonZero,
        pattern: TypePattern::Path { root: "core", name: "NonZero", generic: true },
        rules: &[rule(Number, &["__0"])],
    },
    StdLayout {
        kind: StdKind::OsString,
        pattern: TypePattern::Path { root: "std", name: "OsString", generic: false },
        // `OsString { inner: Buf { inner: Vec<u8> } }` on Unix.
        rules: &[rule(Inner, &["inner", "inner"])],
    },
    StdLayout {
        kind: StdKind::PathBuf,
        pattern: TypePattern::Path { root: "std", name: "PathBuf", generic: false },
        rules: &[rule(Inner, &["inner", "inner", "inner"])],
    },
];

/// The row for a type name.
pub fn layout_for(type_name: &str) -> Option<&'static StdLayout> {
    // `alloc::rc::Weak` and `alloc::sync::Weak` share a name; tell them apart by module.
    STD_LAYOUTS.iter().find(|l| l.pattern.matches(type_name)).map(|l| {
        if l.kind == StdKind::RcWeak && type_name.starts_with("alloc::sync::") {
            &ARC_WEAK
        } else {
            l
        }
    })
}

static ARC_WEAK: StdLayout = StdLayout {
    kind: StdKind::ArcWeak,
    pattern: TypePattern::Path { root: "alloc", name: "Weak", generic: true },
    rules: &[
        rule(SharedPtr, &["ptr"]),
        rule(Strong, &["strong"]),
        rule(Weak, &["weak"]),
        rule(SharedValue, &["data"]),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(name: &str) -> Option<StdKind> {
        layout_for(name).map(|l| l.kind)
    }

    #[test]
    fn recognises_std_type_names() {
        assert_eq!(kind("alloc::string::String"), Some(StdKind::String));
        assert_eq!(kind("alloc::vec::Vec<u32, alloc::alloc::Global>"), Some(StdKind::Vec));
        assert_eq!(
            kind("alloc::collections::vec_deque::VecDeque<i32, alloc::alloc::Global>"),
            Some(StdKind::VecDeque)
        );
        assert_eq!(
            kind("std::collections::hash::map::HashMap<alloc::string::String, i32, std::hash::random::RandomState, alloc::alloc::Global>"),
            Some(StdKind::HashMap)
        );
        assert_eq!(kind("std::collections::hash::set::HashSet<u8, std::hash::random::RandomState>"), Some(StdKind::HashSet));
        assert_eq!(kind("alloc::collections::btree::map::BTreeMap<u8, char, alloc::alloc::Global>"), Some(StdKind::BTreeMap));
        assert_eq!(kind("alloc::rc::Rc<alloc::string::String, alloc::alloc::Global>"), Some(StdKind::Rc));
        assert_eq!(kind("alloc::sync::Arc<u64, alloc::alloc::Global>"), Some(StdKind::Arc));
        assert_eq!(kind("alloc::rc::Weak<u8, alloc::alloc::Global>"), Some(StdKind::RcWeak));
        assert_eq!(kind("alloc::sync::Weak<u8, alloc::alloc::Global>"), Some(StdKind::ArcWeak));
        assert_eq!(kind("core::cell::RefCell<alloc::vec::Vec<u8, alloc::alloc::Global>>"), Some(StdKind::RefCell));
        assert_eq!(kind("core::cell::Cell<i32>"), Some(StdKind::Cell));
        assert_eq!(kind("&str"), Some(StdKind::Str));
        assert_eq!(kind("&[u32]"), Some(StdKind::Slice));
        assert_eq!(kind("&mut [u8]"), Some(StdKind::Slice));
        assert_eq!(kind("&[u8; 4]"), None);
        assert_eq!(kind("values::Vec<u8>"), None, "a user type named Vec is not std's");
        assert_eq!(kind("alloc::vec::Vec"), None, "Vec without arguments is not the generic");
        assert_eq!(kind("alloc::vec::into_iter::IntoIter<u8, alloc::alloc::Global>"), None);
        assert_eq!(kind("core::cell::RefCell"), None);
        assert_eq!(kind("core::cell::UnsafeCell<i32>"), None);
    }

    #[test]
    fn version_gates_order_paths() {
        let deque = layout_for("alloc::collections::vec_deque::VecDeque<i32, alloc::alloc::Global>").unwrap();
        let v195 = RustcVersion::new(1, 95, 0);
        let v197 = RustcVersion::new(1, 97, 0);
        assert_eq!(deque.paths(Role::Head, Some(v195))[0], &["head"]);
        assert_eq!(deque.paths(Role::Head, Some(v197))[0], &["head", "__0"]);
        assert_eq!(deque.paths(Role::Head, Some(v197)).len(), 2, "the other version's path is a fallback");
        assert!(Versions::since(1, 96).contains(RustcVersion::new(1, 96, 0)));
        assert!(!Versions::until(1, 96).contains(RustcVersion::new(1, 96, 1)));
    }
}
