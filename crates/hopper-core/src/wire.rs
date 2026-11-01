//! The JSON form of the API types that the text front ends (MCP, DAP) share.
//!
//! [`to_json`] serialises any API value with serde and then rewrites it for readers that are
//! not Rust:
//! - every [`Moment`] becomes its position URL string (`hopper://<recording>/<ft>.<tid>.<ticks>.<sub>`);
//! - addresses (`addr`, `pc`, `cfa`, `address` fields) become `"0x…"` strings;
//! - byte buffers (`bytes`, `value` arrays of integers) become lowercase hex strings;
//! - a [`RecordingId`] field named `recording` or `id` holding 16 bytes becomes its hex string.
//!
//! Enums keep serde's string tags (`"Step"`, `{"Breakpoint": {"id": 1}}`), never integers.
//! The input helpers accept what the output produces: moments as URL strings, integers as JSON
//! numbers or `"0x…"` / decimal strings.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::api::BreakLocation;
use crate::error::{HopperError, Result};
use crate::ids::RecordingId;
use crate::moment::{Moment, PositionUrl};
use crate::source::SourceLoc;

/// Serialise an API value into hopper's wire JSON.
pub fn to_json<T: Serialize + ?Sized>(value: &T) -> Value {
    let v = serde_json::to_value(value).unwrap_or(Value::Null);
    rewrite(v, None)
}

/// A moment as its position URL string.
pub fn moment_json(m: &Moment) -> Value {
    Value::String(PositionUrl(*m).to_string())
}

/// Parse a moment from its position URL string.
pub fn parse_moment(v: &Value) -> Result<Moment> {
    match v {
        Value::String(s) => Ok(s.parse::<PositionUrl>()?.0),
        other => Err(HopperError::Invalid(format!(
            "expected a position url string, got {other}"
        ))),
    }
}

/// Parse an unsigned integer given as a JSON number, a `"0x…"` hex string or a decimal string.
pub fn parse_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| HopperError::Invalid(format!("not an unsigned integer: {n}"))),
        Value::String(s) => parse_u64_str(s),
        other => Err(HopperError::Invalid(format!(
            "expected an integer, got {other}"
        ))),
    }
}

/// Parse `"0x…"` hex or decimal text.
pub fn parse_u64_str(s: &str) -> Result<u64> {
    let t = s.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16),
        None => t.replace('_', "").parse(),
    };
    parsed.map_err(|_| HopperError::Invalid(format!("not an integer: {s:?}")))
}

/// Parse `file:line` or `file:line:column` (the file may itself contain `:`).
pub fn parse_source_loc(s: &str) -> Result<SourceLoc> {
    let bad = || HopperError::Invalid(format!("expected file:line, got {s:?}"));
    let t = s.trim();
    let (head, last) = t.rsplit_once(':').ok_or_else(bad)?;
    let last: u32 = last.parse().map_err(|_| bad())?;
    if let Some((file, line)) = head.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
        && !file.is_empty()
    {
        return Ok(SourceLoc {
            file: file.to_string(),
            line,
            column: Some(last),
        });
    }
    if head.is_empty() {
        return Err(bad());
    }
    Ok(SourceLoc::new(head, last))
}

/// Parse a breakpoint location: `file:line`, `0x…` (an address) or a function name.
pub fn parse_break_location(s: &str) -> Result<BreakLocation> {
    let t = s.trim();
    if t.is_empty() {
        return Err(HopperError::Invalid("empty breakpoint location".into()));
    }
    if t.starts_with("0x") {
        return Ok(BreakLocation::Address(parse_u64_str(t)?));
    }
    if let Ok(loc) = parse_source_loc(t) {
        return Ok(BreakLocation::Source(loc));
    }
    Ok(BreakLocation::Function(t.to_string()))
}

/// Lowercase hex of a byte buffer.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_addr(n: u64) -> Value {
    Value::String(format!("{n:#x}"))
}

fn as_bytes(v: &[Value]) -> Option<Vec<u8>> {
    v.iter()
        .map(|x| x.as_u64().filter(|n| *n <= 255).map(|n| n as u8))
        .collect()
}

