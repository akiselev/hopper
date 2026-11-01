//! The scripted recording of `fixtures/worked-example`: every instruction boundary the fake can
//! stop at, with its frames, locals, memory writes, calls, allocations and timeline events.
//!
//! The script is built by [`Builder`], which works like a tiny interpreter log: `step` records
//! the instruction about to run, and writes made between two `step`s become visible at the
//! second one (a `WriteEvent`'s `at` is the first moment showing the new bytes; its `pc` is the
//! instruction that wrote them).

use hopper_core::api::{
    AllocationId, AllocationInfo, CallId, CallNode, Confidence, Instruction, MachineKind,
    OriginHop, OwnershipKind, OwnershipLabel, OwnershipState, TimelineEvent, TimelineKind,
    WriteEvent,
};
use hopper_core::{Moment, Place, ProcessId, RecordingId, SourceLoc, Tid};

use crate::values::{CONFIG_TY, FakeVal, RefKind, STRING_TY};

/// The fake recording's id.
pub const RECORDING: RecordingId = RecordingId([
    0x5e, 0xed, 0x0f, 0xa4, 0x2c, 0x11, 0x4b, 0x7e, 0x9a, 0x01, 0x3d, 0x5c, 0x6b, 0x77, 0x80, 0x01,
]);
/// The debuggee's pid (and its only thread's tid).
pub const PID: i32 = 31337;
pub const TID: Tid = Tid(PID);
/// The fixture's source path as the debug information names it.
pub const MAIN_RS: &str = "/hopper/fixtures/worked-example/src/main.rs";
/// The executable.
pub const EXECUTABLE: &str = "/hopper/target/fixtures/worked-example/debug/worked-example";

const STRING_RS: &str =
    "/rustc/0a1b2c3d4e5f60718293a4b5c6d7e8f901234567/library/alloc/src/string.rs";
const VEC_RS: &str = "/rustc/0a1b2c3d4e5f60718293a4b5c6d7e8f901234567/library/alloc/src/vec/mod.rs";
const STDIO_RS: &str =
    "/rustc/0a1b2c3d4e5f60718293a4b5c6d7e8f901234567/library/std/src/io/stdio.rs";

pub(crate) const MAIN: &str = "worked_example::main";
pub(crate) const LOAD: &str = "worked_example::load";
pub(crate) const STRING_FROM: &str = "<alloc::string::String as core::convert::From<&str>>::from";
pub(crate) const CONFIG_CLONE: &str = "<worked_example::Config as core::clone::Clone>::clone";
pub(crate) const VEC_PUSH: &str = "alloc::vec::Vec<T,A>::push";
pub(crate) const PRINT: &str = "std::io::stdio::_print";

// Address-space layout.
pub(crate) const TEXT: (u64, u64) = (0x5555_5555_8000, 0x5555_5557_2000);
pub(crate) const RODATA: (u64, u64) = (0x5555_5555_6000, 0x5555_5555_7000);
pub(crate) const HEAP: (u64, u64) = (0x5555_5555_9000, 0x5555_5557_a000);
pub(crate) const STACK: (u64, u64) = (0x7ffc_8a3e_0000, 0x7ffc_8a3e_2000);
/// Where `"alpha"` sits in `.rodata`.
pub(crate) const ALPHA_LITERAL: u64 = 0x5555_5555_6004;

// Stack slots.
const A: u64 = 0x7ffc_8a3e_1e00;
const B: u64 = 0x7ffc_8a3e_1e20;
const C: u64 = 0x7ffc_8a3e_1e40;
const V: u64 = 0x7ffc_8a3e_1e60;
const N: u64 = 0x7ffc_8a3e_1e78;
const LOAD_NAME: u64 = 0x7ffc_8a3e_1d40;
const LOAD_TEMP: u64 = 0x7ffc_8a3e_1d60;
const CLONE_SELF: u64 = 0x7ffc_8a3e_1cf8;
const PUSH_SELF: u64 = 0x7ffc_8a3e_1cc0;
const PUSH_VALUE: u64 = 0x7ffc_8a3e_1cc8;

