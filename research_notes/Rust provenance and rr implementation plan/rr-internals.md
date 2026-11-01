# rr internals and operating constraints, as the foundation for hopper

Research date: 2026-11-01. Source code is cited at rr `master` commit `ca20dcbfae9b088ee891fe020de562a240faacfe` (committed 2026-10-29), cloned shallowly. Source links have the form `https://github.com/rr-debugger/rr/blob/ca20dcb/src/<file>#L<line>`. The release in use is rr 5.10.0 (15 Oct 2025), the latest; there are no newer releases ([releases](https://github.com/rr-debugger/rr/releases)). Earlier context (`/home/user/ideas/devtools/time-travel-debugger.md`, `.notes/engines.md`) was read first. Its findings are not repeated here: rr's <2x overhead, the user-space design, 5.9 working with `perf_event_paranoid=2` on kernel 6.10+, the `bc`/`bs` packets, and Pernosco's product page.

## 1. The gdbserver protocol as rr implements it, and the cost of each operation

### Takeaway
rr's server speaks plain GDB-RSP plus four rr additions:
- reverse `bc`/`bs`;
- `vRun` with an encoded target (event number, `t<ticks>` or `c<checkpoint>`);
- a `qRRCmd:<name>:<tid>[:args]` escape that reaches rr's own commands (`when`, `when-ticks`, `when-tid`, `checkpoint`, `elapsed-time`, ...);
- reverse continue implemented as "replay forward from an earlier checkpoint and remember the last stop".

It runs exactly one thread at a time and debugs one process (thread group). It advertises no fork or exec events. It has 4 x86 hardware watchpoint slots and no software watchpoints of its own. A headless hopper client can drive all of this without gdb.

### Cited Findings
**Packets**
- `qSupported` reply: `PacketSize=100000` (hex), `QStartNoAckMode+`, `qXfer:features/auxv/exec-file/siginfo:read+`, `qXfer:siginfo:write+`, `multiprocess+`, `hwbreak+`, `swbreak+`, `ConditionalBreakpoints+`, `vContSupported+`, `QPassSignals+`. `ReverseContinue+;ReverseStep+` are added when reverse execution is compiled in. The reply has no `fork-events`, `vfork-events` or `exec-events` — [GdbServerConnection.cc#L685-L716](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L685-L716).
- The top-level dispatcher handles these packet letters: `b c D g G H j k m M p P q Q T v x X z Z ! ? _`. Anything else is "Unhandled debugger request" — [GdbServerConnection.cc#L1301-L1514](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1301-L1514).
- Queries handled include `qC`, `qAttached`, `qfThreadInfo`/`qsThreadInfo`, `qGetTLSAddr`, `qOffsets`, `qSymbol`, `qTStatus`, `qXfer`, `qSearch:memory`, `qMemoryRegionInfo`, and the LLDB-only `qHostInfo`, `qProcessInfo`, `qShlibInfoAddr` and others — [GdbServerConnection.cc#L643-L841](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L643-L841).
- Reverse execution: `bc` is reverse continue and `bs` is reverse step (`RUN_BACKWARD`). Any other `b` packet is unhandled — [GdbServerConnection.cc#L1001-L1016](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1001-L1016).
- `vCont?` answers `vCont;c;C;s;S;`. No `r` (range step) and no `t` are offered — [GdbServerConnection.cc#L1139-L1141](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1139-L1141).
- `Z`/`z` types 0–4 are accepted: software breakpoint, hardware breakpoint, and write, read and access watchpoints. Conditions arrive as agent-expression bytecode (`;X<len>,<bytes>`) — [GdbServerConnection.cc#L1454-L1497](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1454-L1497). rr evaluates them itself ([GdbServerExpression.cc](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerExpression.cc)).

**Seeking (`vRun`)**
- `vRun;<hex-encoded arg>` is rr's seek. An argument `c<N>` restores checkpoint N and `t<N>` seeks to tick count N. A plain number seeks to event N, and no argument restarts from the previous point. A filename is fatal ("we don't support that") — [GdbServerConnection.cc#L1155-L1204](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1155-L1204).
- Seek to event: `seek_to_before_event(e)` restores the nearest earlier checkpoint, then `replay_step_forward(RUN_CONTINUE)` runs until the target is reached. The event is clamped to `final_event - 1` — [GdbServer.cc#L1801-L1818](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1801-L1818).
- Seek to ticks: `compute_time_from_ticks` scans the trace for the event that contains the tick count of the current task, with ticks counted from that task's last CLONE/EXEC. It then calls `seek_to_ticks(time, ticks)` — [GdbServer.cc#L1690-L1720](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1690-L1720), [GdbServer.cc#L1774-L1784](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1774-L1784).

**`qRRCmd` and the commands it reaches**
- Packet shape: `qRRCmd:<name>:<target_tid>[:<arg>]*`, with `:` separating the arguments — [GdbServerConnection.cc#L619-L640](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L619-L640).
- gdb sends it through a generated Python `RRCmd` class using `maint packet qRRCmd:%s:%d`. LLDB uses `process plugin packet send qRRCmd:...` — [DebuggerExtensionCommandHandler.cc#L106-L120](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommandHandler.cc#L106-L120), [#L246](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommandHandler.cc#L246).
- The full command set is in [DebuggerExtensionCommand.cc#L14-L227](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommand.cc#L14-L227):
  - `elapsed-time` and `absolute-time`: the trace frame's `monotonic_time`.
  - `when`: "the number of the last completely replayed rr event".
  - `when-ticks`: "the current rr tick count for the current thread".
  - `when-tid`: the real tid.
  - `info recording` and `when-end`: the last event in the recording.
  - `rr-history-push`, `back`, `forward`.
  - `checkpoint`, `delete checkpoint`, `info checkpoints`.
- Replies are human-readable strings such as `"Current tid: " + tid` and `"Checkpoint N at <where>"`. They are not structured — [DebuggerExtensionCommand.cc#L69](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommand.cc#L69), [#L175](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommand.cc#L175).
- `checkpoint` fails when `timeline()->can_add_checkpoint()` is false, that is, when the current session cannot be cloned — [DebuggerExtensionCommand.cc#L157-L175](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DebuggerExtensionCommand.cc#L157-L175), [ReplayTimeline.h#L110](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.h#L110).

**Threads and processes**
- Thread ids are `p<tgid>.<tid>` with multiprocess, or bare `<tid>` without it, in hex — [GdbServerConnection.cc#L1823-L1833](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServerConnection.cc#L1823-L1833).
- Only one thread runs. "We can only run task |t|; neither diversion nor replay sessions support running multiple threads. So even if gdb tells us to continue multiple threads, we don't do that." If gdb resumes a different thread, rr lets the scheduled task run anyway — [GdbServer.cc#L1088-L1105](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1088-L1105).
- A debug session targets one process. `restart_session` keeps "targeting the same process no matter what is running when we hit the event". The `-p/--onprocess` and `-f/--onfork` options choose which process — [GdbServer.cc#L1801-L1806](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1801-L1806), [ReplayCommand.cc#L32-L49](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayCommand.cc#L32-L49).

**Watchpoints**
- x86 has `NUM_X86_WATCHPOINTS = 4`. If the request needs more debug registers, `set_x86_debug_regs` clears DR7 and returns false, so the `Z2` request fails — [Task.cc#L67](https://github.com/rr-debugger/rr/blob/ca20dcb/src/Task.cc#L67), [Task.cc#L1922-L1931](https://github.com/rr-debugger/rr/blob/ca20dcb/src/Task.cc#L1922-L1931), [AddressSpace.cc#L2059-L2076](https://github.com/rr-debugger/rr/blob/ca20dcb/src/AddressSpace.cc#L2059-L2076).
- The server replies to the watchpoint request with that result. gdb then falls back to its own software watchpoints, which single-step — [GdbServer.cc#L690-L710](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L690-L710).
- Memory written by the kernel during a replayed syscall cannot trip a debug register. rr instead compares watched values after a step (`has_any_watchpoint_changes`, `check_for_watchpoint_changes`), so watchpoints also fire on syscall writes — [ReplaySession.cc#L1070](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplaySession.cc#L1070), [ReplaySession.cc#L2182](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplaySession.cc#L2182), [AddressSpace.h#L570](https://github.com/rr-debugger/rr/blob/ca20dcb/src/AddressSpace.h#L570).

**How reverse continue works**
- `reverse_continue` runs these steps — [ReplayTimeline.cc#L943-L1010](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.cc#L943-L1010) (and on to L1170):
  1. It takes `end = mark()`.
  2. It seeks to a checkpoint before `end` (`seek_to_before_key`).
  3. It applies the breakpoints and watchpoints and replays forward to `end` (`replay_step_to_mark`), remembering every stop.
  4. If the run stopped, the last stop is the answer. It is refined with `reverse_singlestep`.
  5. If there was no stop, it seeks to an even earlier checkpoint and repeats.
- At a breakpoint it single-steps with breakpoints disabled. On AArch64, where watchpoints fire before the instruction takes effect, it single-steps the faulting instruction (same lines).
- Repeated stops are capped (`stop_count_limit = 20`) so a hot breakpoint does not make the run quadratic — [ReplayTimeline.cc#L924](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.cc#L924).

**Checkpoints and their cost model**
- Checkpoints are `ReplaySession` clones made with `os_fork_into`, which forks each tracee process (copy-on-write) — [Session.cc#L690-L707](https://github.com/rr-debugger/rr/blob/ca20dcb/src/Session.cc#L690-L707).
- Automatic checkpoints are placed on an exponential schedule, giving at most O(log L) checkpoints. In `LOW_OVERHEAD` mode they are about 0.5 s of replay apart (`low_overhead_inter_checkpoint_interval = 500000` µs). The comment's rationale: "a reverse step or continue whose destination is within 0.5 should take at most a second", and with checkpoints of Firefox taking about 50 ms "this would make checkpointing overhead about 10% of replay time" — [ReplayTimeline.cc#L1521-L1556](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.cc#L1521-L1556).
- rr's own replay-cost estimate, fitted on Firefox start-up on a Lenovo W530: 0.00205 µs per tick, 39.7 µs per syscall, 0.00183 µs per byte written, and a 998 µs constant — [ReplayTimeline.cc#L1505-L1520](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.cc#L1505-L1520).
- `ReplayTimeline::mark()` "can be expensive ... since we may need to clone the current session and run it a bit" to order it relative to other marks — [ReplayTimeline.h#L92-L99](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.h#L92-L99).

**Other server pieces**
- Diversions (used for gdb's `call`/`print f()`): `DiversionSession` "lets you run task(s) forward without replay". It emulates some syscalls and "essentially ignores the syscalls it doesn't know how to implement". It is meant for short-lived diversions — [DiversionSession.h#L14-L25](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DiversionSession.h#L14-L25), [GdbServer.cc#L1158](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1158).
- LLDB support exists through `rr replay -d lldb`. It is forward-only, because LLDB has no reverse commands — [rr releases 5.8.0](https://github.com/rr-debugger/rr/releases).

### Inferences
- Approximate cost of each operation, from the cost model and the checkpoint strategy above:
  - Seek to event: restoring a fork checkpoint (about 50 ms for a large process, per the comment) plus forward replay to the target. With the default spacing that replay is up to about 0.5 s; with dense explicit checkpoints it can be almost zero.
  - Reverse continue to a watchpoint: one forward replay of each checkpoint interval it searches backwards through, with 4 hardware watchpoints at native speed. The cost grows with how far back the last write lies.
  - More than 4 watched words means gdb's software watchpoints, a single step per instruction.
- These cost figures are estimates read from code comments and model constants. They are not measurements, and no benchmark was run here: the container has no PMU (see §5).
- hopper should use `vRun` with `t<ticks>` and event numbers to seek, and `qRRCmd:when`/`when-ticks` to read its position. Neither needs a gdb process. The replies are free text, so the client must parse strings that may change between rr versions.
- Multi-process traces need one `rr replay -p <pid>` server per process. The gdbserver gives no fork or exec events across processes.

### Gaps
- No measured per-operation latency was found in any source, for single-step over RSP, seek or reverse continue. This needs a benchmark on a machine with a PMU.
- How hardware watchpoints are handled on AArch64 (number of slots) was not read.

## 2. rr's time model: events, ticks and naming a point in time

### Takeaway
An rr "time" is an event number (`FrameTime`, global and totally ordered across threads, because only one thread runs at a time) plus, inside an event, the current thread's tick count. Ticks are a per-thread count of retired conditional branches that resets at clone and exec. rr itself says `(event, ticks, step key)` "does not uniquely identify a program state". Inside one tick count it disambiguates by register contents and return addresses. A stable global timestamp for an index is therefore `(event, tid, ticks, instruction pointer and registers, or an in-tick instruction ordinal)`, and it is valid only for one trace file with one rr version.

### Cited Findings
- `MarkKey = (FrameTime trace_time, Ticks ticks, ReplayStepKey step_key)`. "These values do not uniquely identify a program state, but they are intrinsically totally ordered." The marks database holds "a time-ordered list of Marks associated with each MarkKey" — [ReplayTimeline.h#L260-L300](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.h#L260-L300).
- What a mark stores to disambiguate inside a key: a proto-mark with `Registers regs` and a `ReturnAddressList`. A full mark adds `ExtraRegisters` — [ReplayTimeline.h#L315-L354](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayTimeline.h#L315-L354).
- Trace schema (`rr_trace.capnp`):
  - `FrameTime` (Int64, >0) and `Ticks` (Int64, ≥0).
  - `TicksSemantics` is `retiredConditionalBranches` or `takenBranches` ("Excludes interrupts, far branches, and rets").
  - Each `Frame` holds `tid`, "Per-task total tick count" `ticks`, `monotonicSec` ("The baseline is unspecified"), `memWrites`, `arch`, `registers`, `extraRegisters`, and an event union (`sched`, `signal`, syscalls, ...).

  Source: [rr_trace.capnp#L20-L32](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L20-L32), [#L333-L363](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L333-L363).
- Ticks reset per task: "EXEC and CLONE reset the ticks counter" — [GdbServer.cc#L1699-L1712](https://github.com/rr-debugger/rr/blob/ca20dcb/src/GdbServer.cc#L1699-L1712).
- `Frame.memWrites` holds only the "Userspace writes performed by this event", meaning syscall results and other recorded data. Ordinary instruction stores are not recorded, because replay recomputes them — [rr_trace.capnp#L342-L343](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L342-L343).
- `rr rerun` names string-instruction repeats as one instruction "if not interrupted". Its single steps also include steps where state changes with no user instruction run (syscall exit) — [RerunCommand.cc#L50-L57](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L50-L57).
- Trace compatibility is all-or-nothing. `TRACE_VERSION 85`. A different version gives "incompatible version ... you'll need to replay with that older version" and exits — [TraceStream.cc#L56](https://github.com/rr-debugger/rr/blob/ca20dcb/src/TraceStream.cc#L56), [TraceStream.cc#L1648-L1659](https://github.com/rr-debugger/rr/blob/ca20dcb/src/TraceStream.cc#L1648-L1659).
- The trace header carries a `uuid` — [rr_trace.capnp Header](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L72-L87).

### Inferences
- For hopper's index key, use `(trace uuid, event, tid, ticks, k)`, where `k` is the instruction ordinal since the last tick change, counted by single-stepping or by instrumentation. `(event, ticks)` alone collides inside straight-line code with no conditional branches. The `(regs, return addresses)` trick that rr uses is not a usable index key.
- The event number is a valid global order only because rr serialises threads. Within an event only one thread runs, so `(event, ticks)` with the tid is a total order.
- An index must pin the rr version (TRACE_VERSION) of its trace and store the trace uuid.

### Gaps
- Whether ticks are exactly reproducible between `rr replay` and `rr rerun --singlestep`'s `icount` field was not measured. `icount` is listed as a rerun field ([RerunCommand.cc#L48](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L48)), but its definition was not read.

## 3. Other rr commands and machine-readable output

### Takeaway
Of these commands only `rr traceinfo` and `rr sources` print JSON. `rr dump -r` is a "more easily machine-parseable" text format, `rr ps` is tab-separated, and `rr rerun` writes binary records. The trace itself is Cap'n Proto plus compressed substreams, with a schema in the repo, but it is versioned per rr release with no compatibility promise. hopper should either link `rr_trace.capnp` and pin the rr version, or parse `rr dump -r`.

### Cited Findings
- **`rr dump`**: `[<event-spec>...]` takes numbers, ranges (`1000-5000`) or `end`. Its flags are `-b` syscallbuf, `-e` task events, `-m` recorded-data metadata, `-p` mmaps, `-r` "a more easily machine-parseable format", `-s` statistics and `-t <tid>` — [DumpCommand.cc#L38-L52](https://github.com/rr-debugger/rr/blob/ca20dcb/src/DumpCommand.cc#L38-L52).
- **`rr traceinfo`**: prints JSON with `xcr0`, `bindToCpu`, `cpuidFaulting`, `requiredForwardCompatibilityVersion`, `ticksSemantics`, the CPUID records, `chaosMode`, `exclusionRange`, `maxVirtualAddressSize`, `cpuImproperlyConfigured`, `uname`, `environ` and `program` — [TraceInfoCommand.cc#L56-L143](https://github.com/rr-debugger/rr/blob/ca20dcb/src/TraceInfoCommand.cc#L56-L143). It was introduced in 5.3.0 "for external tools working with traces" — [releases](https://github.com/rr-debugger/rr/releases).
- **`rr ps`**: tab-separated `PID PPID EXIT CMD`, one row per process, built from task events — [PsCommand.cc#L106-L141](https://github.com/rr-debugger/rr/blob/ca20dcb/src/PsCommand.cc#L106-L141).
- **`rr sources`**: prints JSON with `relevant_binaries` and `loaded_elf_binaries` (plus more) and accepts `--substitute=LIB=PATH` and `--gdb-script`. `rr explicit-sources` takes a list of files instead — [SourcesCommand.cc#L67-L100](https://github.com/rr-debugger/rr/blob/ca20dcb/src/SourcesCommand.cc#L67-L100), [#L1206-L1216](https://github.com/rr-debugger/rr/blob/ca20dcb/src/SourcesCommand.cc#L1206-L1216).
- **`rr pack`**: makes a trace self-contained by copying the mmapped files (`--symlink` symlinks them instead) — [PackCommand.cc#L53-L57](https://github.com/rr-debugger/rr/blob/ca20dcb/src/PackCommand.cc#L53-L57).
- **`rr replay` options** — [ReplayCommand.cc#L28-L82](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayCommand.cc#L28-L82):
  - Starting point and target: `-a` (autopilot, no debug server), `-g <EVENT>` (start the server at that event), `-e` (at the end), `-p <PID>|<COMMAND>` (when that process has exec'd), `-f <PID>` (when forked).
  - Server: `-s <PORT>` (server only, port 0 picks one automatically), `-h <HOST>`, `-k` (keep listening).
  - Debugger: `-d <FILE>` (debugger binary, for example rust-gdb), `-x` (gdb script).
  - Diagnostics: `-t <EVENT>` (single-step and dump register states), `--stats=N`.
  - Files and CPU: `--serve-files` (serve files from the trace), `-u` (CPU-unbound, "may diverge").
- **Trace layout**: substreams `EVENTS`, `RAW_DATA`, `MMAPS`, `TASKS` — [TraceStream.h#L74-L86](https://github.com/rr-debugger/rr/blob/ca20dcb/src/TraceStream.h#L74-L86). The schema's file header says it "should be checked by consumers" for its "Must" constraints — [rr_trace.capnp#L1-L12](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L1-L12).

### Inferences
- Use `rr ps` and `rr traceinfo` at ingest to list processes and check the CPU and ticks semantics. Use `rr dump -r` or the capnp schema for event-level metadata: syscalls, mmaps and per-event registers.
- `rr replay -s 0 -p <pid>` is the headless entry point for each process.

### Gaps
- The exact `rr dump -r` line format was not read.
- No source states that any of these outputs is stable across releases.

## 4. Getting instruction-level data out of a replay (and what Pernosco does)

### Takeaway
Upstream rr already ships most of a "parallel instrumented replay" toolkit:
- `rr rerun` does replay without a debugger, with per-single-step register dumps (`--singlestep=ip,gp_x16,...`) over an event range;
- `--export-checkpoints` / `--import-checkpoint` fork a replay at an event and hand clones to other rr processes over a Unix socket.

Per-instruction single-stepping costs one ptrace round trip or more per instruction. The only published figure is "roughly 1000x" for single-stepping recorders. Memory writes from user code are not in the trace, so every write must be recovered by re-execution.

Pernosco gets around this with **rr remix**. It is a closed-source binary-instrumentation engine that runs inside each replay process. It counts conditional branches in software, places itself in memory the recording never uses, and runs many replays in parallel, each instrumenting a different time interval. Its speed is better than DynamoRIO or Valgrind running live, according to O'Callahan, who gives no published numbers.

### Cited Findings
**`rr rerun`**
- Purpose: "a more powerful form of 'rr replay -a'". It does "a replay without debugging support, but it provides options for tracing and dumping tracee state", for example "singlestepping through a range of trace events, dumping selected register values after each step" — [RerunCommand.cc#L11-L15](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L11-L15).
- Options: `-s/--trace-start=<EVENT>`, `-e/--trace-end=<EVENT>`, `--singlestep=<REGS>`, `--event-regs=<REGS>`, `-f/--function=<ADDR>` ("push sentinel return address and jump to <ADDR> to fake call"), `-r` raw, `-u` CPU-unbound — [RerunCommand.cc#L28-L57](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L28-L57).
- `<REGS>` fields: `event`, `icount`, `ip`, `ticks`, `flags`, `gp_x16`, `xmm_x16`, `ymm_x16`. "All data is output in little-endian binary format; records are separated by \n" (same lines).
- When single-stepping is requested, rerun replays with `RUN_SINGLESTEP` and skips partial string-instruction steps — [RerunCommand.cc#L214-L246](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L214-L246).

**Checkpoint export and import**
- `--export-checkpoints=<EVENT>,<NUM>,<FILE>`: "Run to start of <EVENT> and then export checkpoints over Unix socket at <FILE>. Exit after <NUM> connections". `--import-checkpoint=<FILE>`: "Start the replay by importing a checkpoint from another rr instance" — [RerunCommand.cc#L35-L41](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RerunCommand.cc#L35-L41).
- Mechanism: `export_checkpoints` "will return `count` + 1 times; the first `count` times in a forked child with a valid CommandForCheckpoint". The child's stdin, stdout and stderr are rebound to file descriptors sent by `SCM_RIGHTS`. Each child is `session->clone()` followed by `fork()` — [ExportImportCheckpoints.h#L26-L35](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ExportImportCheckpoints.h#L26-L35), [ExportImportCheckpoints.cc#L149-L233](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ExportImportCheckpoints.cc#L149-L233).
- It requires `session->can_clone()` — [ExportImportCheckpoints.cc#L149](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ExportImportCheckpoints.cc#L149).

**Other replay paths**
- `rr replay -t <EVENT>` single-steps and dumps registers while replaying towards the event — [ReplayCommand.cc#L66-L68](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayCommand.cc#L66-L68).
- `--intel-pt-start-checking-event` uses Intel PT to verify control flow (for debugging rr) — [ReplayCommand.cc#L73-L74](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayCommand.cc#L73-L74).
- Single-step overhead: O'Callahan (2017) put single-stepping recorders (qira, gdb's built-in reverse execution) at "roughly 1000x slowdown" — [HN 15427799](https://news.ycombinator.com/item?id=15427799) (cited in the prior engines notes).

**Pernosco and rr remix**
- Pernosco "analyzes user-submitted rr recordings to extract all program states into a database". This "requires heavy binary instrumentation of rr replays, e.g. observing every memory write", hence "rr remix" — [O'Callahan, 2020-12, rr remix](https://robert.ocallahan.org/2020/12/rr-remix-efficient-replay-only-binary.html).
- Remix's results against other engines are qualitative only. On clang++, remix on an rr replay beats DynamoRIO's and Valgrind's null tools running live, for both optimized and unoptimized code. Remix does no inlining.
- Remix is closed: "Unfortunately we aren't in a position to open-source remix at this time."
- An unplanned benefit: replay on hardware or VMs "without hardware performance counters or CPUID faulting, and with incompatible XSAVE layouts".

  Source for the three points above: [rr remix post](https://robert.ocallahan.org/2020/12/rr-remix-efficient-replay-only-binary.html).
- How remix runs:
  - The engine runs inside the replay process. rr uses PTRACE_CONT to enter it, and it runs until the replay step's stopping point.
  - The engine "disregards hardware counters and instead adds instructions to count the conditional branches explicitly".
  - Instrumented and native execution can be switched between replay steps.
  - It scans the recording for "a sufficiently large range of memory that will never be used" to hold the engine.
  - It uses single-exit fragments, because rr tells it in advance where faults will happen.
  - Pernosco runs several replays at once, each instrumenting a different time interval.

  Source: [O'Callahan, 2020-12, Exploiting precognition](https://robert.ocallahan.org/2020/12/exploiting-precognition-in-binary.html).
- O'Callahan (2024) on Pernosco: it "exploit[s] massive parallelism to build omniscient databases that instantly answer key questions" such as "what is the value in this memory location/register at time T?" and "when was the value last modified?". "Scalability of omniscience to very compute-heavy workloads is a problem", though "not as problematic as everyone assumes". "rr hasn't had any major new features in eight years" — [DEBT 2024 post](https://robert.ocallahan.org/2024/10/debt-workshop.html).
- Pernosco "precomputes all program states so shifting time is instantaneous" — [pernos.co overview](https://pernos.co/about/overview/).

### Inferences
- An instrumented-replay path is possible on upstream rr with these steps:
  1. Run `rr rerun --export-checkpoints=E_i,...` once per shard boundary, or one exporter that forks N clones at one event.
  2. Give each shard worker its checkpoint (`--import-checkpoint`).
  3. Have each worker single-step its event window and dump `ip`/`gp`/`xmm`/`ymm`.
- That path gives every register value but not memory writes. Memory writes must be derived by decoding each instruction (from `ip` plus registers) and reading its effective address. hopper would need its own instrumentation (a remix-like in-process engine, or DynamoRIO, Pin or Valgrind in a diversion or on a replay clone) to get writes cheaply.
- Single-stepping at about 1000x gives a 10 s program a cost of about 3 CPU-hours. That is tolerable only for windowed indexing, which matches the plan in the earlier notes.
- `--export-checkpoints` takes event boundaries only, so parallel shards split on events, not tick counts.

### Gaps
- No public patches or upstream branch for "replay with instrumentation" beyond `rr rerun` were found. Remix is closed source.
- No figure was found for the measured cost of a ptrace single-step under rr replay (µs per step). This needs a benchmark.
- Pernosco's database schema and its shard sizes are not public. The pages searched were pernos.co and O'Callahan's blog posts from 2019, 2020 and 2024.

## 5. Environment limits in 2026, and the check run in this container

### Takeaway
rr needs a working retired-conditional-branch PMU counter, ptrace, `perf_event_open`, and (for portable traces) CPUID faulting.
- Bare-metal Intel works.
- AMD Zen 1 to Zen 4 work with the SpecLockMap workaround, rerun after every boot or suspend; `rdrand` does not work on Zen.
- Intel hybrid CPUs are handled in 5.10.
- AArch64 Neoverse/Graviton and Apple M-series under bare-metal Linux work, but only with LSE atomics.
- In cloud VMs it works only where the PMU is virtualised: GCE C4/C4A/M4/A4X with `--performance-monitoring-unit`, and some larger AWS sizes.
- Docker needs `--cap-add=SYS_PTRACE --security-opt seccomp=unconfined`.
- **This container is a Firecracker microVM with no hardware PMU. rr 5.10.0 installs but `rr record /bin/true` fails at once.**

### Cited Findings
**Container check (5 tool calls)**
- Kernel: `Linux vm 6.18.44-fc-v185 #1 SMP PREEMPT_DYNAMIC x86_64` ("fc" is the Firecracker kernel build).
- OS and CPU: Ubuntu 24.04.5, `Intel(R) Xeon(R) Processor @ 2.10GHz`, 4 vCPUs, `/proc/cpuinfo` flags include `hypervisor` and `cpuid_fault`.
- `perf_event_paranoid` = 2.
- `/sys/bus/event_source/devices/` lists only `breakpoint msr power software tracepoint uprobe`. There is no `cpu` PMU device.
- `apt-get install rr` failed with `E: Unable to locate package rr`.
- The GitHub API/`gh` route was refused (HTTP 403, repository not enabled for this session).
- A direct download of `https://github.com/rr-debugger/rr/releases/download/5.10.0/rr-5.10.0-Linux-x86_64.deb` installed `rr version 5.10.0`.
- `rr record /bin/true` then printed `[FATAL src/PerfCounters.cc:431:start_counter()] Unable to open performance counter with 'perf_event_open'; are hardware perf events available? See https://github.com/rr-debugger/rr/wiki/Will-rr-work-on-my-system` and exited with status 1.

  Source: commands run in this session, 2026-11-01.

**Kernel**
- rr needs Linux 4.7 or later (5.7.0: `waitid()` with `__WALL`).
- It works with `perf_event_paranoid=2` on kernel 6.10+ (5.9.0).
- It works with `CONFIG_PROC_MEM_FORCE_PTRACE=y` (Debian 13 kernels, 5.10.0).

  Source: [releases](https://github.com/rr-debugger/rr/releases).
- The wiki still says "you will likely have to change your `kernel.perf_event_paranoid` setting to 1". The check commands are `perf stat -e br_inst_retired.conditional true` (Intel; `br_inst_retired.cond` on newer perf) and `perf stat -e ex_ret_cond true` (AMD), and the count must be non-zero — [Will rr work on my system](https://github.com/rr-debugger/rr/wiki/Will-rr-work-on-my-system).

**Containers**
- Docker needs `--cap-add=SYS_PTRACE --security-opt seccomp=unconfined`. The default seccomp profile blocks `ptrace`, `perf_event_open` and `process_vm_writev`, and no custom profile is published.
- Use `--tmpfs /tmp:exec`, because a noexec tmpfs breaks some replays (liborc).

  Source: [rr wiki: Docker](https://github.com/rr-debugger/rr/wiki/Docker).

**AMD Zen**
- "On Zen CPUs, rr will not work reliably unless you disable the hardware SpecLockMap optimization": run `scripts/zen_workaround.py` as root, and again after each reboot or suspend; `--check` validates it.
- Alternatives:
  - a kernel module (needs `CONFIG_TRACEPOINTS`);
  - setting MSR `0xc0011020` bit 54 and clearing bit 10 at boot from GRUB;
  - fixing SSB mitigation (`nospec_store_bypass_disable` or `spec_store_bypass_disable=on`), because the kernel toggling it resets the workaround.
- Validated parts run from Zen (1700/1800X) to Zen 4 (7840U, 7950X3D).
- "The rdrand instruction does not work with rr on Zen" (#2862).

  Source: [rr wiki: Zen](https://github.com/rr-debugger/rr/wiki/Zen).

**Intel hybrid and AArch64**
- Intel hybrid: "rr prefers P-cores but can also use E-cores correctly" (5.10.0) — [releases](https://github.com/rr-debugger/rr/releases).
- AArch64 has been "production quality, with caveats" since 5.6.0. Supported: Cortex, Neoverse (including AWS Graviton) and Apple M1 running Linux on bare metal. `LDREX`/`STREX` are unsupported, so user code must use LSE atomics — [releases](https://github.com/rr-debugger/rr/releases).

**Cloud and hypervisors**
- GCE: the PMU is "disabled within VMs" by default. It is enabled with `--performance-monitoring-unit=PMU_TYPE` on A4X, C4A, C4 or M4. The enhanced type needs particular C4/M4 sizes (C4: 144 or 288 vCPUs). Changing it on an existing VM takes a restart — [GCE: Enable the PMU](https://docs.cloud.google.com/compute/docs/enable-pmu-in-vms), [PMU overview](https://docs.cloud.google.com/compute/docs/pmu-overview).
- AWS: a vPMU is available only on some (generally large or whole-socket) Intel instance sizes; one older answer lists i3.metal, c5.9xlarge, c5.18xlarge, m5.12xlarge and m5.24xlarge. AWS's Graviton runbook says not every size supports PMU events and that AMD instance families (5a/6a/7a) were "not supported currently" — [AWS Graviton perf runbook](https://aws.github.io/graviton/perfrunbook/debug_hw_perf.html), [SO answer (about 2021, may be stale)](https://kiwix.lema.org/stackoverflow.com_en_all_2023-11/questions/69395912/pmu-x86-64-performance-counters-not-showing-in-perf-under-aws).
- An rr user on i3.16xlarge hit "hardware performance counter not working" — [rr #1753](https://github.com/rr-debugger/rr/issues/1753).
- VirtualBox has no PMU virtualisation and cannot run rr. Xen does not virtualise the PMU for guests — [rr #1753](https://github.com/rr-debugger/rr/issues/1753), [Xen list](https://lists.xen.org/archives/html/xen-devel/2010-03/msg01734.html). The rr README says no known Xen configuration works (prior engines notes).
- CPUID faulting is recorded in the trace header (`hasCpuidFaulting`) and printed by `traceinfo` (`cpuidFaulting`) — [rr_trace.capnp#L92](https://github.com/rr-debugger/rr/blob/ca20dcb/src/rr_trace.capnp#L92), [TraceInfoCommand.cc#L64](https://github.com/rr-debugger/rr/blob/ca20dcb/src/TraceInfoCommand.cc#L64).

### Inferences
- Firecracker as configured here does not expose the PMU, so hopper's record and replay cannot run in these cloud lanes. They need bare metal, a GCE C4/C4A VM with the PMU enabled, or a PMU-capable AWS size. The container's `cpuid_fault` flag shows that faulting is present here; the counter is what is missing.
- gVisor intercepts syscalls in its own kernel (Sentry), so `perf_event_open` and ptrace of the real CPU are not expected to work. This was not verified (see Gaps).
- Replay-side analysis could in principle run without a PMU only with a remix-like engine that counts branches in software. Upstream rr replay still needs the counter.

### Gaps
- No Firecracker or gVisor documentation or rr issue about the PMU was found. The Firecracker result here comes from this container's own check, not from documentation.
- The current (2026) AWS list of PMU-capable instance sizes was not found in AWS documentation.
- Azure was not researched.

## 6. Recording Wine programs under rr

### Takeaway
It can be done but is not supported out of the box. In 2019 rr stopped at Wine's loader re-exec, and the reported fix was `WINELOADERNOEXEC=1`. A 2025 user report adds that Wine merge request 7669 is needed to avoid a "partial write" error, that the preloader must be removed so gdb can find `.so` debug info, and that a patched gdb is needed to debug the Windows-side code. Nobody official documents the setup. This is one user's experience.

### Cited Findings
- WineHQ bug 48074 (Nov 2019), "Record and Replay Framework doesn't work with wine": recording stopped at the loader. The reporter's workaround was `WINELOADERNOEXEC=1`, and the bug was closed invalid with nothing fixed in Wine — [wine-bugs 48074](https://www.winehq.org/pipermail/wine-bugs/2019-November/523204.html).
- 2025 wine-gitlab discussion (Bernhard Übelacker and others): using rr with Wine requires that rr can record at all and that the patch from Wine MR 7669 is applied, otherwise a "partial write" error appears. The preloader has to be removed for gdb to load debug info, and Windows-side debugging needs a patched gdb — [wine-gitlab 2025-04](https://list.winehq.org/hyperkitty/list/wine-gitlab@list.winehq.org/message/JM6VNYCPXAJXC2B4QYHMZMSH2CH4IXFN), [related message](https://list.winehq.org/hyperkitty/list/wine-gitlab@list.winehq.org/message/G7UARQUWG2XA3T52IJU62U6PWY6ZPPVT).
- Related rr limits:
  - io_uring and the other syscalls rr refuses (see §7) also apply to wineserver.
  - rr cannot record memory shared with unrecorded processes, so wineserver must be recorded in the same tree — the rr paper's shared-memory limit, cited in the prior engines notes ([arXiv 1705.05937](https://arxiv.org/pdf/1705.05937)).

### Inferences
- For the Parasolid-under-Wine oracle: start wineserver under rr, or let rr record the tree with `wine` starting it, with `WINELOADERNOEXEC=1`, and expect to carry Wine patches. Treat this as the "run one experiment first" item from the earlier notes. It cannot be tested in this container (no PMU).
- Windows PE code shows up in rr as anonymous executable mappings with no ELF debug info. Index symbolisation needs PE/PDB support in hopper, not gdb's.

### Gaps
- The text and merge status of Wine MR 7669 were not read.
- No rr GitHub issue about Wine was retrieved: the GitHub API was blocked here, and search returned only WineHQ lists.

## 7. Rust specifics: pretty-printers, RDRAND/RDSEED, CPUID faulting, getrandom, io_uring and tokio

### Takeaway
- Rust std's randomness uses the `getrandom` syscall, which rr records. rr hides RDRAND/RDSEED/RTM from CPUID when CPUID faulting is available.
- Without faulting (some VMs and AMD hosts), code that executes `rdrand` after its own CPUID check diverges.
- `io_uring_setup` (also `io_setup`, `clone3`, `close_range`) gets a faked `ENOSYS`, so a runtime must fall back.
- Plain tokio uses epoll and does not touch io_uring. Only the unstable `io-uring` fs feature does, and whether it falls back cleanly on ENOSYS is unverified.
- For pretty-printers, run `rr replay -d rust-gdb`.

### Cited Findings
- **Refused syscalls:** `close_range`, `clone3`, `io_uring_setup` and `io_setup` are rewritten to fail with `-ENOSYS` ("Prevent the various syscalls that we don't support from being used by applications and fake an ENOSYS return") — [record_syscall.cc#L4773-L4782](https://github.com/rr-debugger/rr/blob/ca20dcb/src/record_syscall.cc#L4773-L4782). There is also a note that "io_uring_enter (not yet supported)" — [RecordTask.cc#L1722](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RecordTask.cc#L1722).
- **getrandom and rseq:** `getrandom` has a recording case — [record_syscall.cc#L4477](https://github.com/rr-debugger/rr/blob/ca20dcb/src/record_syscall.cc#L4477). `rseq` is handled — [record_syscall.cc#L4786](https://github.com/rr-debugger/rr/blob/ca20dcb/src/record_syscall.cc#L4786), and has been emulated since 5.6.0 ([releases](https://github.com/rr-debugger/rr/releases)).
- **CPUID masking:** `DisableCPUIDFeatures::amend_cpuid_data` always clears RDRAND (leaf 1 ECX bit 30), RDSEED (leaf 7 EBX bit 18) and RTM (leaf 7 EBX bit 11) — [RecordSession.cc#L2976-L2995](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RecordSession.cc#L2976-L2995).
- **Without CPUID faulting** rr falls back to environment hacks: `OPENSSL_ia32cap=~4611686018427387904:0`, `QT_NO_CPU_FEATURE=rdrnd rdseed rtm` and `SYSTEMD_RDRAND=0` — [RecordSession.cc#L2612-L2624](https://github.com/rr-debugger/rr/blob/ca20dcb/src/RecordSession.cc#L2612-L2624).
- **On Zen**, "rdrand ... does not work with rr" and other libraries must avoid it themselves — [rr wiki: Zen](https://github.com/rr-debugger/rr/wiki/Zen).
- **io_uring in practice:** MariaDB's `mtr --rr` "will fail to start up the server if it has been configured with liburing" until rr#2613 is addressed, so MariaDB turns off native AIO under rr — [MariaDB commit e0c3b5f9a5](https://github.com/MariaDB/server/commit/e0c3b5f9a5).
- **tokio:** tokio's io_uring fs path is compiled only with `tokio_unstable` plus the `io-uring`, `rt` and `fs` features on Linux — [tokio 1.48 tests/fs_uring.rs](https://docs.rs/crate/tokio/latest/source/tests/fs_uring.rs). A tokio 1.52.2 test mentions "falling back to the thread pool" — [tokio 1.52.2 tests/fs_uring_read.rs](https://docs.rs/crate/tokio/1.52.2/source/tests/fs_uring_read.rs).
- **rust-gdb:** `rr replay -d <FILE>` selects the debugger binary (for example `rust-gdb`) — [ReplayCommand.cc#L56](https://github.com/rr-debugger/rr/blob/ca20dcb/src/ReplayCommand.cc#L56). A community gist recommends `rr record` on a built test binary rather than `cargo test`, and setting rr up to pretty-print Rust — [gist spacejam](https://gist.github.com/spacejam/15f27007c0b1bcc1d6b4c9169b18868c).
- Pernosco lists Rust as a supported language — [pernos.co overview](https://pernos.co/about/overview/).

### Inferences
- What tokio hits:
  - Default tokio (mio/epoll, `getrandom`, futex, `rseq` through glibc) needs only syscalls rr records.
  - `clone3` is refused, so glibc's `pthread_create` falls back to `clone`; glibc does fall back on ENOSYS (general knowledge, not verified here).
  - `tokio_unstable` with `io-uring` will see ENOSYS from `io_uring_setup`; whether tokio then uses the blocking pool is unverified.
- hopper's own Rust binaries should avoid `rdrand`-based crates (for example the `rdrand` crate, or `getrandom`'s opt-in RDRAND backend) unless CPUID faulting is guaranteed.

### Gaps
- The body and status of rr issue #2613 (io_uring) were not read: GitHub was not reachable through the API here.
- How tokio behaves on io_uring ENOSYS was not confirmed from tokio source.
- No rr-specific problem report was found for rust-gdb pretty-printers.
