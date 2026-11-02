//! The rr phase-B producer: `rr rerun --singlestep` shards, run in parallel.
//!
//! **What rerun prints** (rr `RerunCommand.cc`, `TraceField.cc` at the rr sources hopper read):
//! with `--singlestep=<fields>` rerun replays the event range `[-s, -e)` and, after every single
//! step of a user instruction, prints the stepped task's fields. With `-r` (raw) each field is
//! its little-endian bytes (8 for `event`, `icount`, `ip`, `flags`, `fsbase`, `gsbase`, `ticks`
//! and each `gp_x16` register; 4 for `tid`) and each record ends with `\n`; without `-r` a
//! record is `name:value` pairs separated by spaces, decimal for `event`, `icount`, `tid` and
//! `ticks`, `0x` hex (no leading zeros) otherwise. `gp_x16` is the 16 general registers in
//! architectural order: rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8–r15. The first record is the
//! state before the first step, printed with `event` one less than the start event. A `rep`
//! string instruction that was not interrupted is one step; a step can also be a state change
//! with no instruction (a syscall exit), which this module recognises as a record with the same
//! `ip` and registers as the task's previous one and skips.
//!
//! **Writes.** rerun dumps registers, not memory. The instruction that took the task from one
//! record to its next is decoded from the binaries' own code bytes ([`CodeSource`]), and its
//! write sites come from [`super::decode::instruction_writes`] with the earlier record as the
//! before-state. The written bytes are known when they come from a register or an immediate;
//! otherwise the write is logged as [`WriteValue::Unrecorded`].
//!
//! **Moments.** `frame_time` is the record's `event` (rr's `when`: the frame being replayed,
//! `TraceReader::time`), `ticks` its `ticks`, and `sub` counts the task's records since its
//! (event, ticks) last changed. That matches [`hopper_rr::RrReplica`]'s `sub` when every record
//! is one instruction; the `rr_rerun_moments_match_replica` test checks it on a PMU host.
//!
//! **Shards.** The window's events are split into contiguous ranges. Each shard is either a plain
//! `rr rerun -s <start> -e <end>` (replaying to its start at full speed, then stepping) or, with
//! [`ShardStart::ImportCheckpoint`], an exporter `rr rerun --export-checkpoints=<start>,1,<socket>`
//! plus an importer `rr rerun --import-checkpoint=<socket> --singlestep=... -s <start> -e <end>`
//! that runs in the exporter's forked session.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use hopper_core::{
    HopperError, Moment, Place, ProcessId, RecordingId, Reg, RegisterSet, Result, Tid,
};
use hopper_rr::Rr;
use hopper_symbols::SymbolSet;

use super::decode::{MAX_INSTRUCTION_LEN, decode_at, instruction_writes};
use super::{LoggedWrite, WriteLogSummary, WriteValue};
use crate::control::IndexControl;
use crate::index::{Index, IndexKind};
use crate::store::{Batch, CoverageScope, CoveredInterval, Record, Stamp};

/// One field of rerun's `--singlestep` list.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum RerunField {
    Event,
    Icount,
    Ip,
    Flags,
    FsBase,
    GsBase,
    Tid,
    Ticks,
    /// General register `n` in rerun's architectural order (0 rax, 1 rcx, 2 rdx, 3 rbx, 4 rsp,
    /// 5 rbp, 6 rsi, 7 rdi, 8–15 r8–r15).
    Gp(u8),
}

const GP_ORDER: [Reg; 16] = [
    Reg::RAX,
    Reg::RCX,
    Reg::RDX,
    Reg::RBX,
    Reg::RSP,
    Reg::RBP,
    Reg::RSI,
    Reg::RDI,
    Reg(8),
    Reg(9),
    Reg(10),
    Reg(11),
    Reg(12),
    Reg(13),
    Reg(14),
    Reg(15),
];

const GP_NAMES: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

impl RerunField {
    /// Bytes in a raw record.
    pub fn raw_size(self) -> usize {
        match self {
            RerunField::Tid => 4,
            _ => 8,
        }
    }
}