// Heap allocations.
const ALLOC_ALPHA: (AllocationId, u64, u64) = (AllocationId(1), 0x5555_5555_99d0, 5);
const ALLOC_CLONE: (AllocationId, u64, u64) = (AllocationId(2), 0x5555_5555_9a00, 5);
const ALLOC_VEC: (AllocationId, u64, u64) = (AllocationId(3), 0x5555_5555_9a20, 128);

/// A local variable in one frame at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    pub name: String,
    pub ty: String,
    pub addr: u64,
    pub size: u64,
    pub value: FakeVal,
    pub ownership: OwnershipState,
}

/// One frame at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameSnap {
    pub call: CallId,
    pub function: String,
    pub file: String,
    pub line: u32,
    /// The current pc (innermost frame) or the return address (outer frames).
    pub pc: u64,
    pub cfa: u64,
    pub locals: Vec<Local>,
}

impl FrameSnap {
    pub(crate) fn location(&self) -> SourceLoc {
        SourceLoc::new(self.file.clone(), self.line)
    }
}

/// One instruction boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StepRec {
    pub moment: Moment,
    pub pc: u64,
    /// Innermost first.
    pub frames: Vec<FrameSnap>,
    /// Writes that became visible at this moment.
    pub writes: Vec<WriteEvent>,
}

impl StepRec {
    pub(crate) fn top(&self) -> &FrameSnap {
        &self.frames[0]
    }
    pub(crate) fn depth(&self) -> usize {
        self.frames.len()
    }
    /// The (call, line) pair: consecutive steps sharing it are one source line's execution.
    pub(crate) fn line_key(&self) -> (CallId, u32) {
        (self.top().call, self.top().line)
    }
}

/// The ownership-labelled origin hops of the worked example, by what they wrote.
#[derive(Clone, Debug)]
pub(crate) struct Hops {
    /// `n = &v[0].name` (line 17).
    pub borrow_n: OriginHop,
    /// `v.push(c)`: `c` moved into the Vec's buffer (line 16).
    pub move_into_vec: OriginHop,
    /// `b.clone()`: the name buffer copied (line 14).
    pub clone_name: OriginHop,
    /// `b.clone()`: `retries` copied (line 14).
    pub clone_retries: OriginHop,
    /// `let b = a` (line 13).
    pub move_a_to_b: OriginHop,
    /// `let a = load()` (line 12).
    pub load_return: OriginHop,
    /// `String::from("alpha")` (line 7).
    pub alpha_literal: OriginHop,
    /// `retries: 3` (line 8).
    pub retries_literal: OriginHop,
}

#[derive(Debug)]
pub(crate) struct Script {
    pub steps: Vec<StepRec>,
    pub calls: Vec<CallNode>,
    pub events: Vec<TimelineEvent>,
    pub allocations: Vec<AllocationInfo>,
    /// Every instruction the recording executed, sorted by address.
    pub instructions: Vec<Instruction>,
    pub hops: Hops,
}

impl Script {
    pub(crate) fn process(&self) -> ProcessId {
        ProcessId::new(PID)
    }
}

// ---------------------------------------------------------------- the builder

struct BFrame {
    snap: FrameSnap,
    next_pc: u64,
}

struct BCall {
    id: CallId,
    parent: Option<CallId>,
    function: String,
    entry: Option<Moment>,
    exit: Option<Moment>,
    depth: u32,
    location: SourceLoc,
}

struct PendingAlloc {
    id: AllocationId,
    addr: u64,
    size: u64,
    requested_by: String,
}

#[derive(Default)]
struct Builder {
    steps: Vec<StepRec>,
    frames: Vec<BFrame>,
    calls: Vec<BCall>,
    pending_writes: Vec<(u64, Vec<u8>)>,
    pending_entry: Option<CallId>,
    pending_exits: Vec<CallId>,
    pending_events: Vec<TimelineKind>,
    pending_allocs: Vec<PendingAlloc>,
    pending_frees: Vec<AllocationId>,
    events: Vec<TimelineEvent>,
    allocations: Vec<AllocationInfo>,
    instructions: Vec<Instruction>,
    frame_time: u64,
    ticks: u64,
    sub: u32,
}

