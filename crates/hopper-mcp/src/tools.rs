//! The tools: their input schemas and their handlers over [`Debugger`].

use hopper_core::api::{
    BreakLocation, Budget, CallFilter, CallId, CallNode, Completeness, Debugger, Frame,
    OriginChain, OriginHop, OwnershipLabel, OwnershipState, PageReq, StepKind, Stop, StopCause,
    Until, ValueView,
};
use hopper_core::target::Direction;
use hopper_core::wire::{self, hex, moment_json, parse_break_location, parse_source_loc};
use hopper_core::{
    FrameId, HopperError, Moment, MomentRange, Place, ProcessId, Result, SourceLoc, Tid,
};
use serde_json::{Map, Value, json};

type Args = Map<String, Value>;

// ---------------------------------------------------------------- schemas

fn moment_prop() -> Value {
    json!({"type": "string", "description": "A position URL (hopper://...). Default: the cursor."})
}

fn frame_prop() -> Value {
    json!({"type": "integer", "minimum": 0, "description": "Frame depth, 0 = innermost. Default 0."})
}

fn thread_prop() -> Value {
    json!({"type": "integer", "description": "Thread id. Default: the thread running at the moment."})
}

fn int_prop(description: &str) -> Value {
    json!({"type": ["integer", "string"], "description": format!("{description} (a number, or a 0x hex string)")})
}

fn page_props(props: &mut Map<String, Value>) {
    props.insert("offset".into(), json!({"type": "integer", "minimum": 0, "description": "Skip this many results. Default 0."}));
    props.insert("limit".into(), json!({"type": "integer", "minimum": 1, "maximum": 1000, "description": "At most this many results. Default 100."}));
}

fn range_props(props: &mut Map<String, Value>) {
    props.insert(
        "from".into(),
        json!({"type": "string", "description": "Only moments at or after this position URL."}),
    );
    props.insert(
        "to".into(),
        json!({"type": "string", "description": "Only moments before this position URL."}),
    );
}

fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        },
    })
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Every tool, with its JSON Schema.
pub fn tool_definitions() -> Vec<Value> {
    let value_target = || {
        obj(json!({
            "expr": {"type": "string", "description": "A source expression: a variable, field, index, deref or borrow (`v[0].name`, `*n`)."},
            "moment": moment_prop(),
            "frame": frame_prop(),
            "thread": thread_prop(),
        }))
    };
    let mut history = value_target();
    range_props(&mut history);
    page_props(&mut history);
    let mut origin = value_target();
    origin.insert("max_hops".into(), json!({"type": "integer", "minimum": 1, "description": "Stop after this many hops. Default 32."}));
    let mut eval = value_target();
    eval.insert("expand".into(), json!({"type": "integer", "minimum": 0, "maximum": 4, "description": "Levels of children to include. Default 1."}));
    let mut calls = obj(json!({
        "function": {"type": "string", "description": "Only calls of functions whose name contains this text (searches the whole tree)."},
        "parent": {"type": "integer", "description": "Only children of this call id. Without `parent` or `function`: the root calls."},
        "thread": thread_prop(),
    }));
    page_props(&mut calls);
    let mut hits = obj(json!({
        "location": {"type": "string", "description": "file:line, matched by path suffix (`main.rs:16`)."},
    }));
    range_props(&mut hits);
    page_props(&mut hits);

    vec![
        tool(
            "recording_info",
            "Recording info",
            "The recording: id, path, first and last moments, processes, the cursor and the index's progress.",
            json!({}),
            &[],
        ),
        tool(
            "processes",
            "Processes",
            "The processes in the recording with their executables and threads.",
            json!({}),
            &[],
        ),
        tool(
            "seek",
            "Seek",
            "Move the cursor to a moment (a position URL). Returns where it stopped.",
            json!({"moment": {"type": "string", "description": "A position URL (hopper://...)."}}),
            &["moment"],
        ),
        tool(
            "step",
            "Step",
            "Step the cursor by source line or instruction, forward or backward in time.",
            json!({
                "kind": {"type": "string", "enum": ["into", "over", "out", "instruction"], "description": "into: next line, entering calls; over: next line in this function; out: until this function returns (reverse: until it was called); instruction: one machine instruction."},
                "direction": {"type": "string", "enum": ["forward", "reverse"], "description": "Default forward."},
                "count": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Repeat this many times. Default 1."},
            }),
            &["kind"],
        ),
        tool(
            "run_until",
            "Run until",
            "Run the cursor forward or backward until a condition: the next user breakpoint, a source location or function, the next write to memory, a moment, or the end of the recording.",
            json!({
                "until": {"type": "string", "enum": ["breakpoints", "location", "write", "moment", "end"]},
                "direction": {"type": "string", "enum": ["forward", "reverse"], "description": "Default forward."},
                "location": {"type": "string", "description": "For until=location: file:line, a function name, or a 0x address."},
                "address": int_prop("For until=write: the first byte to watch"),
                "length": int_prop("For until=write: how many bytes. Default 8"),
                "pid": {"type": "integer", "description": "For until=write: the process. Default: the first."},
                "moment": {"type": "string", "description": "For until=moment: a position URL."},
            }),
            &["until"],
        ),
        tool(
            "stack",
            "Stack",
            "The call stack of a thread at a moment, innermost frame first.",
            json!({"moment": moment_prop(), "thread": thread_prop()}),
            &[],
        ),
        tool(
            "eval",
            "Evaluate",
            "Evaluate an expression in a frame at a moment. Without `expr`, list the frame's locals. Values carry their Rust ownership state (live, moved out, dropped).",
            Value::Object(eval),
            &[],
        ),
        tool(
            "memory",
            "Read memory",
            "Read process memory at a moment, as hex.",
            json!({
                "address": int_prop("The first byte"),
                "length": {"type": "integer", "minimum": 1, "maximum": 65536},
                "moment": moment_prop(),
                "pid": {"type": "integer", "description": "The process. Default: the first."},
            }),
            &["address", "length"],
        ),
        tool(
            "calls",
            "Calls",
            "The call tree from the index: each call's function, entry and exit moments and child count.",
            Value::Object(calls),
            &[],
        ),
        tool(
            "line_hits",
            "Line hits",
            "Every moment a source line started executing, from the index, without moving the cursor.",
            Value::Object(hits),
            &["location"],
        ),
        tool(
            "value_history",
            "Value history",
            "Every moment the value of an expression changed, in the same frame.",
            Value::Object(history),
            &["expr"],
        ),
        tool(
            "last_write",
            "Last write",
            "The last write to a value's bytes (or an address range) at or before a moment: when, by which instruction, in which function and line.",
            json!({
                "expr": {"type": "string", "description": "A source expression whose bytes to look up."},
                "address": int_prop("Instead of expr: the first byte"),
                "length": int_prop("With address: how many bytes. Default 8"),
                "moment": moment_prop(),
                "frame": frame_prop(),
                "thread": thread_prop(),
                "pid": {"type": "integer", "description": "With address: the process. Default: the first."},
            }),
            &[],
        ),
        tool(
            "origin",
            "Origin",
            "Where a value came from: the chain of writes back to its source, each hop with its machine layer (copy, arithmetic, immediate, syscall) and its ownership layer (move, clone, borrow, call return, literal, with a confidence grade) and heap allocation.",
            Value::Object(origin),
            &["expr"],
        ),
    ]
}

// ---------------------------------------------------------------- argument helpers

fn invalid(msg: impl Into<String>) -> HopperError {
    HopperError::Invalid(msg.into())
}

fn opt_str<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(invalid(format!("`{key}` must be a string, got {other}"))),
    }
}

fn req_str<'a>(args: &'a Args, key: &str) -> Result<&'a str> {
    opt_str(args, key)?.ok_or_else(|| invalid(format!("missing `{key}`")))
}

fn opt_u64(args: &Args, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => wire::parse_u64(v)
            .map(Some)
            .map_err(|e| invalid(format!("`{key}`: {e}"))),
    }
}

fn req_u64(args: &Args, key: &str) -> Result<u64> {
    opt_u64(args, key)?.ok_or_else(|| invalid(format!("missing `{key}`")))
}

fn opt_moment(args: &Args, key: &str) -> Result<Option<Moment>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => wire::parse_moment(v)
            .map(Some)
            .map_err(|e| invalid(format!("`{key}`: {e}"))),
    }
}

fn direction(args: &Args) -> Result<Direction> {
    match opt_str(args, "direction")? {
        None | Some("forward") => Ok(Direction::Forward),
        Some("reverse") => Ok(Direction::Reverse),
        Some(other) => Err(invalid(format!(
            "direction must be forward or reverse, not {other:?}"
        ))),
    }
}

