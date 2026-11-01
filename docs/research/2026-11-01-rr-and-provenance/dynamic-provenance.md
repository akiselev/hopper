# Dynamic (run-time) provenance, taint and dataflow tracking for hopper

Scope: run-time and instrumentation techniques for "click a value, see where it came from" in hopper, the
rr-based time-travel debugger for native Linux, mostly Rust, programs. The compile-time side (MIR,
Flowistry, rustc_public) is covered by a sister researcher. Prior context, not redone here:
`/home/user/ideas/devtools/time-travel-debugger.md`. That note already covers Pernosco's parallel shard index, rr's
(event, ticks) time coordinate, fork checkpoints, Tralfamadore and SteamDrill at a high level.

Research date 2026-11-01. Facts with a date are marked. All overhead figures are the source's own numbers on its
own benchmarks. Several (PolyTracker's "negligible", remix's results) are self-reported.

## 1. Shadow-memory taint and provenance engines: label model, overhead, origin capability, licence

### Takeaway
None of the off-the-shelf engines delivers "every byte's full origin" for Rust at low cost. DFSan builds into Rust only on
nightly, on x86_64 Linux. It has 8 OR-able labels, and as of April 2026 users still could not get the build to link.
PolyTracker answers the "byte origin = input offset" question best (2^31−1 labels, a TDAG output), but it is a C/C++
DFSan fork. The binary-level engines (libdft, Dytan, Taintgrind, PANDA taint2) work on any binary, Rust included, but
they cost 1.1x to 50x. They are also either GPL or built on Pin, which is proprietary. None of them was designed to run
on an rr replay. The useful thing to take from them is their design: the label models and shadow layouts.

