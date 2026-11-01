//! Drive the MCP server over JSON lines against the fake, as an agent would.

use std::io::Cursor;
use std::sync::Arc;

use hopper_fake::FakeDebugger;
use serde_json::{Value, json};

/// Run a whole session and return the replies, in order.
fn session(requests: &[Value]) -> Vec<Value> {
    let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
    let mut out = Vec::new();
    hopper_mcp::serve(Arc::new(FakeDebugger::new()), Cursor::new(input), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn init() -> Vec<Value> {
    vec![
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    ]
}

fn call(id: u64, name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}})
}

/// The structured result of a successful tool call.
fn ok(reply: &Value) -> &Value {
    assert_eq!(reply["result"]["isError"], false, "tool failed: {reply}");
    let structured = &reply["result"]["structuredContent"];
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), structured);
    structured
}

/// Run `calls` after the handshake; replies come back with the handshake's removed.
fn tools(calls: Vec<Value>) -> Vec<Value> {
    let mut reqs = init();
    reqs.extend(calls);
    let mut replies = session(&reqs);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    replies.remove(0);
    replies
}

#[test]
fn lists_thirteen_tools_with_schemas() {
    let r = tools(vec![json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})]);
    let list = r[0]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = list.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "recording_info",
            "processes",
            "seek",
            "step",
            "run_until",
            "stack",
            "eval",
            "memory",
            "calls",
            "line_hits",
            "value_history",
            "last_write",
            "origin"
        ]
    );
    for t in list {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        assert!(t["inputSchema"]["properties"].is_object());
        assert!(t["description"].as_str().unwrap().len() > 20);
    }
}

#[test]
fn recording_info_and_processes() {
    let r = tools(vec![
        call(1, "recording_info", json!({})),
        call(2, "processes", json!({})),
    ]);
    let info = ok(&r[0]);
    assert!(info["start"].as_str().unwrap().starts_with("hopper://"));
    assert_eq!(info["cursor"]["moment"], info["start"]);
    assert_eq!(info["index"]["done"], true);
    let procs = ok(&r[1]);
    assert_eq!(procs["processes"][0]["pid"], hopper_fake::PID);
}

#[test]
fn stepping_and_stack() {
    let r = tools(vec![
        call(1, "step", json!({"kind": "over", "count": 4})),
        call(2, "stack", json!({})),
        call(3, "step", json!({"kind": "into"})),
        call(4, "stack", json!({})),
        call(5, "step", json!({"kind": "over", "direction": "reverse"})),
    ]);
    let s = ok(&r[0]);
    assert!(s["location"].as_str().unwrap().ends_with("main.rs:16"));
    assert_eq!(s["cause"], "Step");
    assert_eq!(ok(&r[1])["frames"].as_array().unwrap().len(), 1);
    assert_eq!(ok(&r[2])["function"], "alloc::vec::Vec<T,A>::push");
    let frames = ok(&r[3])["frames"].as_array().unwrap().clone();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1]["function"], "worked_example::main");
    // Reverse step-over from the first line of push leaves it for the caller's line.
    assert!(
        ok(&r[4])["location"]
            .as_str()
            .unwrap()
            .ends_with("main.rs:16")
    );
}

#[test]
fn eval_shows_ownership_and_children() {
    let r = tools(vec![
        call(
            1,
            "run_until",
            json!({"until": "location", "location": "main.rs:18"}),
        ),
        call(2, "eval", json!({"expr": "a"})),
        call(3, "eval", json!({"expr": "v", "expand": 2})),
        call(4, "eval", json!({})),
    ]);
    let stop = ok(&r[0]);
    assert_eq!(stop["cause"], json!({"Breakpoint": {"id": 1}}));
    let a = ok(&r[1]);
    assert_eq!(a["moment"], stop["moment"]);
    assert_eq!(a["value"]["ownership"]["state"], "moved_out");
    assert_eq!(a["value"]["ownership"]["to"], "b");
    let v = &ok(&r[2])["value"];
    assert_eq!(v["children"][0]["children"][0]["name"], "name");
    assert_eq!(v["children"][0]["children"][0]["summary"], "\"alpha\"");
    let locals = ok(&r[3])["locals"].as_array().unwrap().clone();
    assert_eq!(locals.len(), 5);
}

