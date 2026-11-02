//! hopper's Debug Adapter Protocol server, over stdio with `Content-Length` framing.
//!
//! It maps DAP's standard requests onto [`Debugger`], adds DAP's reverse execution
//! (`stepBack`, `reverseContinue`) and `goto` (every execution of a line is a goto target), and
//! exposes hopper's omniscient queries as `hopper/*` custom requests.
//!
//! Choices that differ from other rr front ends:
//! - **`hopper/*` requests answer in the response body**, never through side events: `hopper/calls`,
//!   `hopper/lineHits`, `hopper/origin`, `hopper/valueHistory`, `hopper/lastWrite`, `hopper/seek`
//!   (taking a position URL) and `hopper/cursor`. Payloads are hopper's wire JSON
//!   ([`hopper_core::wire`]): moments are position URL strings and enums are string-tagged
//!   (`"Move"`, `{"Breakpoint": {"id": 1}}`), never integers. The handshake advertises
//!   [`HOPPER_PROTOCOL_VERSION`] as `hopperProtocolVersion`.
//! - **`variablesReference`s never expire.** Each one indexes a table of [`ValueHandle`]s (and
//!   frame-locals and register scopes), and a handle names "this expression, in this frame, at
//!   this moment". The table is never cleared on `continue`, step or seek (midas clears its
//!   table on every resume, so its references die at the next stop): a reference obtained at one
//!   stop still answers `variables` after the cursor has moved, and still describes the value at
//!   the moment it was taken. The same handle always gets the same reference, so the table grows
//!   only with distinct values a client has looked at. Frame ids (`stackTrace`) and goto target
//!   ids work the same way.
//! - The recording never runs off on its own: after `configurationDone` the adapter reports a
//!   stop at the cursor (`reason: "entry"`), and reaching either end of the recording is a stop
//!   (`reason: "pause"`, with an `output` event), not termination, because the user can still go
//!   back. `terminated` is sent only for `terminate` and `disconnect`.
//! - Every stop, stack frame and variable carries `hopperMoment` (a position URL) beside the
//!   standard fields; variables carry `hopperOwnership` (live, moved out, dropped).

mod framing;

use std::collections::HashMap;
use std::hash::Hash;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use hopper_core::api::{
    BreakLocation, BreakpointId, Budget, CallFilter, CallId, Debugger, OwnershipState, PageReq,
    StepKind, Stop, StopCause, Until, ValueHandle, ValueView,
};
use hopper_core::target::Direction;
use hopper_core::wire::{self, moment_json};
use hopper_core::{FrameId, HopperError, Moment, MomentRange, Place, Result, SourceLoc, Tid};
use serde_json::{Map, Value, json};

/// The version of the `hopper/*` extensions, advertised in the `initialize` response.
pub const HOPPER_PROTOCOL_VERSION: &str = "1";

/// What a `launch` or `attach` request asks to open: `{"recording": "<trace dir>"}` or
/// `{"fake": true}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    Recording(PathBuf),
    /// A program to run live under gdbserver (forward only).
    Live {
        program: PathBuf,
        args: Vec<std::ffi::OsString>,
    },
    Fake,
}

