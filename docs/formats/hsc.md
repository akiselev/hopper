# The `.hsc` ownership sidecar, format version 1

An `.hsc` file holds the ownership events of a Rust binary's functions, taken from the final
optimised MIR of every function instance that rustc codegen'd. `cargo hopper-build` writes one
beside every executable it builds (`target/debug/app` gets `target/debug/app.hsc`). The debugger
reads it to relabel machine-level dataflow ("memcpy at PC p") as source-level ownership ("`b` ←
move `a`, line 13"), and to tell a live variable from a moved-out or dropped one.

The reader and writer are `crates/hopper-sidecar` (`codec.rs` holds the byte layout, `model.rs`
the records). This document and `codec.rs` must stay in step; a change to either changes
`FORMAT_VERSION`.

## Identity: the build-id

A sidecar describes exactly one linked binary, identified by its GNU build-id (the descriptor of
the `NT_GNU_BUILD_ID` note in `.note.gnu.build-id`). hopper-build links with `-Wl,--build-id`.
`Sidecar::open_for_binary` reads the binary's build-id (with the `object` crate) and refuses a
sidecar whose recorded build-id differs, or that is an unlinked partial, with
`HopperError::StaleSidecar`. Spans and MIR change between rustc versions, so the binary must come
from the same rustc invocation that wrote the events; hopper-build guarantees this by writing the
events from a rustc driver callback during the build itself.

The same layout also serves the per-crate *partials* the driver writes before linking (binding 1
below). They live in `<target>/<profile>/hopper-partials/<crate>-<stable crate id, 16 hex>.hsc`
and are merged into the binary's sidecar after `cargo build` finishes.

## Layout

```
offset 0   8 bytes   magic: 89 48 53 43 0D 0A 1A 0A   ("\x89HSC\r\n\x1a\n")
offset 8   u32 LE    format version (1)
offset 12  ...       one zstd frame (with content checksum) holding the body
```

The prefix is uncompressed so that a reader can reject another format version before
decompressing. The magic follows PNG's: the high first byte and the CR LF / ^Z / LF tail catch
transfers that mangle binary files. Readers cap the decompressed body at 2 GiB.

### Primitive encodings (inside the body)

| Name | Encoding |
|---|---|
| `uvar` | unsigned LEB128: 7 bits per byte, least significant group first, high bit set on every byte but the last; at most 10 bytes; must fit `u64` |
| `byte` | one byte |
| `bytes` | `uvar` length, then that many bytes |
| `str` | `bytes` holding UTF-8 |
| `sid` | `uvar` index into the string table |
| `opt-sid` | `uvar`: 0 for none, `index + 1` for a string |

A count of items (`uvar`) may not exceed the number of bytes left; readers reject it otherwise.
Every byte of the body is consumed; trailing bytes are an error.

### Body

```
header
string table:  uvar count, then count × str
functions:     uvar count, then count × function
```

#### Header

```
byte binding
  0 = linked binary:   bytes build-id
  1 = partial:         str crate name
                       uvar stable crate id (rustc's StableCrateId as u64)
                       byte output
                         0 = library
                         1 = linked:  str path rustc wrote (absolute)
                                      uvar count, count × uvar stable crate ids it links
str  rustc version     ("rustc 1.97.0-nightly (f53b654a8 2026-04-30)")
uvar count, count × str crate names   (the crates whose functions the file holds)
str  tool              ("hopper-build 0.1.0")
```

Header strings are inline (not in the string table) so the header can be read alone.

#### Function

```
sid   symbol       mangled instance symbol (the symbol table's and DW_AT_linkage_name's name)
sid   demangled    the symbol demangled without its hash ("worked_example::load")
sid   def path     with generic arguments, no trimmed paths ("std::vec::Vec::<Config>::push")
sid   crate        the crate that defines the item
span  def span
byte  origin       0 source, 1 macro expansion (#[derive(..)] and the like),
                   2 compiler shim (drop glue, vtable shims), 3 dependency (another crate's
                   function, a std generic instantiated here)
uvar  count, count × event     in MIR order: by basic block, then statement, then terminator
```

Functions are sorted by symbol and unique by symbol (shared generics can be codegen'd by more
than one crate; the merge keeps one).

#### Span

```
sid   file         as DWARF names it: the (possibly remapped) path rustc used, joined with
                   its working directory when relative
uvar  lo line      1-based
uvar  lo column    1-based, in bytes from the start of the line (DWARF's column)
uvar  hi line − lo line
uvar  hi column    exclusive
```

A span is the one codegen gives the statement's instructions: macro expansions are collapsed to
the function body's context (`walk_chain_collapsed`), and a call terminator uses the call's
`fn_span` (`v.push(c)` → the span starting at `push`), as `codegen_call_terminator` does. So an
event's start is the `(file, line, column)` of the DWARF line row of its instructions.

#### Event

```
byte    kind          see below
span    span
uvar    count, count × inline frame:   sid callee def path, span call site
                      the MIR inlining chain, outermost first; empty for the function's own code
uvar    basic block
uvar    position      0 = the block's terminator, i + 1 = statement i
byte    control path  0 normal, 1 behind a drop flag, 2 unwind (cleanup block) only
opt-sid src
opt-sid dst
uvar    type          0 = none; otherwise sid + 1 of the type's name, then
        uvar size     0 = unknown; otherwise size in bytes + 1
opt-sid callee        for calls (every event of a call terminator) and drops
byte    caveats       bit 0: in unsafe code; bit 1: touches interior mutability (UnsafeCell)
```

### Event kinds

The codes are `hopper_core::api::OwnershipKind`.

| Code | Kind | Emitted for | src → dst |
|---|---|---|---|
| 0 | Move | an operand of a type that is not `Copy`, used as a value | place → place |
| 1 | Copy | an operand of a `Copy` type, used as a value; `CopyForDeref` | place → place |
| 2 | Clone | a call whose callee is `<T as Clone>::clone` | the receiver's referent (`b`) → destination (`c`) |
| 3 | SharedBorrow | `&place`, `&raw const place` | place → destination |
| 4 | MutBorrow | `&mut place`, `&raw mut place` | place → destination |
| 5 | Projection | precedes a borrow of a projected place (field, index, deref, downcast) | base place → projected place |
| 6 | Drop | a `Drop` terminator | dropped place → none |
| 7 | Aggregate | `Rvalue::Aggregate`; one operand event per field follows, into `dst.field` | none → destination |
| 8 | Literal | a constant operand used as a value | the constant's text (`"alpha"`, `3`) → destination |
| 9 | CallReturn | a call that is not a clone | `callee(..)` → destination |
| 10 | StorageDead | `StorageDead` of a local with a debug name | the local → none |

"Used as a value" means: the right side of an assignment (`Use`, `Cast`, `Repeat`,
`WrapUnsafeBinder`), an aggregate field, or a call argument. Operands of arithmetic, comparisons
and discriminant reads are not ownership transfers and emit nothing. A call terminator emits one
operand event per argument (dst `callee(arg i)`), then its Clone or CallReturn event, all at the
terminator's location and the call's span.

**Move or Copy follows the type,** not the MIR operand. Optimised MIR writes `copy _1` for the last
use of a value that is not `Copy` (`Config { name: copy _1, retries: const 3_u32 }` at `-O0` in
the worked example), which in the source is a move, and `move _4` for temporaries of `Copy`
types, which in the source is a copy.

### Places as text

Places are rendered the way the source writes them:

- a local is its debug name (`var_debug_info`), `return` for the return place, or `_N` for a
  compiler temporary. Names declared inside MIR-inlined callees apply only to inlined statements;
- a field is `.name` (from the type's field names; tuple fields and closure captures by index or
  capture name); `(*x).f` reads `x.f`; an enum field reads `(x as Some).0`;
- an index is `x[i]` or `x[3]`; a deref is `*x`;
- a temporary assigned once is rendered through its value: `&place` for a borrow, the place it
  copies, and `base[idx]` / `*x` for the result of `Index::index` / `Deref::deref`. So
  `_7 = &((*_8).name)` with `_8 = <Vec<Config> as Index<usize>>::index(move _9, const 0)` and
  `_9 = &_5` (`v`) renders as `v[0].name`. As a *destination*, a temporary that only holds a copy
  or a constant is named raw (`_8`).

Integer literals drop their type suffix (`0_usize` → `0`); a literal's text is cut at 96
characters.

### Control paths and drop flags

Drop elaboration guards the drop of a maybe-moved local with a drop flag: a bool temporary only
ever assigned constants, tested by `switchInt`. The blocks that test leads to hold a drop that
runs only while the value is still owned: their events are *behind a drop flag* (1). Cleanup
blocks run only while unwinding (2). The local-state API ignores unwind events, and a guarded
drop after a move out does not make the local "dropped".

### Caveats

Labels describe what MIR did even in unsafe code, but ownership-based pruning ("after the move
`a` is dead, skip its aliases") is only sound where borrowck's guarantees hold. An event is
flagged

- **unsafe** (bit 0) when its statement lies inside an `unsafe` block or `unsafe fn` of a
  workspace crate (from HIR; an enclosing block covers closures in it), and conservatively for
  every event whose code has no local HIR: other crates' functions, std code inlined into a user
  function, and compiler shims;
- **interior mutability** (bit 1) when the value's type, or the type of any place the event
  reads or writes or projects through, is not `Freeze` (it contains an `UnsafeCell`).

## Indexes

The reader builds two indexes when it loads a file (they are not stored): by name (mangled
symbol, demangled name, def path; a def path of a generic can name several instances) and by
`(file, start line)` → events, where file names match by path-component suffix after lexical
normalisation (`main.rs` matches `/w/src/main.rs`; `a/x/../b` is `a/b`).

## The join (summary)

`Sidecar::join` takes a PC's DWARF frame (physical function symbol, inline chain, line row), its
callers, and the observed machine effect (bytes read and written, their sizes, and the
source-level places the engine resolved for them). It uses the innermost frame that is the
user's own code (origin `source`, or `macro expansion` when the PC is in it directly), so a
`memcpy` deep inside `Vec::push` is attributed to the call `v.push(c)`; at a caller frame only the
call terminator's events count. Among the events at the row (narrowed by column, and by the
inline chain), it picks by the written and read places, then by size. Confidence: `Exact` for a
single candidate (Projection and StorageDead events write nothing and do not count against an
observed write), `Inferred` when the effect or the call picked one of several, `Heuristic` for a
move or copy with no machine copy observed (a rename, as in optimised code).

## The source view (summary)

The join labels one PC with the MIR event its instruction carries out, so a chain of machine hops
through `v.push(c)` reads `c` → `_8` (the argument temporary) and stores inside `Vec::push`. The
engine lifts each hop to the user-level event it is part of (`hopper_sidecar::source`), then
collapses consecutive hops with the same event:

- the hop's *user frame* is the innermost frame of origin `source` (`Sidecar::user_frame`; a
  derive, a std generic and a shim belong to the call that reached them). Hops in the same user
  frame instance at the same line are one event, decided by the newest of them;
