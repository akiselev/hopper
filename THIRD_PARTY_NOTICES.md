# Third-party notices

hopper adapts code and design from the projects below. Each adapted file says so in its header
comment.

## BugStalker

<https://github.com/godzie44/BugStalker>, commit `6817f52b48a890d5d0ee3b78155bed1357fceb5b`.

Adapted in:

- `crates/hopper-eval/src/location.rs`: the DWARF expression requirement loop
  (`src/debugger/debugee/dwarf/eval.rs`), reading memory through hopper's `Memory` trait instead
  of ptrace.
- `crates/hopper-eval/src/view.rs`: the Rust std specialisations (Vec, VecDeque, String, HashMap
  and HashSet over hashbrown, BTreeMap and BTreeSet, Rc, Arc, Cell, RefCell) and Rust enum variant
  selection (`src/debugger/variable/value/specialization/{mod,hashbrown,btree}.rs`,
  `src/debugger/variable/value/parser.rs`).
- `crates/hopper-eval/src/layouts.rs`: the rustc-version switch for std layouts (`version_switch!`).
- `crates/hopper-symbols/src/dwarf.rs`: per-unit parts parsed lazily on first use (`UnitLazyPart`).

```
MIT License

Copyright (c) 2026 Derevtsov Konstantin

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust's debugger providers

`src/etc/gdb_providers.py`, `src/etc/lldb_providers.py` and `src/etc/rust_types.py` from
<https://github.com/rust-lang/rust> (as shipped with rustc 1.97.0, `2d8144b78`), dual-licensed
under the Apache License 2.0 and the MIT license. They are the reference for the std type layouts
and type-name patterns in `crates/hopper-eval/src/layouts.rs` and the summary formats in
`crates/hopper-eval/src/view.rs`.

```
Copyright (c) The Rust Project Contributors

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```