/// The fields hopper asks for: everything the write decoder and the moments need.
pub fn write_log_fields() -> Vec<RerunField> {
    let mut f = vec![
        RerunField::Event,
        RerunField::Icount,
        RerunField::Tid,
        RerunField::Ticks,
        RerunField::Ip,
        RerunField::Flags,
        RerunField::FsBase,
        RerunField::GsBase,
    ];
    f.extend((0..16).map(RerunField::Gp));
    f
}

/// The `--singlestep=` argument for `fields` (consecutive `Gp(0..16)` collapse to `gp_x16`).
pub fn fields_arg(fields: &[RerunField]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let all_gp =
            fields.len() - i >= 16 && (0..16).all(|k| fields[i + k] == RerunField::Gp(k as u8));
        if all_gp {
            parts.push("gp_x16".to_string());
            i += 16;
            continue;
        }
        parts.push(
            match fields[i] {
                RerunField::Event => "event",
                RerunField::Icount => "icount",
                RerunField::Ip => "ip",
                RerunField::Flags => "flags",
                RerunField::FsBase => "fsbase",
                RerunField::GsBase => "gsbase",
                RerunField::Tid => "tid",
                RerunField::Ticks => "ticks",
                RerunField::Gp(n) => GP_NAMES[n as usize % 16],
            }
            .to_string(),
        );
        i += 1;
    }
    parts.join(",")
}

/// One rerun record: a task's state after a step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRecord {
    pub event: u64,
    pub icount: u64,
    pub tid: Tid,
    pub ticks: u64,
    /// `rip`, `eflags`, `fs_base`, `gs_base` and the general registers that were asked for.
    pub regs: RegisterSet,
}

impl StepRecord {
    fn empty() -> Self {
        StepRecord {
            event: 0,
            icount: 0,
            tid: Tid(0),
            ticks: 0,
            regs: RegisterSet::default(),
        }
    }

    fn set(&mut self, field: RerunField, value: u64) {
        match field {
            RerunField::Event => self.event = value,
            RerunField::Icount => self.icount = value,
            RerunField::Tid => self.tid = Tid(value as u32 as i32),
            RerunField::Ticks => self.ticks = value,
            RerunField::Ip => self.regs.set(Reg::RIP, value),
            RerunField::Flags => self.regs.set(Reg::RFLAGS, value),
            RerunField::FsBase => self.regs.set(Reg::FS_BASE, value),
            RerunField::GsBase => self.regs.set(Reg::GS_BASE, value),
            RerunField::Gp(n) => self.regs.set(GP_ORDER[n as usize % 16], value),
        }
    }
}

/// Parse raw (`-r`) output: fixed-size records of `fields`, each followed by `\n`.
pub fn parse_raw(bytes: &[u8], fields: &[RerunField]) -> Result<Vec<StepRecord>> {
    let mut parser = RawParser::new(fields);
    let mut out = parser.push(bytes)?;
    if !parser.buf.is_empty() {
        return Err(HopperError::Protocol(format!(
            "rerun output ends inside a record ({} of {} bytes)",
            parser.buf.len(),
            parser.record_len
        )));
    }
    out.shrink_to_fit();
    Ok(out)
}

/// An incremental raw parser for streaming rerun's stdout.
#[derive(Debug)]
pub struct RawParser {
    fields: Vec<RerunField>,
    record_len: usize,
    buf: Vec<u8>,
}

impl RawParser {
    pub fn new(fields: &[RerunField]) -> Self {
        let record_len = fields.iter().map(|f| f.raw_size()).sum::<usize>() + 1;
        RawParser {
            fields: fields.to_vec(),
            record_len,
            buf: Vec::new(),
        }
    }

    /// Feed bytes; returns the records they completed.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<StepRecord>> {
        self.buf.extend_from_slice(bytes);
        let n = self.buf.len() / self.record_len;
        let mut out = Vec::with_capacity(n);
        for k in 0..n {
            let rec = &self.buf[k * self.record_len..(k + 1) * self.record_len];
            if rec[self.record_len - 1] != b'\n' {
                return Err(HopperError::Protocol(format!(
                    "rerun raw record {k} does not end with a newline: the field list does not match"
                )));
            }
            let mut r = StepRecord::empty();
            let mut at = 0;
            for f in &self.fields {
                let size = f.raw_size();
                let mut v = [0u8; 8];
                v[..size].copy_from_slice(&rec[at..at + size]);
                r.set(*f, u64::from_le_bytes(v));
                at += size;
            }
            out.push(r);
        }
        self.buf.drain(..n * self.record_len);
        Ok(out)
    }
}