impl LaunchTarget {
    pub fn from_arguments(args: &Value) -> Result<LaunchTarget> {
        if args.get("fake").and_then(Value::as_bool) == Some(true) {
            return Ok(LaunchTarget::Fake);
        }
        if let Some(p) = args.get("program").and_then(Value::as_str)
            && !p.is_empty()
        {
            let program_args = match args.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|a| {
                        a.as_str().map(std::ffi::OsString::from).ok_or_else(|| {
                            HopperError::Invalid("\"args\" must be an array of strings".into())
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                Some(_) => {
                    return Err(HopperError::Invalid(
                        "\"args\" must be an array of strings".into(),
                    ));
                }
            };
            return Ok(LaunchTarget::Live {
                program: PathBuf::from(p),
                args: program_args,
            });
        }
        match args.get("recording").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => Ok(LaunchTarget::Recording(PathBuf::from(p))),
            _ => Err(HopperError::Invalid(
                "launch and attach take {\"recording\": \"<trace dir>\"}, {\"program\": \
                 \"<path>\", \"args\": [..]} (live) or {\"fake\": true}"
                    .into(),
            )),
        }
    }
}

/// Serve one DAP session until `disconnect` or the end of input. `factory` opens the
/// debugger a `launch` or `attach` request names.
pub fn serve<F>(factory: F, mut reader: impl BufRead, writer: impl Write) -> io::Result<()>
where
    F: FnMut(&LaunchTarget) -> Result<Arc<dyn Debugger>>,
{
    let mut session = Session::new(factory, writer);
    while let Some(msg) = framing::read_message(&mut reader)? {
        if msg.get("type").and_then(Value::as_str) != Some("request") {
            continue;
        }
        if session.handle(&msg)? == Flow::Stop {
            break;
        }
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// A table that hands out stable integer ids (from 1) for values, and never forgets them.
#[derive(Debug)]
struct RefTable<T> {
    items: Vec<T>,
    ids: HashMap<T, i64>,
}

impl<T: Clone + Eq + Hash> RefTable<T> {
    fn new() -> Self {
        RefTable {
            items: Vec::new(),
            ids: HashMap::new(),
        }
    }

    fn id(&mut self, item: T) -> i64 {
        if let Some(id) = self.ids.get(&item) {
            return *id;
        }
        self.items.push(item.clone());
        let id = self.items.len() as i64;
        self.ids.insert(item, id);
        id
    }

    fn get(&self, id: i64) -> Option<&T> {
        usize::try_from(id)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| self.items.get(i))
    }
}

/// A frame at a moment (what a DAP `frameId` names).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FrameAt {
    at: Moment,
    frame: FrameId,
}

/// What a `variablesReference` names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum VarRef {
    Locals(FrameAt),
    Registers { at: Moment, tid: Tid },
    Value(ValueHandle),
}

/// How a request failed: an engine error, or a request the session cannot take now.
type Answer = std::result::Result<Value, HopperError>;

struct Session<F, W> {
    factory: F,
    out: W,
    seq: i64,
    debugger: Option<Arc<dyn Debugger>>,
    frames: RefTable<FrameAt>,
    vars: RefTable<VarRef>,
    gotos: RefTable<Moment>,
    source_breakpoints: HashMap<String, Vec<BreakpointId>>,
    function_breakpoints: Vec<BreakpointId>,
    /// Events to send after the current response.
    pending: Vec<(String, Value)>,
}

fn invalid(msg: impl Into<String>) -> HopperError {
    HopperError::Invalid(msg.into())
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn arg_moment(args: &Value, key: &str) -> Result<Option<Moment>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => wire::parse_moment(v).map(Some),
    }
}

fn page_req(args: &Value) -> PageReq {
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 10_000) as u32;
    PageReq { offset, limit }
}

fn range(args: &Value) -> Result<MomentRange> {
    Ok(MomentRange {
        start: arg_moment(args, "from")?,
        end: arg_moment(args, "to")?,
    })
}

fn memory_reference(args: &Value) -> Result<u64> {
    let base = args
        .get("memoryReference")
        .ok_or_else(|| invalid("missing memoryReference"))
        .and_then(wire::parse_u64)?;
    let offset = arg_i64(args, "offset").unwrap_or(0);
    Ok(base.wrapping_add_signed(offset))
}

fn ownership_suffix(o: &OwnershipState) -> String {
    match o {
        OwnershipState::MovedOut { to: Some(to), .. } => format!(" (moved out to {to})"),
        OwnershipState::MovedOut { to: None, .. } => " (moved out)".into(),
        OwnershipState::Dropped { .. } => " (dropped)".into(),
        OwnershipState::Live | OwnershipState::Unknown => String::new(),
    }
}

fn source_json(loc: &SourceLoc) -> Value {
    let name = loc.file.rsplit('/').next().unwrap_or(&loc.file);
    json!({"name": name, "path": loc.file})
}

/// DAP's `stopped` reason and description for a stop cause.
fn stopped_reason(cause: &StopCause) -> (&'static str, Option<String>) {
    match cause {
        StopCause::Step => ("step", None),
        StopCause::Breakpoint { .. } => ("breakpoint", None),
        StopCause::Watchpoint { .. } => ("data breakpoint", None),
        StopCause::Signal { signo } => ("exception", Some(format!("signal {signo}"))),
        StopCause::Exited { code } => ("pause", Some(format!("the process exited with {code}"))),
        StopCause::HistoryStart => ("pause", Some("Reached the start of the recording".into())),
        StopCause::HistoryEnd => ("pause", Some("Reached the end of the recording".into())),
        StopCause::Jump => ("goto", None),
    }
}