- a hop inside a callee is what the call did with the bytes, from where they are once the call
  returns, named in the user frame's places (`Sidecar::call_transfer`): a clone call is its clone
  event; bytes inside the call's destination are its return value (with its literal arguments
  underneath); bytes in memory owned by the receiver (argument 0, `&mut v`) are the by-value
  argument moved into it, named by the element that holds them (`c` → `v[0]`);
- a store into a user function's return place is the caller's call (`Sidecar::returned_value`):
  `a` ← `load()`, with the aggregate that built the return place (`Config { name, .. }`, the field
  the bytes are in shown, the rest elided) and the chosen event (`name` → `return.name`)
  underneath;
- a value reached through a reference starts with the write of that reference (the borrow
  `n = &v[0].name` for `*n`), kept apart from the value's own hops.

The machine hops stay in the chain, each carrying its user-level label.

## Example

`fixtures/worked-example`, `main` at `-O0` (`hsc-dump target/debug/worked-example.hsc --function
worked_example::main`; first lines):

```
12:13 call-return load(..) -> a : Config (32) via load bb0[term]
13:13 move a -> b : Config (32) bb1[0]
14:13 shared-borrow b -> &b : &Config (8) bb1[1]
14:15 copy &b -> <Config as Clone>::clone(arg 0) : &Config (8) via <Config as std::clone::Clone>::clone bb1[term]
14:15 clone b -> c : Config (32) via <Config as std::clone::Clone>::clone bb1[term]
16:7 move c -> Vec::<Config>::push(arg 1) : Config (32) via std::vec::Vec::<Config>::push bb3[term]
17:13 projection v[0] -> v[0].name : String (24) bb5[0]
17:13 shared-borrow v[0].name -> n : &String (8) bb5[0]
19:1 drop v : Vec<Config> (24) via std::ptr::drop_in_place::<std::vec::Vec<Config>> bb9[term]
19:1 drop c : Config (32) via std::ptr::drop_in_place::<Config> bb15[term] [unwind]
```

That fixture's debug sidecar is about 10 KB: 39 function instances, 386 events.
