//! An in-memory stand-in for `rr replay -s`, for tests on machines that cannot run rr.
//!
//! It replays a fixed timeline of instruction boundaries and answers the packets hopper sends
//! the way rr's server does (GdbServerConnection.cc / GdbServer.cc at rr ca20dcb): the same
//! qSupported reply, hex-encoded `qRRCmd` replies with a trailing newline, `vRun;;<hex>` seeks
//! that drop every breakpoint and make the next resume stop in place with signal 0, `watch:`
//! for every data watchpoint, `replaylog:begin` at the start, a synthetic SIGKILL at the end and
//! `W00` after it.
//!
//! The program: one thread (pid = tid = 0x10). At position `p`, `rip = 0x1000 + 4 * (p % 10)`
//! (a loop, so a breakpoint hits repeatedly), `rax = p`, `rsp = 0x7ff0`. The tick count rises
//! after every third instruction; an event completes every 13. The 8 bytes at `WATCHED` count
//! the writes so far; a write happens on the step into each of `WRITES`.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use hopper_rsp::Connection;
use hopper_rsp::packet::{Deframer, Incoming, frame, from_hex, to_hex};

pub const LEN: usize = 60;
pub const WATCHED: u64 = 0x8000;
pub const WRITES: [usize; 3] = [5, 17, 29];
pub const TID: i32 = 0x10;

/// The shape of the fake program's timeline. [`Layout::DEFAULT`] is the one described above;
/// others make a PC come round again inside one tick region.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `rip = 0x1000 + 4 * (p % pc_period)`.
    pub pc_period: usize,
    /// The tick count rises after every `tick_every` instructions.
    pub tick_every: usize,
    /// An event completes every `event_every` instructions.
    pub event_every: usize,
}

impl Layout {
    pub const DEFAULT: Layout = Layout {
        pc_period: 10,
        tick_every: 3,
        event_every: 13,
    };

    pub fn rip(&self, p: usize) -> u64 {
        0x1000 + 4 * (p % self.pc_period) as u64
    }
    pub fn ticks(&self, p: usize) -> u64 {
        (p / self.tick_every) as u64
    }
    pub fn frame_time(&self, p: usize) -> u64 {
        1 + (p / self.event_every) as u64
    }
    /// The first position with the same (event, ticks): where `sub` is 0.
    pub fn region_start(&self, p: usize) -> usize {
        (0..=p)
            .find(|q| self.frame_time(*q) == self.frame_time(p) && self.ticks(*q) == self.ticks(p))
            .unwrap()
    }
    pub fn sub(&self, p: usize) -> u32 {
        (p - self.region_start(p)) as u32
    }
}

pub fn rip(p: usize) -> u64 {
    Layout::DEFAULT.rip(p)
}
pub fn ticks(p: usize) -> u64 {
    Layout::DEFAULT.ticks(p)
}
pub fn frame_time(p: usize) -> u64 {
    Layout::DEFAULT.frame_time(p)
}
fn writes_before(p: usize) -> u64 {
    WRITES.iter().filter(|w| **w <= p).count() as u64
}
/// The first position with the same (event, ticks): where `sub` is 0.
pub fn region_start(p: usize) -> usize {
    Layout::DEFAULT.region_start(p)
}
pub fn sub(p: usize) -> u32 {
    Layout::DEFAULT.sub(p)
}

const SUPPORTED: &str = "PacketSize=100000;QStartNoAckMode+;qXfer:features:read+;\
qXfer:auxv:read+;qXfer:exec-file:read+;qXfer:siginfo:read+;qXfer:siginfo:write+;multiprocess+;\
hwbreak+;swbreak+;ConditionalBreakpoints+;vContSupported+;QPassSignals+;ReverseContinue+;\
ReverseStep+";

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<target><architecture>i386:x86-64</architecture>
<xi:include href="64bit-core.xml"/></target>"#;
const CORE_XML: &str = r#"<feature name="org.gnu.gdb.i386.core">
<reg name="rax" bitsize="64" regnum="0"/>
<reg name="rsp" bitsize="64" regnum="7"/>
<reg name="rip" bitsize="64" regnum="16"/>
</feature>"#;