fn page_req(args: &Args) -> Result<PageReq> {
    let limit = opt_u64(args, "limit")?.unwrap_or(100).clamp(1, 1000) as u32;
    Ok(PageReq {
        offset: opt_u64(args, "offset")?.unwrap_or(0),
        limit,
    })
}

fn range(args: &Args) -> Result<MomentRange> {
    Ok(MomentRange {
        start: opt_moment(args, "from")?,
        end: opt_moment(args, "to")?,
    })
}

/// Rejects arguments the tool does not take, so typos are not silently ignored.
fn check_keys(name: &str, args: &Args) -> Result<()> {
    let defs = tool_definitions();
    let def = defs
        .iter()
        .find(|d| d["name"] == name)
        .expect("a defined tool");
    let props = def["inputSchema"]["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for key in args.keys() {
        if !props.contains_key(key) {
            let known: Vec<&String> = props.keys().collect();
            return Err(invalid(format!(
                "unknown argument `{key}` for {name}; it takes {known:?}"
            )));
        }
    }
    let required = def["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for r in required {
        let r = r.as_str().unwrap_or_default();
        if !args.contains_key(r) {
            return Err(invalid(format!("missing `{r}` for {name}")));
        }
    }
    Ok(())
}

/// The moment a reading tool looks at, and the thread there.
struct Here {
    at: Moment,
    tid: Tid,
}

fn here(d: &dyn Debugger, args: &Args) -> Result<Here> {
    let at = match opt_moment(args, "moment")? {
        Some(m) => m,
        None => d.cursor()?.at,
    };
    let tid = match opt_u64(args, "thread")? {
        Some(t) => Tid(i32::try_from(t).map_err(|_| invalid("thread id out of range"))?),
        None => at.tid,
    };
    Ok(Here { at, tid })
}

fn frame_id(args: &Args, here: &Here) -> Result<FrameId> {
    let depth = opt_u64(args, "frame")?.unwrap_or(0);
    Ok(FrameId {
        tid: here.tid,
        depth: u32::try_from(depth).map_err(|_| invalid("frame out of range"))?,
    })
}

fn process(d: &dyn Debugger, args: &Args) -> Result<ProcessId> {
    let info = d.recording()?;
    match opt_u64(args, "pid")? {
        None => info
            .processes
            .first()
            .map(|p| p.id)
            .ok_or_else(|| HopperError::NotFound("the recording has no processes".into())),
        Some(pid) => info
            .processes
            .iter()
            .find(|p| p.id.pid as u64 == pid)
            .map(|p| p.id)
            .ok_or_else(|| HopperError::NotFound(format!("process {pid}"))),
    }
}

/// Evaluate `expr` at the arguments' moment and frame.
fn value(d: &dyn Debugger, args: &Args) -> Result<ValueView> {
    let h = here(d, args)?;
    d.eval(h.at, frame_id(args, &h)?, req_str(args, "expr")?)
}

// ---------------------------------------------------------------- JSON shapes

fn loc_json(loc: &Option<SourceLoc>) -> Value {
    loc.as_ref()
        .map_or(Value::Null, |l| Value::String(l.to_string()))
}

fn stop_json(s: &Stop) -> Value {
    json!({
        "moment": moment_json(&s.at),
        "thread": s.tid.0,
        "pc": format!("{:#x}", s.pc),
        "cause": wire::to_json(&s.cause),
        "function": s.function,
        "location": loc_json(&s.location),
    })
}

fn place_json(p: &Place) -> Value {
    match p {
        Place::Memory { process, addr, len } => json!({
            "kind": "memory", "pid": process.pid, "address": format!("{addr:#x}"), "length": len,
        }),
        Place::Register { frame, reg } => json!({
            "kind": "register", "thread": frame.tid.0, "frame": frame.depth, "register": reg.name(),
        }),
    }
}

fn ownership_state_json(s: &OwnershipState) -> Value {
    match s {
        OwnershipState::Live => json!({"state": "live"}),
        OwnershipState::MovedOut { to, at, moment } => json!({
            "state": "moved_out",
            "to": to,
            "at": loc_json(at),
            "moment": moment.as_ref().map(moment_json),
        }),
        OwnershipState::Dropped { at, moment } => json!({
            "state": "dropped",
            "at": loc_json(at),
            "moment": moment.as_ref().map(moment_json),
        }),
        OwnershipState::Unknown => json!({"state": "unknown"}),
    }
}