/// Instruction length by mnemonic, roughly as x86-64 encodes them.
fn insn_len(text: &str) -> u64 {
    let mnemonic = text.split_whitespace().next().unwrap_or("");
    match mnemonic {
        "call" => 5,
        "ret" | "push" | "pop" | "nop" => 1,
        "lea" | "mov" => {
            if text.contains("rip") {
                7
            } else {
                4
            }
        }
        "sub" | "add" | "cmp" => 4,
        _ => 3,
    }
}

/// Deterministic instruction bytes. The fake's code bytes are not a real encoding; they only
/// make `memory` and `disassemble` agree.
fn insn_bytes(addr: u64, text: &str) -> Vec<u8> {
    let len = insn_len(text) as usize;
    let opcode = match text.split_whitespace().next().unwrap_or("") {
        "call" => 0xe8,
        "ret" => 0xc3,
        "push" => 0x55,
        "pop" => 0x5d,
        "nop" => 0x90,
        "lea" => 0x48,
        "mov" => 0x48,
        _ => 0x48,
    };
    let mut out = vec![opcode];
    let mut seed = addr ^ 0x9e37_79b9_7f4a_7c15;
    while out.len() < len {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((seed >> 56) as u8);
    }
    out
}

impl Builder {
    fn new() -> Self {
        Builder {
            frame_time: 1,
            ticks: 120_000,
            ..Default::default()
        }
    }

    fn top(&mut self) -> &mut BFrame {
        self.frames.last_mut().expect("a frame")
    }

    fn enter(&mut self, function: &str, file: &str, line: u32, base: u64, cfa: u64) {
        let id = CallId(self.calls.len() as u64);
        let parent = self.frames.last().map(|f| f.snap.call);
        self.calls.push(BCall {
            id,
            parent,
            function: function.to_string(),
            entry: None,
            exit: None,
            depth: self.frames.len() as u32,
            location: SourceLoc::new(file, line),
        });
        self.pending_entry = Some(id);
        self.frames.push(BFrame {
            snap: FrameSnap {
                call: id,
                function: function.to_string(),
                file: file.to_string(),
                line,
                pc: base,
                cfa,
                locals: Vec::new(),
            },
            next_pc: base,
        });
    }

    fn exit(&mut self) {
        let f = self.frames.pop().expect("a frame to exit");
        self.pending_exits.push(f.snap.call);
    }

    fn frame_mut(&mut self, function: &str) -> &mut BFrame {
        self.frames
            .iter_mut()
            .rev()
            .find(|f| f.snap.function == function)
            .unwrap_or_else(|| panic!("no frame of {function}"))
    }

    fn declare(&mut self, name: &str, ty: &str, addr: u64, size: u64) {
        self.top().snap.locals.push(Local {
            name: name.into(),
            ty: ty.into(),
            addr,
            size,
            value: FakeVal::Uninit,
            ownership: OwnershipState::Live,
        });
    }

    fn local_mut(&mut self, function: &str, name: &str) -> &mut Local {
        self.frame_mut(function)
            .snap
            .locals
            .iter_mut()
            .find(|l| l.name == name)
            .unwrap_or_else(|| panic!("no local {name} in {function}"))
    }

    /// Store a value into a local, writing its bytes.
    fn set(&mut self, function: &str, name: &str, value: FakeVal) {
        let local = self.local_mut(function, name);
        let addr = local.addr;
        let bytes = value.encode();
        local.value = value;
        if let Some(bytes) = bytes {
            self.pending_writes.push((addr, bytes));
        }
    }

    /// Update what a local shows without writing it (a reference whose target changed).
    fn refresh(&mut self, function: &str, name: &str, value: FakeVal) {
        self.local_mut(function, name).value = value;
    }

    fn own(&mut self, function: &str, name: &str, ownership: OwnershipState) {
        self.local_mut(function, name).ownership = ownership;
    }

    fn write(&mut self, addr: u64, bytes: Vec<u8>) {
        self.pending_writes.push((addr, bytes));
    }

