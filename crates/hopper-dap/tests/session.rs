//! Drive full DAP sessions against the fake.

use std::io::{BufReader, Cursor, Read};
use std::sync::Arc;

use hopper_core::api::Debugger;
use hopper_dap::{LaunchTarget, serve};
use hopper_fake::FakeDebugger;
use serde_json::{Value, json};

fn frame(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn parse_all(bytes: &[u8]) -> Vec<Value> {
    let mut r = BufReader::new(Cursor::new(bytes.to_vec()));
    let mut out = Vec::new();
    loop {
        let mut header = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if std::io::BufRead::read_line(&mut r, &mut line).unwrap() == 0 {
                return out;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            header.push_str(line);
            len = line
                .strip_prefix("Content-Length: ")
                .unwrap()
                .parse()
                .unwrap();
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).unwrap();
        out.push(serde_json::from_slice(&body).unwrap());
    }
}

/// A scripted client: requests are numbered from 1.
struct Client {
    requests: Vec<Value>,
}

impl Client {
    fn new() -> Self {
        Client {
            requests: Vec::new(),
        }
    }

    fn req(&mut self, command: &str, args: Value) -> i64 {
        let seq = self.requests.len() as i64 + 1;
        self.requests
            .push(json!({"seq": seq, "type": "request", "command": command, "arguments": args}));
        seq
    }

    fn handshake(&mut self) {
        self.req(
            "initialize",
            json!({"adapterID": "hopper", "linesStartAt1": true}),
        );
        self.req("launch", json!({"fake": true}));
        self.req(
            "setBreakpoints",
            json!({"source": {"path": "main.rs"}, "breakpoints": [{"line": 16}, {"line": 1}]}),
        );
        self.req(
            "setFunctionBreakpoints",
            json!({"breakpoints": [{"name": "push"}]}),
        );
        self.req("configurationDone", json!({}));
    }

    fn run(self) -> Transcript {
        let input: Vec<u8> = self.requests.iter().flat_map(frame).collect();
        let mut out = Vec::new();
        let mut opened = Vec::new();
        serve(
            |t: &LaunchTarget| {
                opened.push(t.clone());
                match t {
                    LaunchTarget::Fake => Ok(Arc::new(FakeDebugger::new()) as Arc<dyn Debugger>),
                    LaunchTarget::Recording(_) | LaunchTarget::Live { .. } => {
                        Err(hopper_core::HopperError::Unsupported(
                            "this test opens only the fake".into(),
                        ))
                    }
                }
            },
            Cursor::new(input),
            &mut out,
        )
        .unwrap();
        Transcript {
            messages: parse_all(&out),
            opened,
        }
    }
}

struct Transcript {
    messages: Vec<Value>,
    opened: Vec<LaunchTarget>,
}

impl Transcript {
    fn response(&self, request_seq: i64) -> &Value {
        self.messages
            .iter()
            .find(|m| m["type"] == "response" && m["request_seq"] == request_seq)
            .unwrap_or_else(|| panic!("no response to {request_seq}"))
    }

    fn ok(&self, request_seq: i64) -> &Value {
        let r = self.response(request_seq);
        assert_eq!(r["success"], true, "{r}");
        &r["body"]
    }

    /// Events sent after the response to `request_seq` and before the next response.
    fn events_after(&self, request_seq: i64) -> Vec<&Value> {
        let i = self
            .messages
            .iter()
            .position(|m| m["type"] == "response" && m["request_seq"] == request_seq)
            .unwrap();
        self.messages[i + 1..]
            .iter()
            .take_while(|m| m["type"] == "event")
            .collect()
    }

    fn stopped_after(&self, request_seq: i64) -> &Value {
        let evs = self.events_after(request_seq);
        let e = evs
            .iter()
            .find(|e| e["event"] == "stopped")
            .unwrap_or_else(|| panic!("no stop after {request_seq}"));
        &e["body"]
    }
}

#[test]
fn handshake_advertises_capabilities_and_stops_at_entry() {
    let mut c = Client::new();
    c.handshake();
    let t = c.run();
    let caps = t.ok(1);
    for k in [
        "supportsStepBack",
        "supportsConfigurationDoneRequest",
        "supportsEvaluateForHovers",
        "supportsReadMemoryRequest",
        "supportsDisassembleRequest",
        "supportsGotoTargetsRequest",
    ] {
        assert_eq!(caps[k], true, "{k}");
    }
    assert_eq!(caps["supportsRestartFrame"], false);
    assert_eq!(
        caps["hopperProtocolVersion"],
        hopper_dap::HOPPER_PROTOCOL_VERSION
    );
    assert_eq!(t.opened, vec![LaunchTarget::Fake]);
    let evs: Vec<&str> = t
        .events_after(2)
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(evs, ["output", "initialized"]);
    let bps = &t.ok(3)["breakpoints"];
    assert_eq!(bps[0]["verified"], true);
    assert_eq!(bps[0]["line"], 16);
    assert_eq!(bps[1]["verified"], false);
    assert_eq!(t.ok(4)["breakpoints"][0]["verified"], true);
    let stop = t.stopped_after(5);
    assert_eq!(stop["reason"], "entry");
    assert_eq!(stop["threadId"], hopper_fake::PID);
    assert!(
        stop["hopperMoment"]
            .as_str()
            .unwrap()
            .starts_with("hopper://")
    );
    // Sequence numbers increase across responses and events.
    let seqs: Vec<i64> = t
        .messages
        .iter()
        .map(|m| m["seq"].as_i64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
}

#[test]
fn continue_hits_breakpoints_and_reverse_continue_goes_back() {
    let mut c = Client::new();
    c.handshake();
    let cont1 = c.req("continue", json!({"threadId": hopper_fake::PID}));
    let cont2 = c.req("continue", json!({"threadId": hopper_fake::PID}));
    let st = c.req("stackTrace", json!({"threadId": hopper_fake::PID}));
    let cont3 = c.req("continue", json!({"threadId": hopper_fake::PID}));
    let back = c.req("reverseContinue", json!({"threadId": hopper_fake::PID}));
    let t = c.run();
    let s1 = t.stopped_after(cont1);
    assert_eq!(s1["reason"], "breakpoint");
    assert_eq!(s1["hitBreakpointIds"], json!([1]));
    let s2 = t.stopped_after(cont2);
    assert_eq!(
        s2["hitBreakpointIds"],
        json!([2]),
        "the push function breakpoint"
    );
    let frames = &t.ok(st)["stackFrames"];
    assert_eq!(frames[0]["name"], "alloc::vec::Vec<T,A>::push");
    assert_eq!(frames[0]["presentationHint"], "subtle");
    assert_eq!(frames[1]["name"], "worked_example::main");
    assert_eq!(frames[1]["line"], 16);
    assert_eq!(t.ok(st)["totalFrames"], 2);
    let s3 = t.stopped_after(cont3);
    assert_eq!(s3["reason"], "pause");
    assert_eq!(s3["hopperCause"], "HistoryEnd");
    assert!(t.events_after(cont3).iter().any(|e| e["event"] == "output"));
    assert!(
        !t.messages.iter().any(|m| m["event"] == "terminated"),
        "end of history is not termination"
    );
    let sb = t.stopped_after(back);
    assert_eq!(sb["hitBreakpointIds"], json!([2]));
    assert_eq!(sb["hopperMoment"], s2["hopperMoment"]);
}

#[test]
fn steps_forward_and_back() {
    let mut c = Client::new();
    c.handshake();
    let tid = json!({"threadId": hopper_fake::PID});
    let n1 = c.req("next", tid.clone());
    let n2 = c.req("next", tid.clone());
    let into = c.req("stepIn", tid.clone());
    let st_in = c.req("stackTrace", tid.clone());
    let out = c.req("stepOut", tid.clone());
    let st = c.req("stackTrace", tid.clone());
    // From the middle of line 14 (after the clone returned) back to its start, then to line 13.
    let b0 = c.req("stepBack", tid.clone());
    let b1 = c.req("stepBack", tid.clone());
    let st2 = c.req("stackTrace", tid.clone());
    let i1 = c.req(
        "stepBack",
        json!({"threadId": hopper_fake::PID, "granularity": "instruction"}),
    );
    let t = c.run();
    for s in [n1, n2, into, out, b0, b1, i1] {
        assert_eq!(t.stopped_after(s)["reason"], "step");
    }
    let inner = &t.ok(st_in)["stackFrames"];
    assert_eq!(
        inner[0]["name"],
        "<worked_example::Config as core::clone::Clone>::clone"
    );
    assert_eq!(inner[0]["line"], 3);
    assert_eq!(t.ok(st)["stackFrames"][0]["line"], 14);
    assert_eq!(t.ok(st)["totalFrames"], 1);
    assert_eq!(t.ok(st2)["stackFrames"][0]["line"], 13);
    assert_ne!(
        t.stopped_after(b1)["hopperMoment"],
        t.stopped_after(i1)["hopperMoment"]
    );
}

#[test]
fn variables_references_survive_continue() {
    let mut c = Client::new();
    c.handshake();
    let cont = c.req("continue", json!({"threadId": hopper_fake::PID}));
    let st = c.req("stackTrace", json!({"threadId": hopper_fake::PID}));
    // Frame ids and variables references are allocated in order, from 1.
    let sc = c.req("scopes", json!({"frameId": 1}));
    let locals = c.req("variables", json!({"variablesReference": 1}));
    let cont2 = c.req("continue", json!({"threadId": hopper_fake::PID}));
    let cont3 = c.req("continue", json!({"threadId": hopper_fake::PID}));
    // Reference 3 is the first value with children (a), taken at the line-16 stop.
    let a_kids = c.req("variables", json!({"variablesReference": 3}));
    let regs = c.req("variables", json!({"variablesReference": 2}));
    let t = c.run();
    assert_eq!(t.stopped_after(cont)["reason"], "breakpoint");
    assert_eq!(t.ok(st)["stackFrames"][0]["id"], 1);
    let scopes = &t.ok(sc)["scopes"];
    assert_eq!(scopes[0]["name"], "Locals");
    assert_eq!(scopes[0]["variablesReference"], 1);
    let vars = t.ok(locals)["variables"].as_array().unwrap().clone();
    let names: Vec<&str> = vars.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["a", "b", "c", "v"]);
    assert_eq!(
        vars[0]["value"],
        "Config { name: \"alpha\", retries: 3 } (moved out to b)"
    );
    assert_eq!(vars[0]["hopperOwnership"]["MovedOut"]["to"], "b");
    assert_eq!(vars[0]["variablesReference"], 3);
    assert!(
        vars[0]["memoryReference"]
            .as_str()
            .unwrap()
            .starts_with("0x7ffc")
    );
    assert_eq!(vars[3]["value"], "Vec(len=0)");
    t.stopped_after(cont2);
    assert_eq!(t.stopped_after(cont3)["hopperCause"], "HistoryEnd");
    // After two continues the reference still names a at the first stop.
    let kids = t.ok(a_kids)["variables"].as_array().unwrap().clone();
    assert_eq!(kids[0]["name"], "name");
    assert_eq!(kids[0]["hopperMoment"], vars[0]["hopperMoment"]);
    assert_eq!(kids[1]["value"], "3 (moved out to b)");
    let regs = t.ok(regs)["variables"].as_array().unwrap().clone();
    assert!(regs.iter().any(|r| r["name"] == "rip"));
}

#[test]
fn evaluate_source_memory_and_disassembly() {
    let mut c = Client::new();
    c.handshake();
    c.req(
        "setBreakpoints",
        json!({"source": {"path": "main.rs"}, "breakpoints": [{"line": 18}]}),
    );
    c.req("setFunctionBreakpoints", json!({"breakpoints": []}));
    let cont = c.req("continue", json!({}));
    let ev = c.req("evaluate", json!({"expression": "n", "context": "hover"}));
    let ev_bad = c.req("evaluate", json!({"expression": "zzz", "context": "repl"}));
    let src = c.req("source", json!({"source": {"path": "main.rs"}}));
    let st = c.req("stackTrace", json!({"threadId": hopper_fake::PID}));
    let t0 = c.run();
    assert_eq!(t0.stopped_after(cont)["reason"], "breakpoint");
    let n = t0.ok(ev);
    assert_eq!(n["result"], "&\"alpha\"");
    assert_eq!(n["type"], "&alloc::string::String");
    assert!(n["variablesReference"].as_i64().unwrap() > 0);
    assert_eq!(t0.response(ev_bad)["success"], false);
    assert!(
        t0.ok(src)["content"]
            .as_str()
            .unwrap()
            .contains("v.push(c);")
    );
    let pc = t0.ok(st)["stackFrames"][0]["instructionPointerReference"]
        .as_str()
        .unwrap()
        .to_string();

    // Read the bytes n's place holds and disassemble at the pc, in a second session at line 18.
    let mut c = Client::new();
    c.handshake();
    c.req(
        "setBreakpoints",
        json!({"source": {"path": "main.rs"}, "breakpoints": [{"line": 18}]}),
    );
    c.req("setFunctionBreakpoints", json!({"breakpoints": []}));
    c.req("continue", json!({}));
    let mem = c.req(
        "readMemory",
        json!({"memoryReference": "0x555555559a20", "offset": 8, "count": 8}),
    );
    let bad = c.req("readMemory", json!({"memoryReference": "0x10", "count": 4}));
    let dis = c.req(
        "disassemble",
        json!({"memoryReference": pc, "instructionCount": 3}),
    );
    let dis_neg = c.req(
        "disassemble",
        json!({"memoryReference": pc, "instructionOffset": -2, "instructionCount": 4}),
    );
    let t = c.run();
    let data = t.ok(mem)["data"].as_str().unwrap();
    assert_eq!(data.len(), 12, "8 bytes of base64");
    assert_eq!(t.ok(mem)["address"], "0x555555559a28");
    assert_eq!(t.ok(bad)["unreadableBytes"], 4);
    let ins = t.ok(dis)["instructions"].as_array().unwrap().clone();
    assert_eq!(ins.len(), 3);
    assert_eq!(ins[0]["address"], pc);
    assert_eq!(ins[0]["instruction"], "lea rdi, [rbp-0x90]");
    assert_eq!(ins[0]["line"], 18);
    let ins = t.ok(dis_neg)["instructions"].as_array().unwrap().clone();
    assert_eq!(ins.len(), 4);
    assert_eq!(ins[0]["presentationHint"], "invalid");
    assert_eq!(ins[2]["address"], pc);
}

#[test]
fn goto_targets_are_line_hits() {
    let mut c = Client::new();
    c.handshake();
    let gt = c.req(
        "gotoTargets",
        json!({"source": {"path": "main.rs"}, "line": 14}),
    );
    let go = c.req("goto", json!({"threadId": hopper_fake::PID, "targetId": 1}));
    let st = c.req("stackTrace", json!({"threadId": hopper_fake::PID}));
    let t = c.run();
    let targets = t.ok(gt)["targets"].as_array().unwrap().clone();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["id"], 1);
    let stop = t.stopped_after(go);
    assert_eq!(stop["reason"], "goto");
    assert_eq!(stop["hopperMoment"], targets[0]["hopperMoment"]);
    assert_eq!(t.ok(st)["stackFrames"][0]["line"], 14);
}