fn value_json(d: &dyn Debugger, v: &ValueView, expand: u32) -> Result<Value> {
    let mut out = json!({
        "name": v.name,
        "type": v.type_name,
        "summary": v.summary,
        "ownership": ownership_state_json(&v.ownership),
        "place": v.place.as_ref().map(place_json),
        "child_count": v.child_count,
    });
    if expand > 0 && v.child_count.unwrap_or(0) > 0 {
        let kids = d.children(&v.handle, PageReq::first(100))?;
        let kids: Result<Vec<Value>> = kids.iter().map(|k| value_json(d, k, expand - 1)).collect();
        out["children"] = Value::Array(kids?);
    }
    Ok(out)
}

fn label_json(l: &OwnershipLabel) -> Value {
    json!({
        "kind": wire::to_json(&l.kind),
        "src": l.src,
        "dst": l.dst,
        "span": loc_json(&l.span),
        "confidence": wire::to_json(&l.confidence),
    })
}

fn hop_json(h: &OriginHop) -> Value {
    json!({
        "moment": moment_json(&h.at),
        "pc": format!("{:#x}", h.pc),
        "function": h.function,
        "location": loc_json(&h.location),
        "read": h.read.as_ref().map(place_json),
        "written": place_json(&h.written),
        "machine": wire::to_json(&h.machine),
        "ownership": h.ownership.as_ref().map(label_json),
        "inner": h.inner.iter().map(label_json).collect::<Vec<_>>(),
        "operands": h.operands.iter().map(place_json).collect::<Vec<_>>(),
        "allocation": h.allocation.map(|a| a.0),
    })
}

fn chain_json(c: &OriginChain) -> Value {
    json!({
        "references": c.references.iter().map(hop_json).collect::<Vec<_>>(),
        "hops": c.hops.iter().map(hop_json).collect::<Vec<_>>(),
        "end": wire::to_json(&c.end),
    })
}

fn completeness_json(c: &Completeness) -> Value {
    match c {
        Completeness::Complete => json!({"complete": true}),
        Completeness::Partial { pending } => json!({
            "complete": false,
            "pending": pending.iter().map(|r| json!({
                "from": r.start.as_ref().map(moment_json),
                "to": r.end.as_ref().map(moment_json),
            })).collect::<Vec<_>>(),
        }),
    }
}

fn call_json(c: &CallNode) -> Value {
    json!({
        "id": c.id.0,
        "parent": c.parent.map(|p| p.0),
        "thread": c.tid.0,
        "function": c.function,
        "entry": moment_json(&c.entry),
        "exit": c.exit.as_ref().map(moment_json),
        "depth": c.depth,
        "child_count": c.child_count,
        "location": loc_json(&c.location),
    })
}

fn frame_json(f: &Frame) -> Value {
    json!({
        "depth": f.id.depth,
        "function": f.function,
        "location": loc_json(&f.location),
        "pc": format!("{:#x}", f.pc),
        "cfa": f.cfa.map(|c| format!("{c:#x}")),
        "inlined": f.inlined,
    })
}

// ---------------------------------------------------------------- handlers

/// Run one tool; `None` when no tool has this name.
pub(crate) fn call(d: &dyn Debugger, name: &str, args: &Args) -> Option<Result<Value>> {
    let handler: fn(&dyn Debugger, &Args) -> Result<Value> = match name {
        "recording_info" => recording_info,
        "processes" => processes,
        "seek" => seek,
        "step" => step,
        "run_until" => run_until,
        "stack" => stack,
        "eval" => eval,
        "memory" => memory,
        "calls" => calls,
        "line_hits" => line_hits,
        "value_history" => value_history,
        "last_write" => last_write,
        "origin" => origin,
        _ => return None,
    };
    Some(check_keys(name, args).and_then(|()| handler(d, args)))
}