fn text_field(name: &str) -> Option<RerunField> {
    Some(match name {
        "event" => RerunField::Event,
        "icount" => RerunField::Icount,
        "rip" | "eip" => RerunField::Ip,
        "rflags" | "eflags" => RerunField::Flags,
        "fsbase" => RerunField::FsBase,
        "gsbase" => RerunField::GsBase,
        "tid" => RerunField::Tid,
        "ticks" => RerunField::Ticks,
        other => RerunField::Gp(GP_NAMES.iter().position(|g| *g == other)? as u8),
    })
}

/// Parse text output (without `-r`). Fields hopper does not model are ignored.
pub fn parse_text(text: &str) -> Result<Vec<StepRecord>> {
    let mut out = Vec::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let mut r = StepRecord::empty();
        for pair in line.split_whitespace() {
            let (name, value) = pair.split_once(':').ok_or_else(|| {
                HopperError::Protocol(format!("rerun line {n}: {pair:?} is not name:value"))
            })?;
            let parsed = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse::<i64>().map(|v| v as u64),
            }
            .map_err(|_| HopperError::Protocol(format!("rerun line {n}: bad value {value:?}")))?;
            if let Some(f) = text_field(name) {
                r.set(f, parsed);
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// Code bytes at a runtime address, for decoding rerun's steps.
pub trait CodeSource {
    fn code_at(&self, pc: u64) -> Option<Vec<u8>>;
}

impl CodeSource for SymbolSet {
    /// From the binary file of the module that maps `pc` (code is not self-modifying).
    fn code_at(&self, pc: u64) -> Option<Vec<u8>> {
        let (_, m) = self.module_at(pc)?;
        let file = m.to_file(pc);
        for len in (1..=MAX_INSTRUCTION_LEN).rev() {
            let mut buf = vec![0u8; len];
            if m.binary().read_static(file, &mut buf).is_ok() {
                return Some(buf);
            }
        }
        None
    }
}

/// The general registers and rip, which tell whether a record executed anything.
fn same_state(a: &RegisterSet, b: &RegisterSet) -> bool {
    a.values
        .iter()
        .filter(|(r, _)| r.0 <= Reg::RIP.0)
        .all(|(r, v)| b.get_bytes(*r) == Some(v.as_slice()))
}

/// Turns step records into writes, keeping per-task state across calls (records of one shard
/// arrive in batches).
#[derive(Debug)]
pub struct StepDecoder {
    recording: RecordingId,
    process: ProcessId,
    /// Per task: its previous record and that record's sub.
    last: std::collections::HashMap<Tid, (StepRecord, u32)>,
    pub instructions: u64,
    pub writes: u64,
    pub undecoded: u64,
    pub first: Option<Moment>,
    pub last_moment: Option<Moment>,
}

impl StepDecoder {
    pub fn new(recording: RecordingId, process: ProcessId) -> Self {
        StepDecoder {
            recording,
            process,
            last: Default::default(),
            instructions: 0,
            writes: 0,
            undecoded: 0,
            first: None,
            last_moment: None,
        }
    }

    fn moment(&self, r: &StepRecord, sub: u32) -> Moment {
        Moment {
            recording: self.recording,
            frame_time: r.event,
            tid: r.tid,
            ticks: r.ticks,
            sub,
        }
    }

    /// Decode `records` (in output order) into writes.
    pub fn feed(&mut self, records: Vec<StepRecord>, code: &dyn CodeSource) -> Vec<LoggedWrite> {
        let mut out = Vec::new();
        for r in records {
            let Some((prev, prev_sub)) = self.last.remove(&r.tid) else {
                let m = self.moment(&r, 0);
                self.first.get_or_insert(m);
                self.last_moment = Some(m);
                self.last.insert(r.tid, (r, 0));
                continue;
            };
            if prev.regs.pc() == r.regs.pc() && same_state(&prev.regs, &r.regs) {
                // A state change with no instruction (syscall exit, signal): nothing executed.
                self.last.insert(r.tid, (prev, prev_sub));
                continue;
            }
            self.instructions += 1;
            let sub = if (prev.event, prev.ticks) == (r.event, r.ticks) {
                prev_sub + 1
            } else {
                0
            };
            let at = self.moment(&prev, prev_sub);
            match prev.regs.pc().and_then(|pc| Some((pc, code.code_at(pc)?))) {
                Some((pc, bytes)) => match decode_at(&bytes, pc) {
                    Some(insn) => {
                        for site in instruction_writes(&insn, &prev.regs, Some(&r.regs)) {
                            self.writes += 1;
                            out.push(LoggedWrite {
                                at,
                                pc,
                                place: Place::memory(self.process, site.addr, site.len),
                                value: site.value.map_or(WriteValue::Unrecorded, WriteValue::Bytes),
                            });
                        }
                    }
                    None => self.undecoded += 1,
                },
                None => self.undecoded += 1,
            }
            let m = self.moment(&r, sub);
            self.last_moment = Some(m);
            self.last.insert(r.tid, (r, sub));
        }
        out
    }
}

/// Decode a whole shard's records at once.
pub fn writes_from_steps(
    records: Vec<StepRecord>,
    recording: RecordingId,
    process: ProcessId,
    code: &dyn CodeSource,
) -> (Vec<LoggedWrite>, StepDecoder) {
    let mut d = StepDecoder::new(recording, process);
    let w = d.feed(records, code);
    (w, d)
}

/// A contiguous event range `[start_event, end_event)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Shard {
    pub start_event: u64,
    pub end_event: u64,
}

/// Split `[start, end)` into at most `n` contiguous non-empty shards of near-equal event counts.
pub fn plan_shards(start: u64, end: u64, n: usize) -> Vec<Shard> {
    if end <= start || n == 0 {
        return Vec::new();
    }
    let total = end - start;
    let n = (n as u64).min(total);
    (0..n)
        .map(|k| Shard {
            start_event: start + total * k / n,
            end_event: start + total * (k + 1) / n,
        })
        .collect()
}

/// How each shard reaches its start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShardStart {
    /// Replay from the start of the trace (at full speed) to the shard's start.
    Replay,
    /// An exporter forks a checkpoint at the shard's start for the stepping importer; sockets
    /// are created in this directory.
    ImportCheckpoint { socket_dir: PathBuf },
}