    fn alloc(&mut self, (id, addr, size): (AllocationId, u64, u64), requested_by: &str) {
        self.pending_allocs.push(PendingAlloc {
            id,
            addr,
            size,
            requested_by: requested_by.into(),
        });
    }

    fn free(&mut self, (id, _, _): (AllocationId, u64, u64)) {
        self.pending_frees.push(id);
    }

    fn event(&mut self, kind: TimelineKind) {
        self.frame_time += 1;
        self.pending_events.push(kind);
    }

    /// The index the next `step` gets.
    fn next(&self) -> usize {
        self.steps.len()
    }

    /// Record the instruction boundary before `text` runs, at `line` of the innermost frame.
    fn step(&mut self, line: u32, text: &str) {
        let index = self.steps.len();
        if index > 0 {
            if index % 3 == 2 {
                self.sub += 1;
            } else {
                self.ticks += 2 + (index as u64 * 7) % 5;
                self.sub = 0;
            }
        }
        let moment = Moment {
            recording: RECORDING,
            frame_time: self.frame_time,
            tid: TID,
            ticks: self.ticks,
            sub: self.sub,
        };
        let len = insn_len(text);
        let top = self.top();
        let pc = top.next_pc;
        top.next_pc += len;
        top.snap.line = line;
        top.snap.pc = pc;

        let file = self.top().snap.file.clone();
        self.instructions.push(Instruction {
            addr: pc,
            bytes: insn_bytes(pc, text),
            text: text.to_string(),
            location: Some(SourceLoc::new(file, line)),
        });

        let (write_pc, write_fn, write_loc) = match self.steps.last() {
            Some(prev) => (
                prev.pc,
                Some(prev.top().function.clone()),
                Some(prev.top().location()),
            ),
            None => (pc, None, None),
        };
        let writes = std::mem::take(&mut self.pending_writes)
            .into_iter()
            .map(|(addr, value)| WriteEvent {
                at: moment,
                pc: write_pc,
                place: Place::memory(ProcessId::new(PID), addr, value.len() as u64),
                value,
                function: write_fn.clone(),
                location: write_loc.clone(),
            })
            .collect();

        let frames: Vec<FrameSnap> = self
            .frames
            .iter()
            .rev()
            .map(|f| {
                let mut s = f.snap.clone();
                if s.call != self.frames.last().expect("a frame").snap.call {
                    s.pc = f.next_pc;
                }
                s
            })
            .collect();

        if let Some(id) = self.pending_entry.take() {
            self.calls[id.0 as usize].entry = Some(moment);
        }
        for id in std::mem::take(&mut self.pending_exits) {
            self.calls[id.0 as usize].exit = Some(moment);
        }
        for kind in std::mem::take(&mut self.pending_events) {
            self.events.push(TimelineEvent { at: moment, kind });
        }
        for a in std::mem::take(&mut self.pending_allocs) {
            self.allocations.push(AllocationInfo {
                id: a.id,
                process: ProcessId::new(PID),
                addr: a.addr,
                size: a.size,
                allocated: moment,
                freed: None,
                predecessor: None,
                requested_by: Some(a.requested_by),
            });
        }
        for id in std::mem::take(&mut self.pending_frees) {
            if let Some(a) = self.allocations.iter_mut().find(|a| a.id == id) {
                a.freed = Some(moment);
            }
        }
        self.steps.push(StepRec {
            moment,
            pc,
            frames,
            writes,
        });
    }

    fn finish(mut self, hops: Hops) -> Script {
        let calls = self
            .calls
            .iter()
            .map(|c| CallNode {
                id: c.id,
                parent: c.parent,
                tid: TID,
                function: c.function.clone(),
                entry: c.entry.expect("every call has a step"),
                exit: c.exit,
                depth: c.depth,
                child_count: self.calls.iter().filter(|k| k.parent == Some(c.id)).count() as u64,
                location: Some(c.location.clone()),
            })
            .collect();
        self.instructions.sort_by_key(|i| i.addr);
        self.instructions.dedup_by_key(|i| i.addr);
        Script {
            steps: self.steps,
            calls,
            events: self.events,
            allocations: self.allocations,
            instructions: self.instructions,
            hops,
        }
    }
}