impl<F, W> Session<F, W>
where
    F: FnMut(&LaunchTarget) -> Result<Arc<dyn Debugger>>,
    W: Write,
{
    fn new(factory: F, out: W) -> Self {
        Session {
            factory,
            out,
            seq: 0,
            debugger: None,
            frames: RefTable::new(),
            vars: RefTable::new(),
            gotos: RefTable::new(),
            source_breakpoints: HashMap::new(),
            function_breakpoints: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn send(&mut self, mut msg: Value) -> io::Result<()> {
        self.seq += 1;
        msg["seq"] = json!(self.seq);
        framing::write_message(&mut self.out, &msg)
    }

    fn event(&mut self, event: &str, body: Value) {
        self.pending.push((event.to_string(), body));
    }

    fn output(&mut self, text: impl Into<String>) {
        let mut text = text.into();
        text.push('\n');
        self.event("output", json!({"category": "console", "output": text}));
    }

    fn dbg(&self) -> Result<Arc<dyn Debugger>> {
        self.debugger
            .clone()
            .ok_or_else(|| invalid("no recording is open: send launch or attach first"))
    }

    fn handle(&mut self, req: &Value) -> io::Result<Flow> {
        let command = req
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let request_seq = req.get("seq").cloned().unwrap_or(json!(0));
        let args = req.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let (answer, flow) = match command.as_str() {
            "disconnect" => (Ok(Value::Null), Flow::Stop),
            _ => (self.dispatch(&command, &args), Flow::Continue),
        };
        let response = match answer {
            Ok(body) => json!({
                "type": "response", "request_seq": request_seq, "success": true,
                "command": command, "body": body,
            }),
            Err(e) => {
                // Events queued by a failed request describe nothing that happened.
                self.pending.clear();
                json!({
                    "type": "response", "request_seq": request_seq, "success": false,
                    "command": command, "message": e.to_string(),
                    "body": {"error": {"id": 1, "format": e.to_string(), "showUser": false}},
                })
            }
        };
        self.send(response)?;
        if flow == Flow::Stop {
            self.send(json!({"type": "event", "event": "terminated", "body": {}}))?;
        }
        for (event, body) in std::mem::take(&mut self.pending) {
            self.send(json!({"type": "event", "event": event, "body": body}))?;
        }
        Ok(flow)
    }

    fn dispatch(&mut self, command: &str, args: &Value) -> Answer {
        match command {
            "initialize" => Ok(capabilities()),
            "launch" | "attach" => self.launch(args),
            "setBreakpoints" => self.set_breakpoints(args),
            "setFunctionBreakpoints" => self.set_function_breakpoints(args),
            "setExceptionBreakpoints" => Ok(json!({"breakpoints": []})),
            "configurationDone" => {
                let stop = self.dbg()?.cursor()?;
                self.stopped(&stop, Some("entry"));
                Ok(Value::Null)
            }
            "threads" => self.threads(),
            "stackTrace" => self.stack_trace(args),
            "scopes" => self.scopes(args),
            "variables" => self.variables(args),
            "evaluate" => self.evaluate(args),
            "source" => self.source(args),
            "readMemory" => self.read_memory(args),
            "disassemble" => self.disassemble(args),
            "continue" => self.navigate(|d| d.run(Direction::Forward, Until::Breakpoints)),
            "reverseContinue" => self.navigate(|d| d.run(Direction::Reverse, Until::Breakpoints)),
            "next" => self.step(args, StepKind::Over, Direction::Forward),
            "stepIn" => self.step(args, StepKind::Into, Direction::Forward),
            "stepOut" => self.step(args, StepKind::Out, Direction::Forward),
            "stepBack" => self.step(args, StepKind::Over, Direction::Reverse),
            "gotoTargets" => self.goto_targets(args),
            "goto" => {
                let id = arg_i64(args, "targetId").ok_or_else(|| invalid("missing targetId"))?;
                let m = *self
                    .gotos
                    .get(id)
                    .ok_or_else(|| invalid(format!("no goto target {id}")))?;
                self.navigate(|d| d.seek(m))
            }
            // Replay is synchronous: the debuggee is always stopped when a request arrives.
            "pause" => Ok(Value::Null),
            "terminate" => {
                self.event("terminated", json!({}));
                Ok(Value::Null)
            }
            "hopper/cursor" => {
                let stop = self.dbg()?.cursor()?;
                Ok(json!({"stop": wire::to_json(&stop)}))
            }
            "hopper/seek" => {
                let m = arg_moment(args, "moment")?.ok_or_else(|| invalid("missing moment"))?;
                let stop = self.dbg()?.seek(m)?;
                self.stopped(&stop, None);
                Ok(json!({"stop": wire::to_json(&stop)}))
            }
            "hopper/calls" => self.calls(args),
            "hopper/lineHits" => self.line_hits(args),
            "hopper/origin" => self.origin(args),
            "hopper/valueHistory" => self.value_history(args),
            "hopper/lastWrite" => self.last_write(args),
            other => Err(HopperError::Unsupported(format!("request {other}"))),
        }
    }

    // ------------------------------------------------------------ session setup

    fn launch(&mut self, args: &Value) -> Answer {
        let target = LaunchTarget::from_arguments(args)?;
        let debugger = (self.factory)(&target)?;
        let info = debugger.recording()?;
        self.debugger = Some(debugger);
        self.output(format!(
            "hopper: opened {} (recording {})",
            info.path, info.id
        ));
        self.event("initialized", json!({}));
        Ok(Value::Null)
    }

    fn breakpoint_json(
        &self,
        result: Result<hopper_core::api::UserBreakpoint>,
        line: Option<i64>,
    ) -> Value {
        match result {
            Ok(bp) => {
                let mut v = json!({"id": bp.id.0, "verified": true});
                if let BreakLocation::Source(loc) = &bp.location {
                    v["line"] = json!(loc.line);
                    v["source"] = source_json(loc);
                }
                if let Some(a) = bp.addresses.first() {
                    v["instructionReference"] = json!(format!("{a:#x}"));
                }
                v
            }
            Err(e) => {
                let mut v = json!({"verified": false, "message": e.to_string()});
                if let Some(l) = line {
                    v["line"] = json!(l);
                }
                v
            }
        }
    }

    fn set_breakpoints(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let path = args
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("setBreakpoints needs source.path"))?
            .to_string();
        for id in self.source_breakpoints.remove(&path).unwrap_or_default() {
            d.remove_breakpoint(id)?;
        }
        let lines: Vec<i64> = match args.get("breakpoints").and_then(Value::as_array) {
            Some(bps) => bps.iter().filter_map(|b| arg_i64(b, "line")).collect(),
            None => args
                .get("lines")
                .and_then(Value::as_array)
                .map(|ls| ls.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
        };
        let mut ids = Vec::new();
        let mut out = Vec::new();
        for line in lines {
            let result = u32::try_from(line)
                .map_err(|_| invalid(format!("bad line {line}")))
                .and_then(|l| {
                    d.add_breakpoint(BreakLocation::Source(SourceLoc::new(path.clone(), l)))
                });
            if let Ok(bp) = &result {
                ids.push(bp.id);
            }
            out.push(self.breakpoint_json(result, Some(line)));
        }
        self.source_breakpoints.insert(path, ids);
        Ok(json!({"breakpoints": out}))
    }

    fn set_function_breakpoints(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        for id in std::mem::take(&mut self.function_breakpoints) {
            d.remove_breakpoint(id)?;
        }
        let names: Vec<String> = args
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|bps| {
                bps.iter()
                    .filter_map(|b| arg_str(b, "name"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        for name in names {
            let result = d.add_breakpoint(BreakLocation::Function(name));
            if let Ok(bp) = &result {
                self.function_breakpoints.push(bp.id);
            }
            out.push(self.breakpoint_json(result, None));
        }
        Ok(json!({"breakpoints": out}))
    }

    // ------------------------------------------------------------ state

    fn threads(&mut self) -> Answer {
        let info = self.dbg()?.recording()?;
        let threads: Vec<Value> = info
            .processes
            .iter()
            .flat_map(|p| {
                let exe = p
                    .executable
                    .rsplit('/')
                    .next()
                    .unwrap_or(&p.executable)
                    .to_string();
                p.threads
                    .iter()
                    .map(move |t| json!({"id": t.0, "name": format!("{exe} ({t})")}))
            })
            .collect();
        Ok(json!({"threads": threads}))
    }

    fn stack_trace(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let cursor = d.cursor()?;
        let tid = arg_i64(args, "threadId")
            .map(|t| Tid(t as i32))
            .unwrap_or(cursor.tid);
        let frames = d.frames(cursor.at, tid)?;
        let start = arg_i64(args, "startFrame").unwrap_or(0).max(0) as usize;
        let levels = match arg_i64(args, "levels").unwrap_or(0) {
            n if n > 0 => n as usize,
            _ => frames.len(),
        };
        let total = frames.len();
        let out: Vec<Value> = frames
            .into_iter()
            .skip(start)
            .take(levels)
            .map(|f| {
                let id = self.frames.id(FrameAt {
                    at: cursor.at,
                    frame: f.id,
                });
                let mut v = json!({
                    "id": id,
                    "name": f.function.clone().unwrap_or_else(|| format!("{:#x}", f.pc)),
                    "line": 0,
                    "column": 0,
                    "instructionPointerReference": format!("{:#x}", f.pc),
                    "hopperMoment": moment_json(&cursor.at),
                });
                if let Some(loc) = &f.location {
                    v["source"] = source_json(loc);
                    v["line"] = json!(loc.line);
                    v["column"] = json!(loc.column.unwrap_or(1));
                    if loc.file.starts_with("/rustc/") {
                        v["presentationHint"] = json!("subtle");
                    }
                }
                v
            })
            .collect();
        Ok(json!({"stackFrames": out, "totalFrames": total}))
    }

    fn frame_at(&self, args: &Value) -> Result<FrameAt> {
        let id = arg_i64(args, "frameId").ok_or_else(|| invalid("missing frameId"))?;
        self.frames
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("no frame {id}")))
    }

    fn scopes(&mut self, args: &Value) -> Answer {
        let f = self.frame_at(args)?;
        let locals = self.vars.id(VarRef::Locals(f.clone()));
        let regs = self.vars.id(VarRef::Registers {
            at: f.at,
            tid: f.frame.tid,
        });
        Ok(json!({"scopes": [
            {"name": "Locals", "presentationHint": "locals", "variablesReference": locals, "expensive": false},
            {"name": "Registers", "presentationHint": "registers", "variablesReference": regs, "expensive": false},
        ]}))
    }

    fn variable_json(&mut self, v: &ValueView, evaluate_name: Option<&str>) -> Value {
        let reference = match v.child_count {
            Some(n) if n > 0 => self.vars.id(VarRef::Value(v.handle.clone())),
            _ => 0,
        };
        let mut out = json!({
            "name": v.name,
            "value": format!("{}{}", v.summary, ownership_suffix(&v.ownership)),
            "type": v.type_name,
            "variablesReference": reference,
            "presentationHint": {"kind": "data", "attributes": ["readOnly"]},
            "hopperMoment": moment_json(&v.handle.at),
            "hopperOwnership": wire::to_json(&v.ownership),
        });
        if let Some(Place::Memory { addr, .. }) = v.place {
            out["memoryReference"] = json!(format!("{addr:#x}"));
        }
        if let Some(name) = evaluate_name {
            out["evaluateName"] = json!(name);
        }
        if let Some(n) = v.child_count
            && v.type_name.starts_with("alloc::vec::Vec")
        {
            out["indexedVariables"] = json!(n);
        }
        out
    }

    fn variables(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let id = arg_i64(args, "variablesReference")
            .ok_or_else(|| invalid("missing variablesReference"))?;
        let target = self
            .vars
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("no variables {id}")))?;
        let start = arg_i64(args, "start").unwrap_or(0).max(0) as u64;
        let count = match arg_i64(args, "count").unwrap_or(0) {
            n if n > 0 => n as u32,
            _ => 10_000,
        };
        let vars: Vec<Value> = match target {
            VarRef::Locals(f) => {
                let locals = d.locals(f.at, f.frame)?;
                locals
                    .iter()
                    .skip(start as usize)
                    .take(count as usize)
                    .map(|v| self.variable_json(v, Some(&v.name)))
                    .collect()
            }
            VarRef::Registers { at, tid } => d
                .registers(at, tid)?
                .values
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .map(|(reg, bytes)| {
                    let mut le = [0u8; 8];
                    let n = bytes.len().min(8);
                    le[..n].copy_from_slice(&bytes[..n]);
                    let value = if bytes.len() > 8 {
                        format!(
                            "0x{}",
                            bytes
                                .iter()
                                .rev()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        )
                    } else {
                        format!("{:#x}", u64::from_le_bytes(le))
                    };
                    json!({"name": reg.name(), "value": value, "variablesReference": 0,
                           "presentationHint": {"kind": "data", "attributes": ["readOnly"]}})
                })
                .collect(),
            VarRef::Value(h) => {
                let kids = d.children(
                    &h,
                    PageReq {
                        offset: start,
                        limit: count,
                    },
                )?;
                kids.iter().map(|v| self.variable_json(v, None)).collect()
            }
        };
        Ok(json!({"variables": vars}))
    }

    fn evaluate(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let expr = arg_str(args, "expression").ok_or_else(|| invalid("missing expression"))?;
        let f = match arg_i64(args, "frameId") {
            Some(_) => self.frame_at(args)?,
            None => {
                let c = d.cursor()?;
                FrameAt {
                    at: c.at,
                    frame: FrameId {
                        tid: c.tid,
                        depth: 0,
                    },
                }
            }
        };
        let v = d.eval(f.at, f.frame, expr)?;
        let var = self.variable_json(&v, Some(expr));
        let mut out = json!({
            "result": var["value"],
            "type": var["type"],
            "variablesReference": var["variablesReference"],
            "presentationHint": var["presentationHint"],
            "hopperMoment": var["hopperMoment"],
            "hopperOwnership": var["hopperOwnership"],
        });
        if let Some(m) = var.get("memoryReference") {
            out["memoryReference"] = m.clone();
        }
        Ok(out)
    }

    fn source(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let path = args
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("source needs source.path (hopper has no source references)"))?;
        let text = d.source_text(path)?;
        Ok(json!({"content": &*text, "mimeType": "text/x-rust"}))
    }

    fn first_process(&self, d: &dyn Debugger) -> Result<hopper_core::ProcessId> {
        d.recording()?
            .processes
            .first()
            .map(|p| p.id)
            .ok_or_else(|| HopperError::NotFound("the recording has no processes".into()))
    }

    fn read_memory(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let addr = memory_reference(args)?;
        let count = arg_i64(args, "count").unwrap_or(0).clamp(0, 1 << 20) as u64;
        let at = d.cursor()?.at;
        let process = self.first_process(d.as_ref())?;
        match d.memory(at, process, addr, count) {
            Ok(bytes) => Ok(json!({
                "address": format!("{addr:#x}"),
                "data": framing::base64(&bytes),
                "hopperMoment": moment_json(&at),
            })),
            Err(HopperError::UnreadableMemory { .. }) => Ok(json!({
                "address": format!("{addr:#x}"),
                "unreadableBytes": count,
                "hopperMoment": moment_json(&at),
            })),
            Err(e) => Err(e),
        }
    }

    fn disassemble(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let addr = memory_reference(args)?;
        let skip = arg_i64(args, "instructionOffset").unwrap_or(0);
        let count = arg_i64(args, "instructionCount")
            .unwrap_or(0)
            .clamp(0, 10_000) as usize;
        let at = d.cursor()?.at;
        let process = self.first_process(d.as_ref())?;
        // Instructions before `addr` cannot be found without decoding backwards; DAP lets the
        // adapter pad with invalid instructions instead.
        let pad = if skip < 0 { (-skip) as usize } else { 0 };
        let fetch = (count + skip.max(0) as usize).saturating_sub(pad.min(count));
        let ins = d
            .disassemble(at, process, addr, fetch as u32)
            .unwrap_or_default();
        let mut out: Vec<Value> = (0..pad.min(count))
            .map(|_| json!({"address": "0x0", "instruction": "<unknown>", "presentationHint": "invalid"}))
            .collect();
        for i in ins.iter().skip(skip.max(0) as usize) {
            if out.len() >= count {
                break;
            }
            let mut v = json!({
                "address": format!("{:#x}", i.addr),
                "instructionBytes": wire::hex(&i.bytes),
                "instruction": i.text,
            });
            if let Some(loc) = &i.location {
                v["location"] = source_json(loc);
                v["line"] = json!(loc.line);
            }
            out.push(v);
        }
        while out.len() < count {
            out.push(json!({"address": "0x0", "instruction": "<unknown>", "presentationHint": "invalid"}));
        }
        Ok(json!({"instructions": out}))
    }

    // ------------------------------------------------------------ navigation

    fn stopped(&mut self, stop: &Stop, reason: Option<&str>) {
        let (default_reason, description) = stopped_reason(&stop.cause);
        let mut body = json!({
            "reason": reason.unwrap_or(default_reason),
            "threadId": stop.tid.0,
            "allThreadsStopped": true,
            "hopperMoment": moment_json(&stop.at),
            "hopperCause": wire::to_json(&stop.cause),
        });
        if let StopCause::Breakpoint { id } = stop.cause {
            body["hitBreakpointIds"] = json!([id.0]);
        }
        if let Some(desc) = description {
            body["description"] = json!(desc.clone());
            if matches!(stop.cause, StopCause::HistoryStart | StopCause::HistoryEnd) {
                self.output(format!("hopper: {desc} ({})", stop.at.url()));
            }
        }
        self.event("stopped", body);
    }

    fn navigate(&mut self, go: impl FnOnce(&dyn Debugger) -> Result<Stop>) -> Answer {
        let d = self.dbg()?;
        let stop = go(d.as_ref())?;
        self.stopped(&stop, None);
        Ok(json!({"allThreadsContinued": true}))
    }

    fn step(&mut self, args: &Value, kind: StepKind, dir: Direction) -> Answer {
        let kind = match arg_str(args, "granularity") {
            Some("instruction") => StepKind::Instruction,
            _ => kind,
        };
        self.navigate(|d| d.step(kind, dir))?;
        Ok(Value::Null)
    }

    fn goto_targets(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let path = args
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("gotoTargets needs source.path"))?;
        let line = arg_i64(args, "line").ok_or_else(|| invalid("gotoTargets needs line"))?;
        let loc = SourceLoc::new(path, u32::try_from(line).map_err(|_| invalid("bad line"))?);
        let hits = d.line_hits(loc, MomentRange::all(), PageReq::first(1000))?;
        let total = hits.items.len();
        let targets: Vec<Value> = hits
            .items
            .iter()
            .enumerate()
            .map(|(i, m)| {
                json!({
                    "id": self.gotos.id(*m),
                    "label": format!("execution {} of {total} ({})", i + 1, m.url()),
                    "line": line,
                    "hopperMoment": moment_json(m),
                })
            })
            .collect();
        Ok(json!({"targets": targets}))
    }

    // ------------------------------------------------------------ hopper/* queries

    /// The value a `hopper/*` request is about: `variablesReference` (a value from `variables`
    /// or `evaluate`), or `expression` with an optional `frameId` (default: the cursor's
    /// innermost frame) or `moment`.
    fn target_value(&mut self, args: &Value) -> Result<ValueView> {
        let d = self.dbg()?;
        if let Some(id) = arg_i64(args, "variablesReference") {
            return match self.vars.get(id) {
                Some(VarRef::Value(h)) => d.view(h),
                Some(_) => Err(invalid(format!("variables {id} is a scope, not a value"))),
                None => Err(invalid(format!("no variables {id}"))),
            };
        }
        let expr = arg_str(args, "expression")
            .ok_or_else(|| invalid("give variablesReference or expression"))?;
        let f = match (arg_i64(args, "frameId"), arg_moment(args, "moment")?) {
            (Some(_), _) => self.frame_at(args)?,
            (None, Some(at)) => FrameAt {
                at,
                frame: FrameId {
                    tid: at.tid,
                    depth: arg_i64(args, "depth").unwrap_or(0).max(0) as u32,
                },
            },
            (None, None) => {
                let c = d.cursor()?;
                FrameAt {
                    at: c.at,
                    frame: FrameId {
                        tid: c.tid,
                        depth: 0,
                    },
                }
            }
        };
        d.eval(f.at, f.frame, expr)
    }

    fn calls(&mut self, args: &Value) -> Answer {
        let filter = CallFilter {
            parent: args.get("parent").and_then(Value::as_u64).map(CallId),
            function: arg_str(args, "function").map(str::to_string),
            tid: arg_i64(args, "threadId").map(|t| Tid(t as i32)),
            process: None,
        };
        let d = self.dbg()?;
        let page = d.calls(filter, page_req(args))?;
        let mut out = wire::to_json(&page);
        out["referenceRun"] = reference_run(d.as_ref(), page.items.iter().map(|c| &c.entry));
        Ok(out)
    }

    fn line_hits(&mut self, args: &Value) -> Answer {
        let loc = match arg_str(args, "location") {
            Some(s) => wire::parse_source_loc(s)?,
            None => {
                let path = args
                    .get("source")
                    .and_then(|s| s.get("path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("give location (file:line) or source.path and line"))?;
                let line = arg_i64(args, "line").ok_or_else(|| invalid("missing line"))?;
                SourceLoc::new(path, u32::try_from(line).map_err(|_| invalid("bad line"))?)
            }
        };
        let d = self.dbg()?;
        let page = d.line_hits(loc, range(args)?, page_req(args))?;
        let mut out = wire::to_json(&page);
        out["referenceRun"] = reference_run(d.as_ref(), page.items.iter());
        Ok(out)
    }

    fn origin(&mut self, args: &Value) -> Answer {
        let v = self.target_value(args)?;
        let budget = match args.get("maxHops").and_then(Value::as_u64) {
            Some(n) => Budget {
                max_hops: n.clamp(1, 1024) as u32,
                ..Budget::default()
            },
            None => Budget::default(),
        };
        let chain = self.dbg()?.origin(&v.handle, budget)?;
        Ok(json!({"value": wire::to_json(&v), "chain": wire::to_json(&chain)}))
    }

    fn value_history(&mut self, args: &Value) -> Answer {
        let v = self.target_value(args)?;
        let page = self
            .dbg()?
            .value_history(&v.handle, range(args)?, page_req(args))?;
        let items: Vec<Value> = page
            .items
            .iter()
            .map(|(m, view)| json!({"moment": moment_json(m), "value": wire::to_json(view)}))
            .collect();
        Ok(json!({
            "items": items,
            "total": page.total,
            "completeness": wire::to_json(&page.completeness),
        }))
    }

    fn last_write(&mut self, args: &Value) -> Answer {
        let d = self.dbg()?;
        let place = match args.get("address") {
            Some(a) => {
                let addr = wire::parse_u64(a)?;
                let len = args
                    .get("length")
                    .map(wire::parse_u64)
                    .transpose()?
                    .unwrap_or(8);
                Place::memory(self.first_process(d.as_ref())?, addr, len)
            }
            None => self
                .target_value(args)?
                .place
                .ok_or_else(|| HopperError::Eval("the value has no single place".into()))?,
        };
        let before = match arg_moment(args, "before")? {
            Some(m) => m,
            None => d.cursor()?.at,
        };
        let w = d.last_write(place, before)?;
        Ok(
            json!({"place": wire::to_json(&place), "before": moment_json(&before), "write": wire::to_json(&w)}),
        )
    }
}

fn capabilities() -> Value {
    let mut caps = Map::new();
    for (k, v) in [
        ("supportsConfigurationDoneRequest", true),
        ("supportsStepBack", true),
        ("supportsEvaluateForHovers", true),
        ("supportsReadMemoryRequest", true),
        ("supportsDisassembleRequest", true),
        ("supportsGotoTargetsRequest", true),
        ("supportsRestartFrame", false),
        ("supportsFunctionBreakpoints", true),
        ("supportsSteppingGranularity", true),
        ("supportsTerminateRequest", true),
        ("supportsSetVariable", false),
        ("supportsRestartRequest", false),
    ] {
        caps.insert(k.into(), json!(v));
    }
    caps.insert(
        "hopperProtocolVersion".into(),
        json!(HOPPER_PROTOCOL_VERSION),
    );
    caps.insert(
        "hopperRequests".into(),
        json!([
            "hopper/calls",
            "hopper/lineHits",
            "hopper/origin",
            "hopper/valueHistory",
            "hopper/lastWrite",
            "hopper/seek",
            "hopper/cursor"
        ]),
    );
    Value::Object(caps)
}


/// When the moments come from a live session's reference run (another run of the program the
/// session cannot seek to): which run. `null` otherwise.
fn reference_run<'a>(d: &dyn Debugger, mut moments: impl Iterator<Item = &'a Moment>) -> Value {
    let Ok(info) = d.recording() else {
        return Value::Null;
    };
    match &info.reference_run {
        Some(r) if moments.any(|m| info.is_reference_moment(m)) => {
            json!({"recording": r.id.to_string(), "description": r.description})
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_table_is_stable() {
        let mut t = RefTable::new();
        assert_eq!(t.id("a"), 1);
        assert_eq!(t.id("b"), 2);
        assert_eq!(t.id("a"), 1);
        assert_eq!(t.get(2), Some(&"b"));
        assert_eq!(t.get(0), None);
        assert_eq!(t.get(-1), None);
        assert_eq!(t.get(3), None);
    }

    #[test]
    fn launch_targets() {
        assert_eq!(
            LaunchTarget::from_arguments(&json!({"fake": true})).unwrap(),
            LaunchTarget::Fake
        );
        assert_eq!(
            LaunchTarget::from_arguments(&json!({"recording": "/t"})).unwrap(),
            LaunchTarget::Recording("/t".into())
        );
        assert!(LaunchTarget::from_arguments(&json!({})).is_err());
        assert_eq!(
            LaunchTarget::from_arguments(&json!({"program": "/bin/p", "args": ["x"]})).unwrap(),
            LaunchTarget::Live {
                program: "/bin/p".into(),
                args: vec!["x".into()]
            }
        );
        assert!(LaunchTarget::from_arguments(&json!({"program": "/bin/p", "args": [1]})).is_err());
    }
}