### Cited Findings
**LLVM DataFlowSanitizer (DFSan)**
- DFSan supports "up to 8 labels". Base labels are 8-bit powers of two, and union labels are their bitwise OR. Clang
  describes it as "a work in progress", "under development for x86_64 Linux only". — [Clang DFSan docs](https://clang.llvm.org/docs/DataFlowSanitizer.html)
- Shadow memory is 1 byte per application byte (`shadow = addr ^ 0x500000000000`), and label 0 means unlabelled. With
  origin tracking, each 4-byte-aligned group of application bytes shares one 4-byte origin ID: 4 bits of depth and a
  28-bit chain hash. A chain is a stack ID plus a pointer to the previous chain. "the origin of the last labeled
  operand propagates" when an instruction has several labelled operands. `origin_history_size` defaults to 16. — [DFSan design doc](https://clang.llvm.org/docs/DataFlowSanitizerDesign.html)
- `-dfsan-track-origins=1` records the stores a labelled value passes through. It "roughly doubles" the slowdown and
  adds about 1x memory. Mode `=2` also records loads, at about 4x. Code that is not instrumented (assembly, other
  languages) needs an ABI list (`uninstrumented` / `discard` / `functional` / `custom`). Neither doc gives a figure for
  base DFSan overhead. — [Clang DFSan docs](https://clang.llvm.org/docs/DataFlowSanitizer.html), [design doc](https://clang.llvm.org/docs/DataFlowSanitizerDesign.html)
- Rust: `-Zsanitizer=dataflow` is listed in the unstable book with one supported target, `x86_64-unknown-linux-gnu`.
  Pairing it with `-Zbuild-std` is "strongly recommended", and `-Cllvm-args=-dfsan-...` passes options through. The
  feature is tracked as #39699/#89653 and is still unstable. — [Rust unstable book: sanitizer](https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)
- State in April 2026: a user building with `-Zsanitizer=dataflow -Zsanitizer-dataflow=abilist.txt
  -Cunsafe-allow-abi-mismatch=sanitizer` and `-Zbuild-std` got link failures (`open64.dfsan`, `dup.dfsan`,
  `_Unwind_RaiseException.dfsan`, `__libc_start_main`). Replies suggested `-Zexternal-clangrt`, `_Unwind_*` abilist
  entries and `-Clink-arg=-lc -lpthread -ldl`. No working result was reported. — [users.rust-lang.org thread, 2026-04-28..30](https://users.rust-lang.org/t/how-to-use-dataflow-sanitizer/139842)
- LLVM is still adding DFSan platforms: SystemZ support was added in PR #168991 (2025). — [llvm-project PR 168991](https://github.com/llvm/llvm-project/pull/168991)

**PolyTracker (Trail of Bits)**
- An LLVM pass that tracks "which bytes of an input file are operated on by which functions". It started "as a fork of
  the LLVM DataFlowSanitizer". It tracks up to 2^31−1 taints, against the limit of 16 for DFSan origin tracking, and
  can track "every byte of input at once". Output is a binary TDAG (taint DAG / taint forest with parent labels),
  read through a Python API. Apache-2.0. The README covers C/C++ only ("C++ support is still in progress") and does
  not mention Rust. Its own claim is "negligible performance overhead for almost all inputs". The worst case is a byte
  tainted by many input bytes, as in compression and crypto. — [PolyTracker repo](https://github.com/trailofbits/polytracker)
- Published as a tool demo at ISSTA/ECOOP 2024, where it won a Distinguished Tool Demo award ("modest performance
  overhead", a runtime trace in the co-designed TDAG format). v4.0.0 replaced SQLite with the TDAG format. — [ISSTA 2024 listing](https://2024.issta.org/details/issta-ecoop-2024-tool-demonstrations/7/PolyTracker-Whole-Input-Dynamic-Information-Flow-Tracing), [releases](https://www.github.com/trailofbits/polytracker/releases)

**libdft (Pin)**
- Byte-granular tags, either 1 bit (bit-sized) or 8 bits ("up to 8 distinct values or colors"), stored in a
  process-wide "tagmap". Registers are shadowed in a per-thread vcpu structure. EIP, EFLAGS and segment registers are
  not tracked. Implicit (control) flow is not tracked. Slowdown is 1.14x–6.03x on command-line utilities and
  1.25x–4.83x on Apache and MySQL. The paper notes that Dytan reported a 30x slowdown on gzip. Pin v2.9, 32-bit x86
  (VEE 2012). — [libdft VEE'12 paper](https://www.cs.columbia.edu/~angelos/Papers/2012/libdft_vee2012.pdf)
- libdft64 (angr fork; Pin 3.x, x86-64, basic SSE/AVX) uses "byte level" granularity with the BDD tag-set structure
  from Angora, so labels are sets rather than 8 colours. It ignores implicit flows and eflags. "Performance
  optimization" is a TODO. — [angr/libdft64](https://github.com/angr/libdft64)
- libdft-dr re-implements the libdft64 model on DynamoRIO instead of Pin. — [tosanjay/libdft-dr](https://github.com/tosanjay/libdft-dr)

**Dytan (Pin, ISSTA 2007)**
- A generic taint framework with customisable sources and sinks that covers both data-flow and control-flow tainting.
  The authors' slides report about 50x overhead with both enabled. The paper argues that ignoring control flow "can lead
  to unsafety". Won the ISSTA 2017 Impact Paper Award. — [Dytan paper](https://sites.cc.gatech.edu/fac/Alex.Orso/papers/clause.li.orso.ISSTA07.pdf), [slides](https://sites.cc.gatech.edu/fac/Alex.Orso/papers/clause.li.orso.ISSTA07.presentation.pdf), [award](https://conf.researchr.org/event/issta-2017/issta-2017-agenda-issta-2017-impact-paper-award-dytan-a-generic-dynamic-taint-analysis-framework)

**Taintgrind (Valgrind)**
- Bit-precise shadow memory taken from Memcheck and Flayer. Explicit flow only: no propagation through control
  structures or tainted-pointer dereferences. Sources are given by file and byte range (`--file-filter`,
  `--taint-start`, `--taint-len`), stdin or network, or `--taint-all`, which it warns is "slow!". Output is a
  per-instruction log of "address | instruction | type | runtime values | information flow" that can be piped to
  Graphviz (`log2dot.py`) or emitted as SMT-LIBv2. GPLv2. Supports Valgrind 3.27.1 (entry dated 2026-06-15) on
  x86/amd64/arm Linux. — [wmkhoo/taintgrind](https://github.com/wmkhoo/taintgrind)
- Valgrind's typical slowdown is "on the order of 20x–50x" (Miri paper, 2026). — [Miri POPL 2026 paper](https://research.ralfj.de/papers/2026-popl-miri.pdf)

**DynamoRIO / Dr. Memory**
- Dr. Memory is a memory checker, not a taint tool. Its shadow-memory layer, Umbra, is a DynamoRIO extension ("Dr.
  Memory Framework", DynamoRIO ≥ 5.0.0). It lets a client create and access shadow memory "without knowing the actual
  shadow memory address", and does not restrict the shadow size. — [Umbra docs](https://dynamorio.org/page_drmf_umbra.html), [Dr. Memory Umbra page](https://drmemory.org/page_umbra.html), [Umbra project](https://people.csail.mit.edu/qin_zhao/umbra)
- DynamoRIO taint add-ons are small community projects: "DrTaint" (an RPI thesis library, ARM Umbra adaptation),
  dynTaintTracer ("currently ARM only") and libdft-dr. — [RPI thesis](https://dspace.rpi.edu/server/api/core/bitstreams/61dd006f-db12-405a-8c7e-885b33633a13/content), [dynTaintTracer](https://www.github.com/vanhauser-thc/dynTaintTracer), [libdft-dr](https://github.com/tosanjay/libdft-dr)
- DynamoRIO is BSD-licensed (already cited in the prior note). — [DynamoRIO License.txt](https://github.com/DynamoRIO/dynamorio/blob/master/License.txt)

**PANDA / QEMU taint2**
- Labels are sets per byte ("c = a + b carries the set {1, 2}"). Memory is shadowed in a large mmap'd area, and
  registers are shadowed by register number and byte offset. It works by translating TCG to LLVM and inserting taint
  ops (inline or as calls, with an optional optimisation pass). The example runs under `-replay`. The docs estimate
  it is about 10x faster than the original `taint` plugin but give no absolute numbers. Caps:
  `max_taintset_card` and `max_taintset_compute_number`. APIs include `taint2_label_ram`, `taint2_query_set_ram`
  and `taint2_query_tcn_ram`. The tcn is the "taint compute number", the depth of computation from the source. It is
  "not generally useful by itself" without input plugins (`file_taint`) and query plugins (`tainted_branch`). — [taint2 README](https://github.com/panda-re/panda/blob/dev/panda/plugins/taint2/README.md)

**The general problem: address dependencies**
- Pointer tainting, where a value loaded through a tainted address inherits the taint and which is what a table lookup
  or hash map needs, was found "impractical (and incur many false positives still)" in most forms (Slowinska & Bos,
  EuroSys 2009). The rebuttal "Tainting is Not Pointless" (Kozyrakis et al.) argues it is still useful for
  memory-corruption protection. — [talks.cam.ac.uk abstract](https://talks.cam.ac.uk/talk/index/17038/), [Tainting is not pointless](https://mast.stanford.edu/pubs/tainting_is_not_pointless/)

### Inferences
- **"Byte origin = input offset"** is answered natively by PolyTracker (one label per input byte, a DAG of unions) and
  by Taintgrind or PANDA `file_taint` (per-offset labels). DFSan cannot do it: 8 labels is too few. libdft64's BDD
  tag sets could.
- **"Last write PC"** is not a taint question. It is a write log ("which instruction last stored to this byte"), which
  is what Pernosco's database and DFSan origin chains (stack ID per store, depth ≤ 16) give. For hopper, store a
  last-writer event per byte, not labels. Provenance then becomes a graph walk over the write log plus each
  instruction's semantics (which inputs fed this store), not a label propagated forward.
- Forward taint answers "where did input byte k go". Hopper's click asks "where did this value come from", which is
  backward. Doing that backwards on demand over a write log is cheaper than forward full-provenance taint, because
  only the queried value's cone is walked. Zhang & Gupta's demand-driven slicing makes the same trade (section 4).
- Licences: Taintgrind, Valgrind, PANDA (QEMU-derived) and Unicorn are GPLv2. DynamoRIO is BSD. PolyTracker, Miri,
  icicle and bytehound are Apache/MIT. A permissive hopper should take ideas, not code, from the GPL tools.
- DFSan for Rust: even if it links, recording a DFSan build under rr changes the program under test. rr does support
  sanitizer builds (section 2), but it is a separate build configuration, not a replay-time analysis.

### Gaps
- No published overhead for base DFSan, or for PolyTracker on real workloads. The ISSTA 2024 paper's evaluation was not
  read in full.
- Not established whether anyone has built Rust programs with PolyTracker. Its LLVM version would also have to match
  rustc's LLVM.
- Intel Pin's licence (proprietary, free of charge) was not fetched from a primary source in this pass.
- PANDA's licence was not read from the repository. GPLv2 is inferred from its QEMU base.

## 2. Running instrumentation during rr replay instead of during recording

### Takeaway
You cannot run Valgrind, DynamoRIO or Pin on rr's replay. Pernosco instead wrote its own replay-only binary
instrumenter, "remix". It is closed source and beat DynamoRIO and Valgrind on a null tool, partly because it can see
the future. SteamDrill used static binary rewriting over Arnold replays rather than Pin. An emulator replay between rr
events is feasible in principle: the trace stores registers and memory writes per frame, plus a tick count. The one
hard requirement is counting retired conditional branches exactly as rr's hardware counter does, so that async
signals and context switches land at the right instruction. Remix does exactly this in software. Nobody has published
an emulator-seeded-from-rr replayer.

### Cited Findings
- On whether `valgrind rr replay` checks the application, rr's author wrote: "No. If you do that, Valgrind will not
  check the application." Binary instrumentation of replay is possible "but has to be a bit different", and had been
  implemented "in a closed-source branch" (2017). In 2022 khuey closed the issue, noting that rr works with ASan/MSan
  builds and "there is little need for Valgrind proper". — [rr issue #16](https://github.com/rr-debugger/rr/issues/16)
- **rr remix** (O'Callahan, December 2020) is a new instrumentation framework for rr replays, built for Pernosco,
  which "needs heavy binary instrumentation of rr replays" such as observing every memory write. Goals: replay any rr
  recording (self-modifying code included), allow arbitrary instrumentation, keep overhead low on browser-sized
  applications, and stay simple. A side goal is replaying on machines without perf counters or CPUID faulting, or with
  incompatible XSAVE layouts. With a null tool on clang++: "On optimized clang++, remix of an rr replay beats both
  DynamoRio and Valgrind instrumenting a normal run". Remix does no inlining. "we aren't in a position to open-source
  remix at this time." — [rr remix post](https://robert.ocallahan.org/2020/12/rr-remix-efficient-replay-only-binary.html)
- Remix's use of foreknowledge: it scans the whole recording for an address range no replay process ever touches and
  places the engine there. rr says in advance whether a step will end in a segfault, so fragments "have no early
  exits"; "once a fragment has been entered, it is guaranteed to run to completion". Remix does not use hardware
  counters; it "adds instructions to count the conditional branches explicitly". Invariant: each replay step leaves
  memory and registers exactly as native execution would, so instrumented and native execution can alternate.
  Pernosco runs several replays at once, each instrumenting a different time interval. — [Exploiting precognition in binary instrumentation](https://robert.ocallahan.org/2020/12/exploiting-precognition-in-binary.html)
- rr's progress counter: x86 has one deterministic counter, retired conditional branches (RCB). rr pairs the RCB count
  with the full register set to identify a point for async signal or context-switch delivery. The counter interrupt
  fires late ("skid"), so rr programs it to stop early, then sets a breakpoint at the target PC and repeats until the
  RCB count and registers match. — [rr paper, arXiv 1705.05937](https://arxiv.org/pdf/1705.05937)
- rr checkpoints use `fork` (copy-on-write) and delay creating non-main threads. A checkpoint "typically takes less than
  ten milliseconds". — [rr paper](https://arxiv.org/pdf/1705.05937)
- **What a trace contains** (rr master `rr_trace.capnp`): per `Frame`, a `tid`, per-task total `ticks`, the full
  `registers` and `extraRegisters`, and `memWrites` (tid, addr, size, zero `holes`, `sizeIsConservative`). Events
  include `syscall`, `sched`, `signal`/`signalDelivery`/`signalHandler`, `instructionTrap`, `patchSyscall`,
  `syscallbufFlush` and `syscallbufAbortCommit`. The `mmaps` file records each mapping with
  `source: zero | trace | file(backingFileName)`, and `tasks` records clone/exec/exit. The header carries
  `ticksSemantics`. — [rr_trace.capnp](https://raw.githubusercontent.com/rr-debugger/rr/master/src/rr_trace.capnp)
- `rr dump -D` dumps memory at syscalls or signals, `-p` dumps mmap data, `-b` dumps syscallbuf contents, `-r` gives
  machine-parseable output. — [rr(1) man page](https://manpages.ubuntu.com/manpages/jammy/man1/rr.1.html)
- rr.soft (sidkshatriya) is a modified rr that records and replays without hardware counters, using "lightweight
  dynamic (and static) instrumentation". It is slower, needs `-W` at replay as well, and upstreaming it would be "a
  large feature patch". — [rr.soft](https://github.com/gmh5225/rr.soft), [HN discussion](https://hn.svelte.dev/item/43481652)
- **SteamDrill / OmniTable (OSDI '22)** is built on Spark SQL and the Arnold record/replay system. It parallelises
  replay into epochs from checkpoints taken during recording. Its authors say Pin-style dynamic instrumentation "is a
  scalability bottleneck" when replays are parallelised, so it "performs static binary instrumentation". It
  disassembles binaries and rewrites basic blocks to call cursors at the breakpoints a query needs, and single-steps
  only for cursors that produce no breakpoints. Instrumenting every instruction "imposes a many orders of magnitude
  slowdown", so it analyses filter clauses, for example evaluating only at malloc and free. Its time column is
  (count of backward control-flow transfers, IP), computed from Intel's deterministic branch and call counters plus
  counted unconditional backward branches. Queries ran 2–290x faster than gdb on one core (geometric mean 17x) and
  6.9–1809x on 64 cores (geometric mean 99x). 64 cores gave 10.5x over sequential, and multi-replay resolution
  3.6x. — [SteamDrill paper PDF](https://www.usenix.org/system/files/osdi22-quinn.pdf), [OSDI '22 page](https://www.usenix.org/conference/osdi22/presentation/quinn)
- Intel's PinPlay-style Program Record/Replay Toolkit runs Pintools on replays, with the restriction that "Pintools that
  change application control flow (with PIN_ExecuteAt() for example) cannot work with replay". — [Intel Program Record/Replay Toolkit](https://intel.com/content/www/us/en/developer/articles/tool/program-recordreplay-toolkit.html)
- **Emulators.** icicle is an "experimental fuzzing-specific, multi-architecture emulation framework" in Rust with its
  own SLEIGH runtime. It has x86_64-linux and aarch64-linux examples, p-code `CodeInjector` hooks and a "Linux
  userspace emulator" component (`icicle-linux`). MIT/Apache-2.0. — [icicle-emu](https://github.com/icicle-emu/icicle-emu)
  Unicorn is a QEMU-based CPU emulator ("High performance via Just-In-Time compilation", "fine-grained
  instrumentation"), GPLv2, with no OS or syscall layer described. — [unicorn-engine](https://github.com/unicorn-engine/unicorn)
  QEMU's own record/replay needs icount mode and a snapshot, and replays non-deterministic events. — [QEMU replay docs](https://qemu.org/docs/master/system/replay.html)
- A search for any published bridge that seeds Unicorn or QEMU from an rr trace found nothing. — (negative result;
  search "rr recording emulator replay unicorn qemu seeded from rr trace memory snapshot", 2026-11-01)

### Inferences
- **Design for an emulator-between-events replayer** (hopper's "remix-lite"). It follows from the trace schema above;
  none of it is tested:
  1. Run rr replay to frame *N* (or restore a fork checkpoint). Read registers through the gdb protocol and the
     replay tracee's memory, or rebuild memory from `mmaps` plus the recorded `memWrites` from the start.
  2. Emulate user-space instructions until the next frame's trigger: a syscall instruction, or the tick count of a
     `sched`/`signal` frame.
  3. At a syscall, do not execute it. Apply that frame's `memWrites` and returned registers, and continue. This is
     exactly the replay rule rr itself uses for most syscalls.
  4. Count ticks with rr's `ticksSemantics`, as remix does in software. Where a frame's ticks are reached, match the
     registers as rr does, to allow for code with no conditional branches.
  5. Cross-check the emulator by comparing its registers and memory digest to rr's at each frame boundary. This is
     free fault detection, because rr's frames store the full register file.

  The hard parts:
  - `syscallbuf`: syscalls buffered in-process are flushed as batches.
  - `patchSyscall`: rr rewrites syscall sites.
  - Trapped `rdtsc`/`cpuid` (`instructionTrap`).
  - vDSO.
  - Multithreaded `sched` switches.
  - AVX and x87 coverage in the emulator.
  - Shared memory with other processes.

  A single-threaded CPU-bound geometry test hits few of these. That is the same "near-ideal workload" argument the
  prior note makes.
- The cheaper first step, with no emulator: instrument by **static rewriting or breakpoints inside rr replay**, as
  SteamDrill does. Malloc/free events, function entries and named decision sites need only breakpoints. Single-step
  only inside a small window. Per-instruction dataflow over a whole run needs either the emulator or a DynamoRIO-class
  JIT that is rr-aware.
- Running DynamoRIO inside an rr replay tracee would perturb the tick counter and the memory layout. The engine's
  pages would collide with the address space the recording expects. This is why remix places its engine in a
  never-used range and counts branches itself. Treat "DynamoRIO on rr replay" as unsupported. This is an inference
  from the remix posts, which call the problem "a bit different", and from rr #16.
- icicle (Rust, permissive, p-code hooks) suits a hopper emulator better than Unicorn (GPLv2). Its x86-64 SLEIGH
  coverage of AVX2/AVX-512 is unverified, and that matters for optimised Rust and `f64` SIMD code.

### Gaps
- Pernosco's dataflow database layout and remix's tick-exactness technique are unpublished beyond the 2020 posts. The
  promised follow-up posts on the basic architecture and dataflow analysis were not found.
- No measured speed for icicle or Unicorn on x86-64 Rust code, and no AVX coverage figures.
- Not verified whether rr's `ticksSemantics` on current Intel and AMD parts exactly equals "conditional branches
  retired" as an emulator would count them. AMD and Zen differences were not checked.

## 3. Rust-specific run-time sources: Miri, compile-time probes, tracing, allocation tracking

### Takeaway
Miri is deterministic, permissively licensed, and already tracks provenance and borrow tags. But it is about
3000–7000x slower than unoptimised native code and cannot generally call FFI, so it suits only small pure-Rust tests
as an "omniscient recorder". For CADabra's Boolean cases that means hours, not seconds, and Parasolid calls are out.
Allocation tracking at the allocator boundary is cheap and mature: DHAT, the `dhat` crate, bytehound. Hopper can get
it for free in replay by breaking on the allocator entry points.

### Cited Findings
- Miri detects UB, including uninitialised reads, out-of-bounds, use-after-free and invalid values. Stacked Borrows
  is the default and Tree Borrows is opt-in (`-Zmiri-tree-borrows`). Other flags: `-Zmiri-strict-provenance` and
  `-Zmiri-provenance-gc` (default every 10,000 basic blocks). It is isolated and deterministic for a given
  `-Zmiri-seed` (allocation addresses, scheduling, weak memory), with `-Zmiri-deterministic-concurrency`.
  `-Zmiri-track-pointer-tag=<tag>` prints a backtrace when a tag is created or popped. `-Zmiri-track-alloc-id=<id>`
  (or `miri_track_alloc` at run time) and `-Zmiri-track-alloc-accesses` trace allocation events and accesses.
  Apache-2.0/MIT. — [rust-lang/miri README](https://github.com/rust-lang/miri)
- `-Zmiri-native-lib` is "experimental", Unix only and "unsound". It allows only integer and pointer arguments.
  Native-allocated memory is not accessible from Rust, and Miri stops tracking initialisation and provenance for
  memory shared with native code. — [Miri README](https://github.com/rust-lang/miri)
- Speed (POPL 2026 paper; rustc 1.90.0-nightly 2025-06-28; i7-12700H; array fill-and-sum benchmark against a native
  build at `-Copt-level=0`): "Miri is about 3000x slower than the native code" at the smallest *n* and "about 7000x"
  at the largest. The authors call it "too slow for typical fuzzing setups". Its main weaknesses are "low performance,
  and the fact that Miri cannot execute Rust programs that call native libraries via FFI". A large speedup would need
  "a version of Miri built as a sanitizer". Over more than 100,000 crates, more than 70% of tests pass under Miri and
  fewer than 8% time out. — [Miri POPL 2026 paper](https://research.ralfj.de/papers/2026-popl-miri.pdf), [POPL 2026 listing](https://popl26.sigplan.org/details/POPL-2026-popl-research-papers/50/Miri-Practical-Undefined-Behavior-Detection-for-Rust)
- Native-call internals: as of March 2025, in native-call mode "the interpreter memory itself is accessed directly by
  external code via pointers created from integers and passed via libffi" (rust PR #138352). A tracing-based FFI
  design draft (stopping and restarting execution and changing page protections around native calls) "suffers" in
  performance. — [rust-lang/rust PR 138352](https://github.com/rust-lang/rust/pull/138352), [nia-e FFI design note](https://hackmd.io/@nia-e/SJUzIeo-eg)
- Tree Borrows (PLDI 2025, distinguished paper) replaces the Stacked Borrows stack with a tree. On the 30,000 most-used
  crates it "rejects 54% fewer test cases than Stacked Borrows". — [Ralf Jung blog, 2025-07-07](https://www.ralfj.de/blog/2025/07/07/tree-borrows-paper.html), [PLDI 2025 listing](https://pldi25.sigplan.org/details/pldi-2025-papers/42/Tree-Borrows)
- **Valgrind DHAT** tracks each block's lifetime in instructions ("repeatable") and inspects "every memory access" to
  attribute bytes read and written to blocks. `malloc(100)` then `realloc(200)` counts as two blocks; the 100-byte
  copy is added to read and write counts and the original allocation point is kept. It also has `--mode=copy`
  (memcpy profiling) and `--mode=ad-hoc`. — [DHAT manual](https://valgrind.org/docs/manual/dh-manual.html)
- **The `dhat` crate** (0.3.3) installs a global allocator wrapper and writes `dhat-heap.json`. It does not track
  memory accesses, only measures inside `main`, and is "experimental". A block allocated before profiling and
  reallocated within it "is treated as a new allocation". — [dhat docs.rs](https://docs.rs/dhat/latest/dhat/)
- **bytehound** is an LD_PRELOAD profiler that "gathers every allocation and deallocation, along with full stack
  traces", with a custom cheap unwinder. It supports jemalloc through the `jemallocator` crate (AMD64). Apache-2.0/MIT. — [koute/bytehound](https://github.com/koute/bytehound)

### Inferences
- **Miri as an omniscient recorder.** Its determinism plus `track-alloc-id`/`track-pointer-tag` already give
  object-level histories ("this allocation was created here, this tag was invalidated here") that no binary tool can
  derive. A custom driver over Miri's interpreter could log every MIR-level read, write and retag with source
  provenance. That is unverified: the Machine-hook API was not checked in this pass. Budget: at 3000–7000x versus
  *unoptimised* native, which is itself much slower than release, a 1 s release-mode Boolean case would run for an hour
  or more. Use it as a "deep dive on a minimised case" mode, not the default. CADabra's oracle tests call Parasolid
  under Wine through FFI, so they are out entirely.
- Rust semantic events (moves, clones, drops, borrows) cannot be seen at machine level: a move is a `memcpy` or nothing
  at all. The options are compile-time probes (the sister researcher's area) or DWARF plus MIR-to-PC maps applied to
  the machine-level write log. A probe build changes the binary, so it is "a different recording", like DFSan.
- The `tracing` crate gives semantic spans and events at a cost the developer chooses. CADabra's `decision-trace` /
  `DecisionLedger` is the same idea specialised (prior note). In replay, breakpoints on its emit function make these
  events free to index, because they are a few call sites, not every instruction.

### Gaps
- Miri performance on float-heavy, optimised-style code and after the 2025 Tree Borrows optimisations was not
  measured. The only number found is the POPL microbenchmark.
- Whether Miri exposes stable hooks (a `Machine` trait used by out-of-tree drivers) for an external event logger was
  not checked from a primary source.
- No source found for a production MIR or LLVM pass that emits move, clone and drop events at run time for Rust.

## 4. Dynamic slicing and "why" queries at machine level: following values through arithmetic, maps and serialisation

### Takeaway
Pernosco follows copies only, through memory and registers. It stops at immediates, I/O and arithmetic, and keeps
idempotent writes. Full dynamic dependence graphs are 1–2 GB even for executions of about 130M instructions unless they
are compacted (about 94 MB) or built on demand from a trace on disk. Address dependencies (hash maps, table lookups)
are the known failure mode of taint: following them over-taints. The practical answer is a backward, demand-driven
walk with explicit user choices at each fork, not exhaustive forward propagation.

### Cited Findings
- Pernosco "detects copying through memory and registers and follows dataflow backwards through them". Explanations
  "terminate when you reach a point where a value was derived from something other than a CPU copy", such as an
  immediate, I/O or arithmetic. There is "no particular reason" to stop at arithmetic except that the resulting DAGs are
  harder to visualise. Pernosco keeps idempotent writes: for `x = 0; y = 0; x = y;` it shows `x = y` and `y = 0`,
  where gdb+rr breaks at `x = 0`. — [O'Callahan, Explaining dataflow in Pernosco (2019)](https://robert.ocallahan.org/2019/11/explaining-dataflow-in-pernosco.html)
- Zhang & Gupta compacted dynamic dependence graphs to roughly 20–210 MB from 0.84–1.95 GB, representing only about 6%
  of edges explicitly by sharing representative edges between statement pairs (PLDI 2004). Zhang's thesis reports
  graphs averaging 2 GB shrunk to 94 MB for executions averaging 130M instructions, and precise slicing time cut from up
  to 20 minutes (demand-driven) to 16 s. — [PLDI 2004 paper](https://www.cs.purdue.edu/homes/xyzhang/Comp/pldi04.pdf), [Zhang thesis](https://www.cs.ucr.edu/~gupta/Thesis/xyzhang.pdf)
- The ICSE 2003 lazy-preprocessing (LP) algorithm keeps the trace on disk and builds only the part of the dependence
  graph a slicing query needs. — [ICSE 2003 paper](https://www.cs.purdue.edu/homes/xyzhang/Comp/icse03.pdf)
- PANDA taint2's "taint compute number" measures how many computations separate a byte from its source, and
  `max_taintset_card` caps label-set growth. Both are mechanisms against blow-up. — [taint2 README](https://github.com/panda-re/panda/blob/dev/panda/plugins/taint2/README.md)
- PolyTracker's worst case is "a single byte tainted by many input bytes" (compression, crypto). The TDAG shares union
  nodes. — [PolyTracker repo](https://github.com/trailofbits/polytracker)
- Pointer tainting is found mostly "impractical" with false positives. — [Slowinska & Bos, EuroSys 2009 abstract](https://talks.cam.ac.uk/talk/index/17038/)
- Whyline's "why did / why didn't" questions doubled developer speed. Event-logging omniscient debuggers hit walls at
  10^7–10^9 events (prior note's citations). — [Ko & Myers 2008](https://faculty.washington.edu/ajko/papers/Ko2008JavaWhyline.pdf)

### Inferences
- **Recommended semantics for hopper's click**, layered:
  - **(a) copy chain.** The Pernosco default: exact and cheap, and through Rust's moves it covers most "where did this
    come from" questions in move-heavy code.
  - **(b) one-step arithmetic expansion on demand.** At an arithmetic node, show the operands as children and let the
    user pick which to follow. This is a lazy DDG in LP style, with no exhaustive forward taint.
  - **(c) address dependencies shown, never followed automatically.** For a load through a computed address
    (`map[key]`), show "value came from slot X; slot address was computed from key K". The user chooses to follow the
    data (the earlier insert into slot X, via the copy chain) or the key. This avoids the pointer-tainting blow-up.
  - **(d) the hash-map insight.** "Where did the value in the map come from" is answered by the copy chain to the
    `insert`. The hash is only an address computation.
  - **(e) serialisation.** Byte-wise encoders are copies plus shifts and ors. A rule that treats shift/or/mask
    combinations of whole bytes as "packing" keeps the chain intact. PolyTracker's per-byte input offsets show this
    works for parsers.
- Using f64 provenance for CADabra: the interesting values come from arithmetic (an intersection point is computed, not
  copied). Layer (b) plus semantic anchors (`site!` decision events) matters more than deep copy chains.

### Gaps
- No source found that measures how Pernosco or any commercial tool handles hash maps specifically.
- The Whyline machine-level analogue ("why didn't") needs control dependence from a CFG or post-dominators of the
  binary. Not researched here.

## 5. Heap object identity over time (malloc/free, Rust's global allocator, realloc, moves)

### Takeaway
Object identity comes from logging the allocator entry points (alloc, realloc, dealloc with return value, size and
call stack) and chaining realloc results. That is cheap enough to do at record time, and free at replay time with
breakpoints. Moves of the owning header (a Vec's ptr/cap/len) are ordinary copies, so the copy-chain dataflow follows
them. Together these give "this Vec's buffer was reallocated 3 times, here".

### Cited Findings
- DHAT treats realloc as a new block that keeps the original allocation point, and counts the implicit copy as
  reads and writes. — [DHAT manual](https://valgrind.org/docs/manual/dh-manual.html)
- The `dhat` crate does this at the Rust global-allocator layer (a wrapper `GlobalAlloc`) and treats an in-window
  realloc of an earlier block as a new allocation. — [dhat docs.rs](https://docs.rs/dhat/latest/dhat/)
- bytehound records "every allocation and deallocation, along with full stack traces" via LD_PRELOAD, including
  jemalloc users. — [bytehound](https://github.com/koute/bytehound)
- Miri gives each allocation an AllocId and can trace its creation, frees and accesses (`-Zmiri-track-alloc-id`,
  `-Zmiri-track-alloc-accesses`). — [Miri README](https://github.com/rust-lang/miri)
- SteamDrill prunes instrumentation to "malloc and free" for heap queries, which is cheap even in replay. — [SteamDrill paper](https://www.usenix.org/system/files/osdi22-quinn.pdf)
- rr runs one thread at a time and gives a total (event, ticks) order. — [rr paper](https://arxiv.org/pdf/1705.05937)

### Inferences
- **Hopper heap index.** In replay, set breakpoints at Rust's allocator entry points (the global allocator's
  alloc/realloc/dealloc as compiled, plus libc malloc/realloc/free for FFI) and log (time, ptr, size, align, caller
  PCs). Build an interval map from address to allocation-epoch ID, valid over time intervals. A realloc that returns
  a new pointer creates a successor edge, giving the "object lineage" chain. Each historical address resolves to
  (allocation, offset) for its moment, so the dataflow UI can say "byte 16 of the 3rd buffer of Vec at `foo.rs:42`".
- **Owner identity.** A Vec's identity across moves is the copy chain of its `ptr` field. The chain plus the
  realloc lineage covers the case "moved into a struct, then reallocated during push". DWARF type info at the
  allocation site's caller (the `RawVec` grow frame) labels the buffer's element type. The Rust symbol names to break
  on (`__rust_alloc`, `__rdl_alloc`, `__rust_realloc`) were not verified in this pass. They vary with
  `#[global_allocator]` and LTO.
- Allocator calls are rare compared with instructions, so this can run in a quick indexing pass with no full
  instrumentation, even under plain rr breakpoints.

### Gaps
- Not verified: the exact allocator shim symbol names in current rustc (2026), and whether they survive LTO and
  inlining in release builds.
- Arena and bump allocators inside the program (common in geometry kernels) are invisible at the allocator boundary
  and would need per-crate hooks.

## 6. Overhead budgets (record time against offline index time) and the recommendation

### Takeaway
At record time, keep rr alone: under 2x, from the prior note. Optionally add allocator logging, which bytehound and
`dhat` show is cheap enough. Push all per-instruction provenance into offline, parallel replay instrumentation, where
10–100x per shard is acceptable because shards run in parallel and only on the windows asked for. Use Miri only as an
opt-in deep mode for minimised pure-Rust cases (3000–7000x).

### Cited Findings (overhead table, source figures only)
| Mechanism | Label / record model | Overhead (source) | Rust | Can run on rr replay? | Licence |
|---|---|---|---|---|---|
| rr record | syscall results, memWrites per frame, ticks | <2x ([rr paper](https://arxiv.org/abs/1705.05937)) | yes, any binary | n/a | MIT ([rr repo](https://github.com/rr-debugger/rr)) |
| DFSan | 8 OR labels, 1 B shadow per byte; origins as 4 B per 4 B, chain depth 16 | origins ~2x on top of DFSan (mode 1), ~4x (mode 2); base not published ([Clang](https://clang.llvm.org/docs/DataFlowSanitizer.html)) | nightly, x86_64 Linux, link problems reported April 2026 ([forum](https://users.rust-lang.org/t/how-to-use-dataflow-sanitizer/139842)) | as an instrumented build recorded by rr; not a replay analysis ([rr #16](https://github.com/rr-debugger/rr/issues/16)) | Apache-2.0 with LLVM exception (LLVM) |
| PolyTracker | up to 2^31−1 labels, TDAG unions, input offsets | "negligible" (self-reported) ([repo](https://github.com/trailofbits/polytracker)) | not documented | no | Apache-2.0 |
| libdft / libdft64 | 1-bit or 8-bit tags per byte (VEE'12); BDD tag sets (libdft64) | 1.14–6.03x utilities, 1.25–4.83x servers ([VEE'12](https://www.cs.columbia.edu/~angelos/Papers/2012/libdft_vee2012.pdf)) | any binary | no (Pin; JIT engine perturbs the replay) | libdft64 licence not named on page ([repo](https://github.com/angr/libdft64)) |
| Dytan | configurable; data and control flow | ~50x with control flow ([slides](https://sites.cc.gatech.edu/fac/Alex.Orso/papers/clause.li.orso.ISSTA07.presentation.pdf)); 30x gzip ([libdft paper](https://www.cs.columbia.edu/~angelos/Papers/2012/libdft_vee2012.pdf)) | any binary | no | not checked |
| Taintgrind | bit-precise Memcheck shadow; per-instruction log | Valgrind class, 20–50x typical ([Miri paper](https://research.ralfj.de/papers/2026-popl-miri.pdf)) | any binary | no ([rr #16](https://github.com/rr-debugger/rr/issues/16)) | GPLv2 ([repo](https://github.com/wmkhoo/taintgrind)) |
| PANDA taint2 | label sets per byte, tcn depth | ~10x faster than the old taint plugin (no absolute figure) ([README](https://github.com/panda-re/panda/blob/dev/panda/plugins/taint2/README.md)) | any guest | runs on PANDA's own replay, not rr | GPLv2 (inferred from the QEMU base) |
| rr remix (Pernosco) | arbitrary tool on rr replay | null tool faster than DynamoRIO/Valgrind native ([post](https://robert.ocallahan.org/2020/12/rr-remix-efficient-replay-only-binary.html)) | any binary | yes, built for it | closed source |
| SteamDrill | SQL cursors via static rewriting on Arnold replays | queries 17x (1 core) / 99x (64 cores) faster than gdb, geometric mean ([paper](https://www.usenix.org/system/files/osdi22-quinn.pdf)) | any binary | Arnold, not rr | not checked |
| Miri | AllocId, borrow tags, provenance per byte | 3000–7000x versus `-O0` native ([POPL'26](https://research.ralfj.de/papers/2026-popl-miri.pdf)) | pure Rust; FFI experimental | no (own interpreter, deterministic) | Apache-2.0/MIT |
| DHAT / dhat crate / bytehound | per-allocation records (+ per-access for DHAT) | DHAT "slowly"; the crate is cheaper than DHAT ([dhat](https://docs.rs/dhat/latest/dhat/)) | yes | allocator breakpoints in replay | GPL (Valgrind) / Apache-MIT |

- Pernosco's offline index build uses "10s to 100s of GB" and saturates a 36-core machine for about an hour on a large
  trace (prior note). — [O'Callahan Rust-Pernosco slides](https://robert.ocallahan.org/assets/images/Rust-Pernosco.pdf)
- Parallel replays per time interval are how Pernosco spreads instrumentation cost. — [Exploiting precognition](https://robert.ocallahan.org/2020/12/exploiting-precognition-in-binary.html)

### Inferences (recommendation)
1. **Record time: plain rr only.** Do not record sanitizer or probe builds by default. They make "the program you
   debug" differ from the program that failed, and DFSan for Rust is not reliably buildable as of 2026-04. Allow an
   optional "semantic build" profile, in which `tracing`, CADabra's `decision-trace` or compile-time move probes are
   enabled, recorded as a separate trace and labelled as such.
2. **Index time, phase A (cheap, breakpoint-only, inside rr replay).** Allocator events become heap lineage. Function
   entries become the call tree. Named semantic events (`site!` decisions, `tracing` callsites) are captured too. This
   is SteamDrill-style pruning: about 10^3–10^6 events, little cost.
3. **Index time, phase B (per-instruction, for windows).** Build a hopper replay engine: an emulator seeded from rr
   state that advances between rr frames, applying recorded `memWrites` and registers at syscalls, counting branches in
   software for ticks, and verifying against rr's per-frame registers. It emits a last-writer log of (time, PC,
   address range, source operands: memory locations or registers). icicle (Rust, MIT/Apache, p-code hooks) is the
   first candidate; the fallback is a custom x86-64 interpreter for the instruction subset rustc emits. Run one shard
   per checkpoint interval in parallel. A 10–100x slowdown per shard is acceptable offline, which is an inference from
   Pernosco's one-hour/36-core budget and the seconds-long CADabra cases.
4. **Query time.** Backward, demand-driven walks over the last-writer log (LP-style). Copy chains are followed by
   default, arithmetic is expanded one level on request, and address dependencies are shown but not followed. Heap
   offsets are mapped to (allocation lineage, offset) and to DWARF types. No forward taint is stored.
5. **Optional deep mode.** Miri with alloc and tag tracking, for minimised pure-Rust reproductions, to show
   borrow-level history (which reference invalidated which) that machine code cannot express.
6. **Do not** build on Valgrind, Pin, DynamoRIO or Unicorn for replay instrumentation. They do not run on rr replays,
   and Valgrind and Unicorn are GPL. Pernosco had to write remix for the same reason.

### Gaps
- No measured cost exists for an emulator-between-rr-events replayer. The 10–100x figure is a planning assumption.
  The first experiment should be: emulate one recorded CADabra Boolean case frame by frame with icicle, check the
  registers at every frame against rr, and time it.
- No figure found for rr replay speed when stopping at about 10^5 breakpoints, which decides phase A's cost.