#[test]
fn agent_reproduces_the_origin_chain_using_only_moments() {
    // Step 1: find when line 18 runs, without moving the cursor.
    let r = tools(vec![
        call(1, "line_hits", json!({"location": "main.rs:18"})),
        call(2, "recording_info", json!({})),
    ]);
    let hits = ok(&r[0]);
    let at18 = hits["moments"][0].as_str().unwrap().to_string();
    assert_eq!(hits["completeness"]["complete"], true);
    assert_ne!(
        ok(&r[1])["cursor"]["moment"],
        at18,
        "line_hits must not move the cursor"
    );

    // Step 2: ask where `*n` came from at that moment, then inspect each hop by its moment.
    let r = tools(vec![call(
        1,
        "origin",
        json!({"expr": "*n", "moment": at18}),
    )]);
    let chain = ok(&r[0]).clone();
    assert_eq!(chain["value"], "\"alpha\"");
    let hops = chain["hops"].as_array().unwrap();
    let kinds: Vec<&str> = hops
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
    assert!(hops.iter().all(|h| h["ownership"]["confidence"] == "Exact"));
    assert_eq!(hops[1]["allocation"], 3);
    assert_eq!(hops[5]["machine"], "Copy");

    // The literal's bytes at the last hop's moment are "alpha".
    let lit = &hops[5];
    let r = tools(vec![
        call(
            1,
            "memory",
            json!({
                "address": lit["read"]["address"], "length": lit["read"]["length"], "moment": lit["moment"],
            }),
        ),
        call(2, "seek", json!({"moment": hops[3]["moment"]})),
        call(3, "eval", json!({"expr": "a"})),
    ]);
    assert_eq!(ok(&r[0])["ascii"], "alpha");
    let at_move = ok(&r[1]);
    assert_eq!(at_move["moment"], hops[3]["moment"]);
    assert!(
        at_move["location"]
            .as_str()
            .unwrap()
            .ends_with("main.rs:13")
    );
    assert_eq!(ok(&r[2])["value"]["ownership"]["state"], "moved_out");
}

#[test]
fn history_and_last_write() {
    let r = tools(vec![
        call(1, "run_until", json!({"until": "end"})),
        call(2, "value_history", json!({"expr": "b"})),
        call(3, "last_write", json!({"expr": "b"})),
        call(
            4,
            "last_write",
            json!({"address": "0x7ffc8a3e1e78", "length": 8}),
        ),
    ]);
    let end = ok(&r[0]);
    assert_eq!(end["cause"], "HistoryEnd");
    let entries = ok(&r[1])["entries"].as_array().unwrap().clone();
    let states: Vec<&str> = entries
        .iter()
        .map(|e| e["ownership"]["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["live", "live", "dropped"]);
    assert_eq!(entries[0]["summary"], "<uninitialised>");
    let w = &ok(&r[2])["write"];
    assert!(w["location"].as_str().unwrap().ends_with("main.rs:13"));
    let n = &ok(&r[3])["write"];
    assert!(n["location"].as_str().unwrap().ends_with("main.rs:17"));
    assert_eq!(n["bytes"].as_str().unwrap().len(), 16);
}

#[test]
fn calls_tree() {
    let r = tools(vec![
        call(1, "calls", json!({})),
        call(2, "calls", json!({"parent": 0})),
        call(3, "calls", json!({"function": "push"})),
    ]);
    assert_eq!(ok(&r[0])["calls"][0]["function"], "worked_example::main");
    assert_eq!(ok(&r[1])["calls"].as_array().unwrap().len(), 4);
    let push = &ok(&r[2])["calls"][0];
    assert!(push["entry"].as_str().unwrap().starts_with("hopper://"));
    assert!(push["exit"].as_str().unwrap().starts_with("hopper://"));
}

#[test]
fn errors_are_tool_results() {
    let r = tools(vec![
        call(1, "eval", json!({"expr": "nope"})),
        call(2, "seek", json!({"moment": "hopper://bad"})),
        call(3, "step", json!({"kind": "sideways"})),
        call(4, "eval", json!({"expresion": "a"})),
        call(5, "memory", json!({"address": 16, "length": 4})),
        call(6, "seek", json!({})),
    ]);
    for reply in &r {
        assert_eq!(reply["result"]["isError"], true, "{reply}");
        assert!(
            !reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        r[3]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("expresion")
    );
}

#[test]
fn ping_and_newer_protocol() {
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        json!({"jsonrpc":"2.0","id":"b","method":"ping"}),
    ]);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["id"], "b");
}