#[derive(Debug, Default)]
pub struct Stats {
    pub vruns: usize,
    pub packets: Vec<String>,
}

#[derive(Debug)]
struct Sim {
    layout: Layout,
    pos: usize,
    breakpoints: Vec<u64>,
    watches: Vec<(u64, u64)>,
    absorb: bool,
    at_exit: bool,
    exited: bool,
    checkpoints: BTreeMap<u64, usize>,
    next_checkpoint: u64,
    ack: bool,
    deframer: Deframer,
    out: VecDeque<u8>,
    stats: Stats,
}

fn thread_field() -> String {
    format!("thread:p{TID:x}.{TID:x};")
}

impl Sim {
    fn reply(&mut self, s: &str) {
        self.out.extend(frame(s.as_bytes()));
    }

    fn watch_hit(&self, p: usize) -> bool {
        WRITES.contains(&p)
            && self
                .watches
                .iter()
                .any(|(a, l)| WATCHED < a + l && *a < WATCHED + 8)
    }

    fn forward(&mut self, step: bool) -> String {
        if self.exited {
            panic!("rr aborts on a resume after the end of the trace");
        }
        if self.absorb {
            self.absorb = false;
            return format!("T00{}", thread_field());
        }
        if self.at_exit {
            self.exited = true;
            return "W00".into();
        }
        // A continue that starts on a breakpoint executes its trap at once (which is why
        // debuggers step over a breakpoint before continuing from it).
        if !step && self.breakpoints.contains(&self.layout.rip(self.pos)) {
            return format!("T05{}swbreak:;", thread_field());
        }
        loop {
            if self.pos + 1 >= LEN {
                self.at_exit = true;
                return format!("T09{}", thread_field());
            }
            self.pos += 1;
            if self.watch_hit(self.pos) {
                return format!("T05{}watch:{WATCHED:x};", thread_field());
            }
            if step {
                return format!("T05{}", thread_field());
            }
            if self.breakpoints.contains(&self.layout.rip(self.pos)) {
                return format!("T05{}swbreak:;", thread_field());
            }
        }
    }

    fn backward(&mut self, step: bool) -> String {
        if self.absorb {
            self.absorb = false;
            return format!("T00{}", thread_field());
        }
        self.at_exit = false;
        let start = self.pos;
        loop {
            if self.pos == 0 {
                return format!("T00{}replaylog:begin;", thread_field());
            }
            let leaving = self.pos;
            self.pos -= 1;
            if step {
                return format!("T05{}", thread_field());
            }
            // Running backwards, a watchpoint stops just after the write; rr reports the last
            // stop strictly before where the reverse run began.
            if leaving != start && self.watch_hit(leaving) {
                self.pos = leaving;
                return format!("T05{}watch:{WATCHED:x};", thread_field());
            }
            if self.breakpoints.contains(&self.layout.rip(self.pos)) {
                return format!("T05{}swbreak:;", thread_field());
            }
        }
    }