fn moved(to: &str, line: u32, at: Moment) -> OwnershipState {
    OwnershipState::MovedOut {
        to: Some(to.into()),
        at: Some(SourceLoc::new(MAIN_RS, line)),
        moment: Some(at),
    }
}

fn dropped(line: u32, at: Moment) -> OwnershipState {
    OwnershipState::Dropped {
        at: Some(SourceLoc::new(MAIN_RS, line)),
        moment: Some(at),
    }
}

// ---------------------------------------------------------------- the worked example

/// The recording of `fixtures/worked-example`.
pub(crate) fn worked_example() -> Script {
    let name_a = FakeVal::string(5, ALLOC_ALPHA.1, "alpha");
    let name_c = FakeVal::string(5, ALLOC_CLONE.1, "alpha");
    let config_a = FakeVal::config(name_a.clone(), 3);
    let config_c = FakeVal::config(name_c.clone(), 3);
    let vec_ty = format!("alloc::vec::Vec<{CONFIG_TY}>");
    let empty_vec = FakeVal::Vec {
        elem_ty: CONFIG_TY.into(),
        cap: 0,
        ptr: 8,
        elems: Vec::new(),
    };
    let grown_vec = FakeVal::Vec {
        elem_ty: CONFIG_TY.into(),
        cap: 4,
        ptr: ALLOC_VEC.1,
        elems: Vec::new(),
    };
    let full_vec = FakeVal::Vec {
        elem_ty: CONFIG_TY.into(),
        cap: 4,
        ptr: ALLOC_VEC.1,
        elems: vec![config_c.clone()],
    };
    let push_self = |v: &FakeVal| FakeVal::Ref {
        kind: RefKind::Mut,
        addr: V,
        target_ty: vec_ty.clone(),
        target: Box::new(v.clone()),
    };

    let mut b = Builder::new();
    b.event(TimelineKind::Exec {
        executable: EXECUTABLE.into(),
    });

    // fn main
    b.enter(MAIN, MAIN_RS, 11, 0x5555_5555_8a40, 0x7ffc_8a3e_1f60);
    b.declare("a", CONFIG_TY, A, 32);
    b.step(12, "lea rdi, [rbp-0x160]"); // 0
    b.step(12, "call worked_example::load"); // 1

    // fn load
    b.enter(LOAD, MAIN_RS, 6, 0x5555_5555_89a0, 0x7ffc_8a3e_1d90);
    b.declare("name", STRING_TY, LOAD_NAME, 24);
    b.step(7, "sub rsp, 0x58"); // 2
    b.step(7, "lea rsi, [rip+0xd65a]"); // 3
    b.step(
        7,
        "call <alloc::string::String as core::convert::From<&str>>::from",
    ); // 4

    // String::from
    b.enter(
        STRING_FROM,
        STRING_RS,
        2799,
        0x5555_5555_c120,
        0x7ffc_8a3e_1d00,
    );
    b.step(2800, "push rbp"); // 5
    b.step(2801, "call alloc::alloc::alloc"); // 6
    b.alloc(ALLOC_ALPHA, STRING_FROM);
    b.event(TimelineKind::Syscall { name: "brk".into() });
    b.step(2801, "call memcpy"); // 7
    b.write(ALLOC_ALPHA.1, b"alpha".to_vec());
    let alpha_step = b.next();
    b.step(2801, "mov rax, qword ptr [rbp-0x18]"); // 8
    b.step(2802, "ret"); // 9
    b.exit();
    b.set(LOAD, "name", name_a.clone());
    b.step(7, "mov rax, qword ptr [rbp-0x48]"); // 10

    // Config { name, retries: 3 }
    b.step(8, "mov rcx, qword ptr [rbp-0x40]"); // 11
    b.write(LOAD_TEMP, name_a.encode().expect("bytes"));
    b.own(
        LOAD,
        "name",
        OwnershipState::MovedOut {
            to: Some("Config.name".into()),
            at: Some(SourceLoc::new(MAIN_RS, 8)),
            moment: None,
        },
    );
    b.step(8, "mov dword ptr [rbp-0x20], 3"); // 12
    b.write(LOAD_TEMP + 24, 3u32.to_le_bytes().to_vec());
    let retries_step = b.next();
    b.step(8, "mov rax, qword ptr [rbp-0x50]"); // 13
    b.step(9, "movups xmm0, xmmword ptr [rbp-0x40]"); // 14
    b.set(MAIN, "a", config_a.clone());
    let return_step = b.next();
    b.step(9, "ret"); // 15
    b.exit();

    b.step(12, "nop"); // 16
    // let b = a;
    b.declare("b", CONFIG_TY, B, 32);
    b.step(13, "mov rax, qword ptr [rbp-0x160]"); // 17
    b.step(13, "movups xmm0, xmmword ptr [rbp-0x158]"); // 18
    b.set(MAIN, "b", config_a.clone());
    let move_ab_step = b.next();
    let after_move = b.peek_moment();
    b.own(MAIN, "a", moved("b", 13, after_move));
    b.step(13, "movups xmmword ptr [rbp-0x138], xmm0"); // 19

    // let c = b.clone();
    b.declare("c", CONFIG_TY, C, 32);
    b.step(14, "lea rdi, [rbp-0x140]"); // 20
    b.step(
        14,
        "call <worked_example::Config as core::clone::Clone>::clone",
    ); // 21
    b.enter(CONFIG_CLONE, MAIN_RS, 3, 0x5555_5555_8900, 0x7ffc_8a3e_1d10);
    b.declare("self", &format!("&{CONFIG_TY}"), CLONE_SELF, 8);
    b.set(
        CONFIG_CLONE,
        "self",
        FakeVal::Ref {
            kind: RefKind::Shared,
            addr: B,
            target_ty: CONFIG_TY.into(),
            target: Box::new(config_a.clone()),
        },
    );
    b.step(3, "push rbp"); // 22
    b.step(
        3,
        "call <alloc::string::String as core::clone::Clone>::clone",
    ); // 23
    b.alloc(ALLOC_CLONE, CONFIG_CLONE);
    b.write(ALLOC_CLONE.1, b"alpha".to_vec());
    let clone_name_step = b.next();
    b.step(3, "mov ecx, dword ptr [rax+0x18]"); // 24
    b.step(3, "ret"); // 25
    b.exit();
    b.set(MAIN, "c", config_c.clone());
    let clone_return_step = b.next();
    b.step(14, "nop"); // 26

    // let mut v = Vec::new();
    b.declare("v", &vec_ty, V, 24);
    b.step(15, "call alloc::vec::Vec<T>::new"); // 27
    b.step(15, "mov qword ptr [rbp-0x100], rax"); // 28
    b.set(MAIN, "v", empty_vec.clone());
    b.step(16, "lea rdi, [rbp-0x100]"); // 29

    // v.push(c);
    b.step(16, "call alloc::vec::Vec<T,A>::push"); // 30
    b.enter(VEC_PUSH, VEC_RS, 2398, 0x5555_5555_d300, 0x7ffc_8a3e_1d10);
    b.declare("self", &format!("&mut {vec_ty}"), PUSH_SELF, 8);
    b.declare("value", CONFIG_TY, PUSH_VALUE, 32);
    b.set(VEC_PUSH, "self", push_self(&empty_vec));
    b.set(VEC_PUSH, "value", config_c.clone());
    b.step(2401, "cmp rax, qword ptr [rdi]"); // 31
    b.step(2402, "call alloc::raw_vec::RawVec<T,A>::grow_one"); // 32
    b.alloc(ALLOC_VEC, VEC_PUSH);
    b.set(MAIN, "v", grown_vec.clone());
    b.refresh(VEC_PUSH, "self", push_self(&grown_vec));
    b.step(2405, "call core::ptr::write"); // 33
    b.write(ALLOC_VEC.1, config_c.encode().expect("bytes"));
    let into_vec_step = b.next();
    let at_move_into_vec = b.peek_moment();
    b.own(MAIN, "c", moved("v[0]", 16, at_move_into_vec));
    b.own(
        VEC_PUSH,
        "value",
        OwnershipState::MovedOut {
            to: Some("*self.ptr.add(len)".into()),
            at: Some(SourceLoc::new(VEC_RS, 2405)),
            moment: Some(at_move_into_vec),
        },
    );
    b.step(2406, "add qword ptr [rdi+0x10], 1"); // 34
    b.set(MAIN, "v", full_vec.clone());
    b.refresh(VEC_PUSH, "self", push_self(&full_vec));
    b.step(2407, "ret"); // 35
    b.exit();
    b.step(16, "nop"); // 36

    // let n = &v[0].name;
    b.declare("n", &format!("&{STRING_TY}"), N, 8);
    b.step(17, "mov rax, qword ptr [rbp-0xf8]"); // 37
    b.step(17, "lea rax, [rax]"); // 38
    b.set(
        MAIN,
        "n",
        FakeVal::Ref {
            kind: RefKind::Shared,
            addr: ALLOC_VEC.1,
            target_ty: STRING_TY.into(),
            target: Box::new(name_c.clone()),
        },
    );
    let borrow_step = b.next();
    b.step(18, "lea rdi, [rbp-0x90]"); // 39

    // println!("{n} {}", b.retries);
    b.step(18, "call std::io::stdio::_print"); // 40
    b.enter(PRINT, STDIO_RS, 1239, 0x5555_5557_0000, 0x7ffc_8a3e_1c80);
    b.step(1240, "push rbp"); // 41
    b.event(TimelineKind::Syscall {
        name: "write".into(),
    });
    b.step(1241, "ret"); // 42
    b.exit();
    b.step(18, "nop"); // 43

    // end of main: drop v, then b (a and c were moved out).
    b.step(
        19,
        "call core::ptr::drop_in_place<alloc::vec::Vec<worked_example::Config>>",
    ); // 44
    b.free(ALLOC_CLONE);
    b.free(ALLOC_VEC);
    let at_drop_v = b.peek_moment();
    b.own(MAIN, "v", dropped(19, at_drop_v));
    b.step(19, "call core::ptr::drop_in_place<worked_example::Config>"); // 45
    b.free(ALLOC_ALPHA);
    let at_drop_b = b.peek_moment();
    b.own(MAIN, "b", dropped(19, at_drop_b));
    b.event(TimelineKind::ProcessExit { code: Some(0) });
    b.step(19, "ret"); // 46

    let pid = ProcessId::new(PID);
    let mem = |addr: u64, len: u64| Place::memory(pid, addr, len);
    let exact = |kind, src: &str, dst: &str, line: u32| OwnershipLabel {
        kind,
        src: Some(src.into()),
        dst: Some(dst.into()),
        span: Some(SourceLoc::new(MAIN_RS, line)),
        confidence: Confidence::Exact,
    };
    let hop = |b: &Builder,
               step: usize,
               read: Option<Place>,
               written: Place,
               machine: MachineKind,
               ownership: OwnershipLabel,
               allocation: Option<AllocationId>| {
        let s = &b.steps[step];
        let prev = &b.steps[step - 1];
        OriginHop {
            at: s.moment,
            pc: prev.pc,
            function: Some(prev.top().function.clone()),
            location: Some(prev.top().location()),
            read,
            written,
            machine,
            ownership: Some(ownership),
            allocation,
        }
    };
    let hops = Hops {
        borrow_n: hop(
            &b,
            borrow_step,
            Some(mem(V + 8, 8)),
            mem(N, 8),
            MachineKind::Arithmetic,
            exact(OwnershipKind::SharedBorrow, "v[0].name", "n", 17),
            None,
        ),
        move_into_vec: hop(
            &b,
            into_vec_step,
            Some(mem(PUSH_VALUE, 32)),
            mem(ALLOC_VEC.1, 32),
            MachineKind::Copy,
            exact(OwnershipKind::Move, "c", "v[0]", 16),
            Some(ALLOC_VEC.0),
        ),
        clone_name: hop(
            &b,
            clone_name_step,
            Some(mem(ALLOC_ALPHA.1, 5)),
            mem(ALLOC_CLONE.1, 5),
            MachineKind::Copy,
            exact(OwnershipKind::Clone, "b", "c", 14),
            Some(ALLOC_CLONE.0),
        ),
        clone_retries: hop(
            &b,
            clone_return_step,
            Some(mem(B + 24, 4)),
            mem(C + 24, 4),
            MachineKind::Copy,
            exact(OwnershipKind::Clone, "b.retries", "c.retries", 14),
            None,
        ),
        move_a_to_b: hop(
            &b,
            move_ab_step,
            Some(mem(A, 32)),
            mem(B, 32),
            MachineKind::Copy,
            exact(OwnershipKind::Move, "a", "b", 13),
            None,
        ),
        load_return: hop(
            &b,
            return_step,
            Some(mem(LOAD_TEMP, 32)),
            mem(A, 32),
            MachineKind::Copy,
            exact(OwnershipKind::CallReturn, "load()", "a", 12),
            None,
        ),
        alpha_literal: hop(
            &b,
            alpha_step,
            Some(mem(ALPHA_LITERAL, 5)),
            mem(ALLOC_ALPHA.1, 5),
            MachineKind::Copy,
            exact(OwnershipKind::Literal, "\"alpha\"", "name", 7),
            Some(ALLOC_ALPHA.0),
        ),
        retries_literal: hop(
            &b,
            retries_step,
            None,
            mem(LOAD_TEMP + 24, 4),
            MachineKind::Immediate,
            exact(OwnershipKind::Literal, "3", "Config.retries", 8),
            None,
        ),
    };
    b.finish(hops)
}