fn as_moment(map: &Map<String, Value>) -> Option<Moment> {
    const KEYS: [&str; 5] = ["recording", "frame_time", "tid", "ticks", "sub"];
    if map.len() != KEYS.len() || !KEYS.iter().all(|k| map.contains_key(*k)) {
        return None;
    }
    serde_json::from_value(Value::Object(map.clone())).ok()
}

fn rewrite(v: Value, key: Option<&str>) -> Value {
    match v {
        Value::Object(map) => {
            if let Some(m) = as_moment(&map) {
                return moment_json(&m);
            }
            Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let nv = rewrite(v, Some(&k));
                        (k, nv)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => {
            match key {
                Some("bytes" | "value" | "raw") => {
                    if let Some(b) = as_bytes(&items) {
                        return Value::String(hex(&b));
                    }
                }
                Some("recording" | "id") if items.len() == 16 => {
                    if let Some(b) = as_bytes(&items) {
                        let mut id = [0u8; 16];
                        id.copy_from_slice(&b);
                        return Value::String(RecordingId(id).to_hex());
                    }
                }
                _ => {}
            }
            Value::Array(items.into_iter().map(|v| rewrite(v, None)).collect())
        }
        Value::Number(n) => match (key, n.as_u64()) {
            (Some("addr" | "pc" | "cfa" | "address"), Some(a)) => hex_addr(a),
            _ => Value::Number(n),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{StopCause, WriteEvent};
    use crate::ids::{ProcessId, Tid};
    use crate::place::Place;

    fn m() -> Moment {
        Moment {
            recording: RecordingId([0xab; 16]),
            frame_time: 3,
            tid: Tid(7),
            ticks: 9,
            sub: 1,
        }
    }

    #[test]
    fn moments_become_urls_and_round_trip() {
        let v = to_json(&m());
        assert_eq!(
            v,
            Value::String(format!("hopper://{}/3.7.9.1", "ab".repeat(16)))
        );
        assert_eq!(parse_moment(&v).unwrap(), m());
    }

    #[test]
    fn nested_fields_are_rewritten() {
        let w = WriteEvent {
            at: m(),
            pc: 0x401000,
            place: Place::memory(ProcessId::new(1), 0x10, 2),
            value: vec![0xde, 0xad],
            function: None,
            location: None,
        };
        let v = to_json(&w);
        assert_eq!(v["pc"], "0x401000");
        assert_eq!(v["value"], "dead");
        assert_eq!(v["place"]["Memory"]["addr"], "0x10");
        assert_eq!(v["place"]["Memory"]["len"], 2);
        assert!(v["at"].as_str().unwrap().starts_with("hopper://"));
        assert_eq!(to_json(&StopCause::Step), "Step");
    }

    #[test]
    fn locations_parse() {
        assert_eq!(
            parse_source_loc("main.rs:16").unwrap(),
            SourceLoc::new("main.rs", 16)
        );
        let l = parse_source_loc("C:/x/main.rs:16:5").unwrap();
        assert_eq!(
            (l.file.as_str(), l.line, l.column),
            ("C:/x/main.rs", 16, Some(5))
        );
        assert!(parse_source_loc("main.rs").is_err());
        assert_eq!(
            parse_break_location("0x10").unwrap(),
            BreakLocation::Address(16)
        );
        assert_eq!(
            parse_break_location("load").unwrap(),
            BreakLocation::Function("load".into())
        );
        assert_eq!(
            parse_break_location("Vec::push").unwrap(),
            BreakLocation::Function("Vec::push".into())
        );
    }

    #[test]
    fn integers_parse_from_numbers_and_strings() {
        assert_eq!(parse_u64(&Value::from(12)).unwrap(), 12);
        assert_eq!(parse_u64(&Value::from("0x1f")).unwrap(), 31);
        assert_eq!(parse_u64(&Value::from("40")).unwrap(), 40);
        assert!(parse_u64(&Value::from("zz")).is_err());
        assert!(parse_u64(&Value::from(-1)).is_err());
    }
}