    fn rr_cmd(&mut self, body: &str) -> String {
        let mut parts = body.split(':');
        let name = parts.next().unwrap();
        let _tid = parts.next().unwrap();
        let args: Vec<String> = parts
            .map(|a| String::from_utf8(from_hex(a.as_bytes()).unwrap()).unwrap())
            .collect();
        let text = match name {
            "when" => format!("Completed event: {}", self.layout.frame_time(self.pos)),
            "when-ticks" => format!("Current tick: {}", self.layout.ticks(self.pos)),
            "when-tid" => format!("Current tid: {TID}"),
            "when-end" => format!(
                "Event at end of recording: {}",
                self.layout.frame_time(LEN - 1)
            ),
            "checkpoint" => {
                let where_ = args.first().expect("rr requires the where argument");
                self.next_checkpoint += 1;
                self.checkpoints.insert(self.next_checkpoint, self.pos);
                format!("Checkpoint {} at {where_}", self.next_checkpoint)
            }
            "delete checkpoint" => {
                let id: u64 = args[0].parse().unwrap();
                match self.checkpoints.remove(&id) {
                    Some(_) => format!("Deleted checkpoint {id}."),
                    None => format!("No checkpoint number {id}."),
                }
            }
            other => format!("Command '{other}' not found."),
        };
        to_hex(format!("{text}\n").as_bytes())
    }

    fn vrun(&mut self, args: &str) -> String {
        self.stats.vruns += 1;
        let arg = args.strip_prefix(';').expect("empty program name");
        let arg = String::from_utf8(from_hex(arg.as_bytes()).unwrap()).unwrap();
        let target = if let Some(c) = arg.strip_prefix('c') {
            match self.checkpoints.get(&c.parse().unwrap()) {
                Some(p) => *p,
                None => return "E01".into(),
            }
        } else if let Some(t) = arg.strip_prefix('t') {
            let t: u64 = t.parse().unwrap();
            match (0..LEN).find(|p| self.layout.ticks(*p) == t) {
                Some(p) => p,
                None => return "E01".into(),
            }
        } else {
            let e: u64 = arg.parse().unwrap();
            (0..LEN)
                .find(|p| self.layout.frame_time(*p) == e)
                .unwrap_or(LEN - 1)
        };
        self.pos = target;
        self.breakpoints.clear();
        self.watches.clear();
        self.absorb = true;
        self.at_exit = false;
        self.exited = false;
        format!("T05{}", thread_field())
    }

    fn handle(&mut self, p: &str) {
        self.stats.packets.push(p.to_string());
        let r: String = if p.starts_with("qSupported") {
            SUPPORTED.into()
        } else if p == "QStartNoAckMode" {
            self.reply("OK");
            self.ack = false;
            return;
        } else if p == "?" {
            format!("T05{}", thread_field())
        } else if p.starts_with("Hg") || p.starts_with("Hc") {
            "OK".into()
        } else if let Some(rest) = p.strip_prefix("qXfer:features:read:") {
            let annex = rest.split(':').next().unwrap();
            match annex {
                "target.xml" => format!("l{TARGET_XML}"),
                "64bit-core.xml" => format!("l{CORE_XML}"),
                _ => "E00".into(),
            }
        } else if p.starts_with("qXfer:exec-file:read:") {
            "l/tmp/fake-prog".into()
        } else if p.starts_with("qXfer:auxv:read:") {
            let mut v = Vec::new();
            for (t, val) in [(9u64, 0x1000u64), (0, 0)] {
                v.extend(t.to_le_bytes());
                v.extend(val.to_le_bytes());
            }
            let mut s = b"l".to_vec();
            s.extend(hopper_rsp::packet::escape_binary(&v));
            self.out.extend(frame(&s));
            return;
        } else if p == "g" {
            let mut b = Vec::new();
            b.extend((self.pos as u64).to_le_bytes());
            b.extend(0x7ff0u64.to_le_bytes());
            b.extend(self.layout.rip(self.pos).to_le_bytes());
            to_hex(&b)
        } else if let Some(rest) = p.strip_prefix('m') {
            let (a, l) = rest.split_once(',').unwrap();
            let a = u64::from_str_radix(a, 16).unwrap();
            let l = usize::from_str_radix(l, 16).unwrap();
            if a >= WATCHED && a + l as u64 <= WATCHED + 8 {
                let v = writes_before(self.pos).to_le_bytes();
                let off = (a - WATCHED) as usize;
                to_hex(&v[off..off + l])
            } else {
                "E01".into()
            }
        } else if let Some(rest) = p.strip_prefix('Z').or(p.strip_prefix('z')) {
            let insert = p.starts_with('Z');
            let mut f = rest.split(',');
            let ty: u8 = f.next().unwrap().parse().unwrap();
            let addr = u64::from_str_radix(f.next().unwrap(), 16).unwrap();
            let len = u64::from_str_radix(f.next().unwrap(), 16).unwrap();
            match (ty, insert) {
                (0 | 1, true) => self.breakpoints.push(addr),
                (0 | 1, false) => self.breakpoints.retain(|b| *b != addr),
                (_, true) => self.watches.push((addr, len)),
                (_, false) => self.watches.retain(|w| *w != (addr, len)),
            }
            "OK".into()
        } else if p == "vCont?" {
            "vCont;c;C;s;S;".into()
        } else if p == "vCont;c" {
            self.forward(false)
        } else if p.starts_with("vCont;s:") {
            self.forward(true)
        } else if p == "bc" {
            self.backward(false)
        } else if p == "bs" {
            self.backward(true)
        } else if let Some(rest) = p.strip_prefix("qRRCmd:") {
            self.rr_cmd(rest)
        } else if let Some(rest) = p.strip_prefix("vRun;") {
            self.vrun(rest)
        } else if p == "qfThreadInfo" {
            format!("mp{TID:x}.{TID:x}")
        } else if p == "qsThreadInfo" {
            "l".into()
        } else {
            String::new()
        };
        self.reply(&r);
    }
}