#[test]
fn hopper_requests_answer_in_the_response_body() {
    let mut c = Client::new();
    c.handshake();
    let cur = c.req("hopper/cursor", json!({}));
    let hits = c.req("hopper/lineHits", json!({"location": "main.rs:18"}));
    let t0 = c.run();
    let at18 = t0.ok(hits)["items"][0].as_str().unwrap().to_string();
    let start = t0.ok(cur)["stop"]["at"].as_str().unwrap().to_string();
    assert!(
        t0.events_after(hits).is_empty(),
        "no side events for queries"
    );

    let mut c = Client::new();
    c.handshake();
    let seek = c.req("hopper/seek", json!({"moment": at18}));
    let cur = c.req("hopper/cursor", json!({}));
    let origin = c.req("hopper/origin", json!({"expression": "*n"}));
    let origin_at = c.req("hopper/origin", json!({"expression": "a", "moment": start}));
    let hist = c.req("hopper/valueHistory", json!({"expression": "a"}));
    let lw = c.req("hopper/lastWrite", json!({"expression": "b"}));
    let lw_addr = c.req(
        "hopper/lastWrite",
        json!({"address": "0x7ffc8a3e1e78", "length": 8}),
    );
    let calls = c.req("hopper/calls", json!({}));
    let kids = c.req("hopper/calls", json!({"parent": 0, "limit": 2}));
    let bad = c.req("hopper/seek", json!({"moment": "hopper://nope"}));
    let unknown = c.req("hopper/frobnicate", json!({}));
    let t = c.run();

    assert_eq!(t.ok(seek)["stop"]["at"], at18.as_str());
    assert_eq!(t.ok(seek)["stop"]["cause"], "Jump");
    assert_eq!(t.stopped_after(seek)["reason"], "goto");
    assert_eq!(t.ok(cur)["stop"]["at"], at18.as_str());

    let chain = &t.ok(origin)["chain"];
    let kinds: Vec<&str> = chain["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["ownership"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "SharedBorrow",
            "Move",
            "Clone",
            "Move",
            "CallReturn",
            "Literal"
        ]
    );
    assert_eq!(chain["end"], "Literal");
    assert_eq!(chain["hops"][1]["allocation"], 3);
    assert!(
        chain["hops"][0]["at"]
            .as_str()
            .unwrap()
            .starts_with("hopper://")
    );
    assert_eq!(
        chain["hops"][0]["written"]["Memory"]["addr"],
        "0x7ffc8a3e1e78"
    );
    assert_eq!(t.ok(origin)["value"]["summary"], "\"alpha\"");
    assert_eq!(
        t.ok(origin_at)["chain"]["end"]["Unknown"]["reason"],
        "`a` is not written yet"
    );

    let items = t.ok(hist)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 3);
    assert_eq!(items[2]["value"]["ownership"]["MovedOut"]["to"], "b");
    assert_eq!(t.ok(hist)["completeness"], "Complete");

    let w = &t.ok(lw)["write"];
    assert_eq!(w["location"]["line"], 13);
    assert_eq!(t.ok(lw_addr)["write"]["location"]["line"], 17);

    assert_eq!(t.ok(calls)["items"][0]["function"], "worked_example::main");
    assert_eq!(t.ok(kids)["items"].as_array().unwrap().len(), 2);
    assert_eq!(t.ok(kids)["total"], 4);

    assert_eq!(t.response(bad)["success"], false);
    assert!(
        t.response(unknown)["message"]
            .as_str()
            .unwrap()
            .contains("hopper/frobnicate")
    );
}

#[test]
fn requests_before_launch_fail_and_disconnect_terminates() {
    let mut c = Client::new();
    c.req("initialize", json!({}));
    let early = c.req("threads", json!({}));
    let bad_launch = c.req("launch", json!({"recording": "/no/such/trace"}));
    let empty = c.req("attach", json!({}));
    c.req("attach", json!({"fake": true}));
    let th = c.req("threads", json!({}));
    let disc = c.req("disconnect", json!({}));
    c.req("threads", json!({}));
    let t = c.run();
    assert_eq!(t.response(early)["success"], false);
    assert!(
        t.response(bad_launch)["message"]
            .as_str()
            .unwrap()
            .contains("this test opens only the fake")
    );
    assert_eq!(t.response(empty)["success"], false);
    assert_eq!(t.ok(th)["threads"][0]["id"], hopper_fake::PID);
    assert_eq!(t.events_after(disc)[0]["event"], "terminated");
    assert_eq!(
        t.messages.last().unwrap()["event"],
        "terminated",
        "nothing is served after disconnect"
    );
    assert_eq!(
        t.opened,
        vec![
            LaunchTarget::Recording("/no/such/trace".into()),
            LaunchTarget::Fake
        ]
    );
}