/// `rr rerun` single-stepping one shard, raw output on stdout.
pub fn stepping_command(
    rr: &Rr,
    trace: Option<&Path>,
    shard: &Shard,
    fields: &[RerunField],
) -> Command {
    let mut c = rr.command();
    c.arg("rerun")
        .arg(format!("--singlestep={}", fields_arg(fields)))
        .arg("-r");
    // rerun refuses `-s 0` (its minimum is 1); 0 is the default anyway.
    if shard.start_event > 0 {
        c.arg("-s").arg(shard.start_event.to_string());
    }
    c.arg("-e").arg(shard.end_event.to_string());
    if let Some(t) = trace {
        c.arg(t);
    }
    c
}

/// `rr rerun --export-checkpoints=<event>,1,<socket> <trace>`.
pub fn exporter_command(rr: &Rr, trace: &Path, event: u64, socket: &Path) -> Command {
    let mut c = rr.command();
    c.arg("rerun")
        .arg(format!(
            "--export-checkpoints={event},1,{}",
            socket.display()
        ))
        .arg(trace);
    c
}

/// `rr rerun --import-checkpoint=<socket> --singlestep=... -r -s <start> -e <end>`.
pub fn importer_command(rr: &Rr, socket: &Path, shard: &Shard, fields: &[RerunField]) -> Command {
    let mut c = stepping_command(rr, None, shard, fields);
    c.arg(format!("--import-checkpoint={}", socket.display()));
    c
}

/// A phase-B run over rr.
#[derive(Clone, Debug)]
pub struct RerunJob {
    pub rr: Rr,
    pub trace_dir: PathBuf,
    pub recording: RecordingId,
    pub process: ProcessId,
    pub shards: Vec<Shard>,
    pub start: ShardStart,
    /// Writes buffered per shard before a chunk is committed.
    pub flush_every: usize,
}

fn spawn(mut c: Command, stdout: Stdio) -> Result<Child> {
    let desc = format!("{c:?}");
    c.stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| HopperError::Io(format!("cannot run {desc}: {e}")))
}