#[derive(Clone, Debug)]
pub struct FakeRr {
    sim: Arc<Mutex<Sim>>,
}

impl FakeRr {
    /// A server stopped at position `start`.
    pub fn new(start: usize) -> FakeRr {
        FakeRr::with_layout(start, Layout::DEFAULT)
    }

    /// A server for a program with timeline `layout`, stopped at position `start`.
    pub fn with_layout(start: usize, layout: Layout) -> FakeRr {
        FakeRr {
            sim: Arc::new(Mutex::new(Sim {
                layout,
                pos: start,
                breakpoints: vec![],
                watches: vec![],
                absorb: false,
                at_exit: false,
                exited: false,
                checkpoints: BTreeMap::new(),
                next_checkpoint: 0,
                ack: true,
                deframer: Deframer::default(),
                out: VecDeque::new(),
                stats: Stats::default(),
            })),
        }
    }

    pub fn pos(&self) -> usize {
        self.sim.lock().unwrap().pos
    }

    pub fn vruns(&self) -> usize {
        self.sim.lock().unwrap().stats.vruns
    }

    pub fn inserted_breakpoints(&self) -> Vec<u64> {
        self.sim.lock().unwrap().breakpoints.clone()
    }

    /// Every packet received since the last call.
    #[allow(dead_code, reason = "not every test binary inspects packets")]
    pub fn take_packets(&self) -> Vec<String> {
        std::mem::take(&mut self.sim.lock().unwrap().stats.packets)
    }
}

impl Read for FakeRr {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut s = self.sim.lock().unwrap();
        if s.out.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "fake rr has nothing to say",
            ));
        }
        let n = buf.len().min(s.out.len());
        for (slot, b) in buf.iter_mut().zip(s.out.drain(..n)) {
            *slot = b;
        }
        Ok(n)
    }
}

impl Write for FakeRr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.sim.lock().unwrap();
        s.deframer.push(buf);
        while let Some(item) = s.deframer.next_item().unwrap() {
            match item {
                Incoming::Packet(p) => {
                    if s.ack {
                        s.out.push_back(b'+');
                    }
                    let p = String::from_utf8(p).unwrap();
                    s.handle(&p);
                }
                Incoming::Ack | Incoming::Nack => {}
                other => panic!("fake rr got {other:?}"),
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Connection for FakeRr {
    fn interrupt_writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.clone()))
    }
}