fn recording_info(d: &dyn Debugger, _: &Args) -> Result<Value> {
    let info = d.recording()?;
    let status = d.index_status();
    let cursor = d.cursor()?;
    Ok(json!({
        "recording": info.id.to_hex(),
        "path": info.path,
        "rr_version": info.rr_version,
        "start": info.start.as_ref().map(moment_json),
        "end": info.end.as_ref().map(moment_json),
        "processes": info.processes.iter().map(|p| json!({
            "pid": p.id.pid, "exec_generation": p.id.exec_generation,
            "executable": p.executable, "threads": p.threads.iter().map(|t| t.0).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "cursor": stop_json(&cursor),
        "index": {
            "done": status.done,
            "error": status.error,
            "phases": status.phases.iter().map(|(n, p)| json!({"phase": n, "progress": p})).collect::<Vec<_>>(),
        },
    }))
}

fn processes(d: &dyn Debugger, _: &Args) -> Result<Value> {
    let info = d.recording()?;
    Ok(json!({
        "processes": info.processes.iter().map(|p| json!({
            "pid": p.id.pid,
            "exec_generation": p.id.exec_generation,
            "executable": p.executable,
            "threads": p.threads.iter().map(|t| t.0).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }))
}

fn seek(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let m = opt_moment(args, "moment")?.ok_or_else(|| invalid("missing `moment`"))?;
    Ok(stop_json(&d.seek(m)?))
}

fn step(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let kind = match req_str(args, "kind")? {
        "into" => StepKind::Into,
        "over" => StepKind::Over,
        "out" => StepKind::Out,
        "instruction" => StepKind::Instruction,
        other => {
            return Err(invalid(format!(
                "kind must be into, over, out or instruction, not {other:?}"
            )));
        }
    };
    let dir = direction(args)?;
    let count = opt_u64(args, "count")?.unwrap_or(1).clamp(1, 1000);
    let mut stop = d.step(kind, dir)?;
    for _ in 1..count {
        if matches!(stop.cause, StopCause::HistoryStart | StopCause::HistoryEnd) {
            break;
        }
        stop = d.step(kind, dir)?;
    }
    Ok(stop_json(&stop))
}

fn run_until(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let dir = direction(args)?;
    let stop = match req_str(args, "until")? {
        "breakpoints" => d.run(dir, Until::Breakpoints)?,
        "end" => d.run(dir, Until::EndOfTrace)?,
        "moment" => {
            let m = opt_moment(args, "moment")?
                .ok_or_else(|| invalid("until=moment needs `moment`"))?;
            d.run(dir, Until::Moment(m))?
        }
        "write" => {
            let addr = req_u64(args, "address")?;
            let len = opt_u64(args, "length")?.unwrap_or(8);
            d.run(
                dir,
                Until::Watch(Place::memory(process(d, args)?, addr, len)),
            )?
        }
        "location" => {
            let loc: BreakLocation = parse_break_location(req_str(args, "location")?)?;
            // A temporary breakpoint that is removed whatever the run does.
            let bp = d.add_breakpoint(loc)?;
            let others: Vec<_> = d
                .breakpoints()?
                .into_iter()
                .filter(|b| b.id != bp.id)
                .collect();
            for b in &others {
                d.remove_breakpoint(b.id)?;
            }
            let result = d.run(dir, Until::Breakpoints);
            d.remove_breakpoint(bp.id)?;
            for b in others {
                d.add_breakpoint(b.location)?;
            }
            result?
        }
        other => {
            return Err(invalid(format!(
                "until must be breakpoints, location, write, moment or end, not {other:?}"
            )));
        }
    };
    Ok(stop_json(&stop))
}

fn stack(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let h = here(d, args)?;
    let frames = d.frames(h.at, h.tid)?;
    Ok(json!({
        "moment": moment_json(&h.at),
        "thread": h.tid.0,
        "frames": frames.iter().map(frame_json).collect::<Vec<_>>(),
    }))
}

fn eval(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let h = here(d, args)?;
    let frame = frame_id(args, &h)?;
    let expand = opt_u64(args, "expand")?.unwrap_or(1).min(4) as u32;
    match opt_str(args, "expr")? {
        Some(expr) => {
            let v = d.eval(h.at, frame, expr)?;
            Ok(json!({
                "moment": moment_json(&h.at),
                "frame": frame.depth,
                "value": value_json(d, &v, expand)?,
            }))
        }
        None => {
            let locals = d.locals(h.at, frame)?;
            let locals: Result<Vec<Value>> =
                locals.iter().map(|v| value_json(d, v, expand)).collect();
            Ok(json!({"moment": moment_json(&h.at), "frame": frame.depth, "locals": locals?}))
        }
    }
}

fn memory(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let h = here(d, args)?;
    let addr = req_u64(args, "address")?;
    let len = req_u64(args, "length")?.clamp(1, 65536);
    let bytes = d.memory(h.at, process(d, args)?, addr, len)?;
    let ascii: String = bytes
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            }
        })
        .collect();
    Ok(json!({
        "moment": moment_json(&h.at),
        "address": format!("{addr:#x}"),
        "length": bytes.len(),
        "hex": hex(&bytes),
        "ascii": ascii,
    }))
}

fn calls(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let filter = CallFilter {
        parent: opt_u64(args, "parent")?.map(CallId),
        function: opt_str(args, "function")?.map(str::to_string),
        tid: opt_u64(args, "thread")?.map(|t| Tid(t as i32)),
        process: None,
    };
    let page = d.calls(filter, page_req(args)?)?;
    Ok(json!({
        "calls": page.items.iter().map(call_json).collect::<Vec<_>>(),
        "total": page.total,
        "completeness": completeness_json(&page.completeness),
        "reference_run": reference_run_json(d, page.items.iter().map(|c| &c.entry)),
    }))
}

/// When the moments come from a live session's reference run (another run of the program,
/// which the session cannot seek to): which run, so an agent does not pass them to `seek`.
fn reference_run_json<'a>(d: &dyn Debugger, moments: impl Iterator<Item = &'a Moment>) -> Value {
    let Ok(info) = d.recording() else {
        return Value::Null;
    };
    let mut moments = moments;
    match &info.reference_run {
        Some(r) if moments.any(|m| info.is_reference_moment(m)) => json!({
            "recording": r.id.to_string(),
            "description": r.description,
            "note": "these moments belong to the reference run and cannot be sought in this session",
        }),
        _ => Value::Null,
    }
}

fn line_hits(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let loc = parse_source_loc(req_str(args, "location")?)?;
    let page = d.line_hits(loc.clone(), range(args)?, page_req(args)?)?;
    Ok(json!({
        "location": loc.to_string(),
        "moments": page.items.iter().map(moment_json).collect::<Vec<_>>(),
        "total": page.total,
        "completeness": completeness_json(&page.completeness),
        "reference_run": reference_run_json(d, page.items.iter()),
    }))
}

fn value_history(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let v = value(d, args)?;
    let page = d.value_history(&v.handle, range(args)?, page_req(args)?)?;
    Ok(json!({
        "expr": v.name,
        "entries": page.items.iter().map(|(m, v)| json!({
            "moment": moment_json(m),
            "summary": v.summary,
            "type": v.type_name,
            "ownership": ownership_state_json(&v.ownership),
        })).collect::<Vec<_>>(),
        "total": page.total,
        "completeness": completeness_json(&page.completeness),
    }))
}

fn last_write(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let h = here(d, args)?;
    let place = match (opt_str(args, "expr")?, opt_u64(args, "address")?) {
        (Some(_), Some(_)) => return Err(invalid("give `expr` or `address`, not both")),
        (Some(_), None) => value(d, args)?
            .place
            .ok_or_else(|| HopperError::Eval("the value has no single place".into()))?,
        (None, Some(addr)) => Place::memory(
            process(d, args)?,
            addr,
            opt_u64(args, "length")?.unwrap_or(8),
        ),
        (None, None) => return Err(invalid("give `expr` or `address`")),
    };
    let w = d.last_write(place, h.at)?;
    Ok(json!({
        "moment": moment_json(&h.at),
        "place": place_json(&place),
        "write": w.map(|w| json!({
            "moment": moment_json(&w.at),
            "pc": format!("{:#x}", w.pc),
            "place": place_json(&w.place),
            "bytes": hex(&w.value),
            "function": w.function,
            "location": loc_json(&w.location),
        })),
    }))
}

fn origin(d: &dyn Debugger, args: &Args) -> Result<Value> {
    let v = value(d, args)?;
    let budget = match opt_u64(args, "max_hops")? {
        Some(n) => Budget {
            max_hops: n.clamp(1, 1024) as u32,
            ..Budget::default()
        },
        None => Budget::default(),
    };
    let chain = d.origin(&v.handle, budget)?;
    let mut out = chain_json(&chain);
    out["moment"] = moment_json(&v.handle.at);
    out["expr"] = Value::String(v.name.clone());
    out["value"] = Value::String(v.summary.clone());
    Ok(out)
}