fn run_shard(
    job: &RerunJob,
    k: usize,
    shard: &Shard,
    code: &(dyn CodeSource + Sync),
    index: &Index,
    control: &IndexControl,
) -> Result<StepDecoder> {
    let fields = write_log_fields();
    let (mut exporter, mut stepper) = match &job.start {
        ShardStart::Replay => (
            None,
            spawn(
                stepping_command(&job.rr, Some(&job.trace_dir), shard, &fields),
                Stdio::piped(),
            )?,
        ),
        ShardStart::ImportCheckpoint { socket_dir } => {
            std::fs::create_dir_all(socket_dir)?;
            let socket = socket_dir.join(format!("shard-{k}.sock"));
            let exp = spawn(
                exporter_command(&job.rr, &job.trace_dir, shard.start_event, &socket),
                Stdio::null(),
            )?;
            (
                Some(exp),
                spawn(
                    importer_command(&job.rr, &socket, shard, &fields),
                    Stdio::piped(),
                )?,
            )
        }
    };
    let mut parser = RawParser::new(&fields);
    let mut decoder = StepDecoder::new(job.recording, job.process);
    let mut out = BufReader::with_capacity(1 << 20, stepper.stdout.take().expect("piped stdout"));
    let mut buf = vec![0u8; 1 << 20];
    let mut pending: Vec<Record> = Vec::new();
    let interval = |d: &StepDecoder| CoveredInterval {
        // rerun prints the state before the first step as event `start - 1`.
        from: Some(Stamp(shard.start_event.saturating_sub(1), 0, 0, 0)),
        through: d
            .last_moment
            .as_ref()
            .map(Stamp::of)
            .or(Some(Stamp(shard.start_event, 0, 0, 0))),
        scope: CoverageScope::AllThreads,
    };
    let commit = |pending: &mut Vec<Record>, d: &StepDecoder| -> Result<()> {
        let mut cov = index.coverage(IndexKind::Writes)?;
        let iv = interval(d);
        match cov
            .intervals
            .iter_mut()
            .find(|i| i.from == iv.from && i.scope == iv.scope)
        {
            Some(i) => *i = iv,
            None => cov.intervals.push(iv),
        }
        index.commit(Batch {
            chunks: vec![(IndexKind::Writes, std::mem::take(pending))],
            coverage: vec![(IndexKind::Writes, cov)],
            resume: None,
        })
    };
    loop {
        if control.is_cancelled() {
            let _ = stepper.kill();
            if let Some(e) = exporter.as_mut() {
                let _ = e.kill();
            }
            commit(&mut pending, &decoder)?;
            return Err(HopperError::Cancelled);
        }
        let n = out.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let records = parser.push(&buf[..n])?;
        for w in decoder.feed(records, code) {
            pending.push(Record::Write {
                at: Stamp::of(&w.at),
                pc: w.pc,
                place: w.place,
                value: w.value,
            });
        }
        if pending.len() >= job.flush_every.max(1) {
            commit(&mut pending, &decoder)?;
        }
    }
    let status = stepper.wait()?;
    if let Some(mut e) = exporter {
        let _ = e.wait();
    }
    if !status.success() {
        let mut err = String::new();
        if let Some(mut s) = stepper.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        return Err(HopperError::TargetRefused {
            request: format!(
                "rr rerun shard {k} ({}..{})",
                shard.start_event, shard.end_event
            ),
            reply: format!("{status}: {}", err.trim()),
        });
    }
    commit(&mut pending, &decoder)?;
    Ok(decoder)
}