impl Builder {
    /// The moment the next `step` will get (for ownership states that name it).
    fn peek_moment(&self) -> Moment {
        let index = self.steps.len();
        let (ticks, sub) = if index % 3 == 2 {
            (self.ticks, self.sub + 1)
        } else {
            (self.ticks + 2 + (index as u64 * 7) % 5, 0)
        };
        Moment {
            recording: RECORDING,
            frame_time: self.frame_time,
            tid: TID,
            ticks,
            sub,
        }
    }
}

/// The initial bytes of `.rodata`: some other constants, then `"alpha"` at [`ALPHA_LITERAL`].
pub(crate) fn rodata_image() -> Vec<u8> {
    let mut v = vec![0u8; (RODATA.1 - RODATA.0) as usize];
    let off = (ALPHA_LITERAL - RODATA.0) as usize;
    v[..4].copy_from_slice(&[0x01, 0x00, 0x02, 0x00]);
    v[off..off + 5].copy_from_slice(b"alpha");
    let fmt = b" \n";
    v[off + 8..off + 8 + fmt.len()].copy_from_slice(fmt);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_ordered_and_sized() {
        let s = worked_example();
        assert!((40..=50).contains(&s.steps.len()), "{}", s.steps.len());
        for w in s.steps.windows(2) {
            assert!(w[0].moment < w[1].moment);
        }
        let lines: std::collections::BTreeSet<u32> = s
            .steps
            .iter()
            .filter(|st| st.top().file == MAIN_RS && st.top().function == MAIN)
            .map(|st| st.top().line)
            .collect();
        for l in 12..=18 {
            assert!(lines.contains(&l), "main never at line {l}");
        }
        let fns: std::collections::BTreeSet<&str> =
            s.calls.iter().map(|c| c.function.as_str()).collect();
        for f in [MAIN, LOAD, STRING_FROM, CONFIG_CLONE, VEC_PUSH] {
            assert!(fns.contains(f), "{f}");
        }
    }

    #[test]
    fn peek_matches_step() {
        let s = worked_example();
        let a_moved = s
            .steps
            .iter()
            .find_map(|st| {
                st.frames.iter().find_map(|f| {
                    f.locals.iter().find_map(|l| match &l.ownership {
                        OwnershipState::MovedOut {
                            moment: Some(m), ..
                        } if l.name == "a" => Some(*m),
                        _ => None,
                    })
                })
            })
            .unwrap();
        let first = s
            .steps
            .iter()
            .find(|st| {
                st.frames[0].locals.iter().any(|l| {
                    l.name == "a" && matches!(l.ownership, OwnershipState::MovedOut { .. })
                })
            })
            .unwrap();
        assert_eq!(first.moment, a_moved);
    }
}