/// Run every shard of `job` in parallel (one thread and one rr process, or two with checkpoints,
/// per shard) and log their writes into `index`.
pub fn run_rerun_shards(
    job: &RerunJob,
    code: &(dyn CodeSource + Sync),
    index: &Index,
    control: &IndexControl,
) -> Result<WriteLogSummary> {
    let started = Instant::now();
    let results: Vec<Result<StepDecoder>> = std::thread::scope(|s| {
        let handles: Vec<_> = job
            .shards
            .iter()
            .enumerate()
            .map(|(k, shard)| s.spawn(move || run_shard(job, k, shard, code, index, control)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(HopperError::Index("a rerun shard panicked".into())))
            })
            .collect()
    });
    let mut summary = WriteLogSummary {
        instructions: 0,
        writes: 0,
        undecoded: 0,
        first: None,
        last: None,
        elapsed: Default::default(),
    };
    for r in results {
        let d = r?;
        summary.instructions += d.instructions;
        summary.writes += d.writes;
        summary.undecoded += d.undecoded;
        if summary
            .first
            .is_none_or(|f| d.first.is_some_and(|x| x.is_before(&f)))
        {
            summary.first = d.first.or(summary.first);
        }
        if summary
            .last
            .is_none_or(|l| d.last_moment.is_some_and(|x| l.is_before(&x)))
        {
            summary.last = d.last_moment.or(summary.last);
        }
    }
    summary.elapsed = started.elapsed();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Code bytes from a map, for tests.
    struct Code(Vec<(u64, Vec<u8>)>);

    impl CodeSource for Code {
        fn code_at(&self, pc: u64) -> Option<Vec<u8>> {
            self.0
                .iter()
                .find(|(a, _)| *a == pc)
                .map(|(_, b)| b.clone())
        }
    }

    /// A raw record laid out exactly as `print_trace_fields` writes it for `write_log_fields()`.
    fn raw_record(
        event: u64,
        icount: u64,
        tid: i32,
        ticks: u64,
        ip: u64,
        gp: [u64; 16],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&event.to_le_bytes());
        b.extend_from_slice(&icount.to_le_bytes());
        b.extend_from_slice(&(tid as u32).to_le_bytes());
        b.extend_from_slice(&ticks.to_le_bytes());
        b.extend_from_slice(&ip.to_le_bytes());
        b.extend_from_slice(&0x246u64.to_le_bytes()); // flags
        b.extend_from_slice(&0x7f00_0000u64.to_le_bytes()); // fsbase
        b.extend_from_slice(&0u64.to_le_bytes()); // gsbase
        for g in gp {
            b.extend_from_slice(&g.to_le_bytes());
        }
        b.push(b'\n');
        b
    }

    #[test]
    fn field_argument_matches_rerun_names() {
        assert_eq!(
            fields_arg(&write_log_fields()),
            "event,icount,tid,ticks,ip,flags,fsbase,gsbase,gp_x16"
        );
        assert_eq!(fields_arg(&[RerunField::Ip, RerunField::Gp(4)]), "ip,rsp");
        let rec_len: usize = write_log_fields()
            .iter()
            .map(|f| f.raw_size())
            .sum::<usize>()
            + 1;
        assert_eq!(rec_len, 8 + 8 + 4 + 8 + 8 + 8 + 8 + 8 + 16 * 8 + 1);
    }

    #[test]
    fn parses_raw_records_and_decodes_writes() {
        let rsp = 0x7ffe_0000u64;
        let mut gp = [0u64; 16];
        gp[4] = rsp; // rsp
        gp[3] = 0xdead_beef; // rbx
        let fields = write_log_fields();
        // Initial state (event start-1), then `push rbx` at 0x1000, then `mov [rsp], 7` at
        // 0x1001 (7 bytes), then a syscall-exit record that changes nothing.
        let mut out = raw_record(9, 0, 100, 50, 0x1000, gp);
        let mut gp2 = gp;
        gp2[4] = rsp - 8;
        out.extend(raw_record(10, 1, 100, 50, 0x1001, gp2));
        out.extend(raw_record(10, 2, 100, 50, 0x1009, gp2));
        out.extend(raw_record(10, 2, 100, 50, 0x1009, gp2));
        let records = parse_raw(&out, &fields).unwrap();
        assert_eq!(records.len(), 4);
        assert_eq!(records[1].regs.pc(), Some(0x1001));
        assert_eq!(records[1].regs.sp(), Some(rsp - 8));
        assert_eq!(records[0].tid, Tid(100));
        assert_eq!(records[0].regs.get(Reg::RBX), Some(0xdead_beef));
        assert_eq!(records[0].regs.get(Reg::FS_BASE), Some(0x7f00_0000));
        // A truncated stream is an error, not a short result.
        assert!(parse_raw(&out[..out.len() - 3], &fields).is_err());

        let code = Code(vec![
            (0x1000, vec![0x53]),
            (0x1001, vec![0x48, 0xc7, 0x04, 0x24, 0x07, 0, 0, 0]),
        ]);
        let rec = RecordingId([1; 16]);
        let (writes, d) = writes_from_steps(records, rec, ProcessId::new(100), &code);
        assert_eq!(d.instructions, 2);
        assert_eq!(d.writes, 2);
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].pc, 0x1000);
        assert_eq!(
            writes[0].place,
            Place::memory(ProcessId::new(100), rsp - 8, 8)
        );
        assert_eq!(
            writes[0].value,
            WriteValue::Bytes(0xdead_beefu64.to_le_bytes().to_vec())
        );
        assert_eq!(writes[0].at.order_key(), (9, 50, 0));
        assert_eq!(
            writes[1].place,
            Place::memory(ProcessId::new(100), rsp - 8, 8)
        );
        assert_eq!(
            writes[1].value,
            WriteValue::Bytes(7u64.to_le_bytes().to_vec())
        );
        // The second instruction ran inside event 10 at the same ticks: sub restarts there.
        assert_eq!(writes[1].at.order_key(), (10, 50, 0));
    }

    #[test]
    fn streaming_parser_handles_split_reads() {
        let fields = write_log_fields();
        let one = raw_record(1, 0, 5, 0, 0x10, [0; 16]);
        let mut p = RawParser::new(&fields);
        assert!(p.push(&one[..50]).unwrap().is_empty());
        let mut rest = one[50..].to_vec();
        rest.extend(&one);
        assert_eq!(p.push(&rest).unwrap().len(), 2);
        // A record without its newline means the field list is wrong.
        let mut bad = one.clone();
        *bad.last_mut().unwrap() = b'x';
        assert!(RawParser::new(&fields).push(&bad).is_err());
    }

    #[test]
    fn parses_text_records() {
        let text = "event:12 icount:3 tid:4242 ticks:77 rip:0x401000 rflags:0x246 rax:0x0 rcx:0x5 rsp:0x7ffe0000\n\
                    event:12 icount:4 tid:4242 ticks:77 rip:0x401003 rflags:0x246 rax:0x1 rcx:0x5 rsp:0x7ffe0000\n";
        let r = parse_text(text).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].event, 12);
        assert_eq!(r[0].tid, Tid(4242));
        assert_eq!(r[0].ticks, 77);
        assert_eq!(r[0].regs.pc(), Some(0x401000));
        assert_eq!(r[1].regs.get(Reg::RAX), Some(1));
        assert_eq!(r[1].regs.get(Reg::RCX), Some(5));
        assert!(parse_text("event:x\n").is_err());
        assert!(parse_text("garbage\n").is_err());
    }

    #[test]
    fn shards_cover_the_range() {
        let s = plan_shards(10, 20, 3);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].start_event, 10);
        assert_eq!(s[2].end_event, 20);
        assert!(s.windows(2).all(|w| w[0].end_event == w[1].start_event));
        assert_eq!(plan_shards(5, 7, 8).len(), 2);
        assert!(plan_shards(5, 5, 2).is_empty());
    }

    #[test]
    fn commands_match_rerun_flags() {
        let rr = Rr {
            binary: "rr".into(),
            version: hopper_rr::RrVersion::parse("rr version 5.9.0").unwrap(),
        };
        let fields = write_log_fields();
        let args = |c: &Command| {
            c.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let shard = Shard {
            start_event: 0,
            end_event: 40,
        };
        let c = stepping_command(&rr, Some(Path::new("/t")), &shard, &fields);
        assert_eq!(
            args(&c),
            [
                "rerun",
                "--singlestep=event,icount,tid,ticks,ip,flags,fsbase,gsbase,gp_x16",
                "-r",
                "-e",
                "40",
                "/t"
            ]
        );
        let shard = Shard {
            start_event: 12,
            end_event: 40,
        };
        let e = exporter_command(&rr, Path::new("/t"), 12, Path::new("/s/x.sock"));
        assert_eq!(
            args(&e),
            ["rerun", "--export-checkpoints=12,1,/s/x.sock", "/t"]
        );
        let i = importer_command(&rr, Path::new("/s/x.sock"), &shard, &fields);
        let a = args(&i);
        assert!(a.contains(&"--import-checkpoint=/s/x.sock".to_string()));
        assert!(a.windows(2).any(|w| w == ["-s", "12"]));
    }
}
