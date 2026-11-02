//! Where two runs part ways: align two event streams by (site, per-site ordinal) and name the
//! first difference.
//!
//! The k-th event of site `s` in one stream is paired with the k-th event of `s` in the other,
//! whatever the interleaving of different sites (so two threads' decision events may interleave
//! differently between runs). **Control flow diverges** at the first event, in either stream,
//! that has no partner: one run reached a site more often than the other. Alignment stops
//! there: only pairs whose events both come before the divergence points are compared. Of
//! those, the first pair (in the left stream's order) whose payloads differ is the **first
//! payload difference**. Payloads compare as JSON values, so numbers compare by exact value.
//!
//! **JSONL input** ([`load_jsonl`]): one JSON object per line with a string `site`, an optional
//! `at` (a `hopper://` position URL), and either a `payload` member or, when there is none,
//! every other member as the payload. Blank lines are skipped. `docs/requests.md` asks cadabra3
//! for its `decision-trace` events in exactly this shape.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use hopper_core::{HopperError, Moment, PositionUrl, Result};
use serde_json::Value;

/// One event of a stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub site: String,
    pub payload: Value,
    /// Where the event happened in a recording, when known.
    pub at: Option<Moment>,
}

impl Event {
    pub fn new(site: impl Into<String>, payload: Value) -> Self {
        Event {
            site: site.into(),
            payload,
            at: None,
        }
    }
}

/// An event's place in its stream.
#[derive(Clone, Debug, PartialEq)]
pub struct EventRef {
    /// Position in the stream (0-based).
    pub index: usize,
    /// How many earlier events of the same site the stream has (0-based).
    pub ordinal: u64,
    pub event: Event,
}

/// Two aligned events whose payloads differ.
#[derive(Clone, Debug, PartialEq)]
pub struct PayloadDifference {
    pub left: EventRef,
    pub right: EventRef,
}

/// Where control flow diverged: the earliest unpartnered event of each stream (at least one is
/// present).
#[derive(Clone, Debug, PartialEq)]
pub struct ControlFlowDivergence {
    /// The first left event with no right partner (the left run reached its site once more
    /// than the right run did).
    pub left: Option<EventRef>,
    /// The first right event with no left partner.
    pub right: Option<EventRef>,
}

/// The comparison of two streams.
#[derive(Clone, Debug, PartialEq)]
pub struct Divergence {
    /// Pairs compared (both events before the divergence points).
    pub aligned: usize,
    pub first_payload_difference: Option<PayloadDifference>,
    pub control_flow: Option<ControlFlowDivergence>,
}

impl Divergence {
    /// Both streams agree: same sites, same counts, same payloads.
    pub fn is_identical(&self) -> bool {
        self.first_payload_difference.is_none() && self.control_flow.is_none()
    }
}

/// Per-site ordinals of a stream, and the position of each (site, ordinal).
fn ordinals(events: &[Event]) -> (Vec<u64>, HashMap<(&str, u64), usize>) {
    let mut counts: HashMap<&str, u64> = HashMap::new();
    let mut ords = Vec::with_capacity(events.len());
    let mut at = HashMap::with_capacity(events.len());
    for (i, e) in events.iter().enumerate() {
        let c = counts.entry(e.site.as_str()).or_insert(0);
        ords.push(*c);
        at.insert((e.site.as_str(), *c), i);
        *c += 1;
    }
    (ords, at)
}

fn event_ref(events: &[Event], ords: &[u64], i: usize) -> EventRef {
    EventRef {
        index: i,
        ordinal: ords[i],
        event: events[i].clone(),
    }
}

/// Compare two streams (see the module docs for the rules).
pub fn first_divergence(left: &[Event], right: &[Event]) -> Divergence {
    let (lo, lat) = ordinals(left);
    let (ro, rat) = ordinals(right);
    let left_split = (0..left.len()).find(|&i| !rat.contains_key(&(left[i].site.as_str(), lo[i])));
    let right_split =
        (0..right.len()).find(|&j| !lat.contains_key(&(right[j].site.as_str(), ro[j])));
    let mut aligned = 0;
    let mut first = None;
    for i in 0..left_split.unwrap_or(left.len()) {
        let Some(&j) = rat.get(&(left[i].site.as_str(), lo[i])) else {
            continue;
        };
        if right_split.is_some_and(|r| j >= r) {
            continue;
        }
        aligned += 1;
        if first.is_none() && left[i].payload != right[j].payload {
            first = Some(PayloadDifference {
                left: event_ref(left, &lo, i),
                right: event_ref(right, &ro, j),
            });
        }
    }
    let control_flow =
        (left_split.is_some() || right_split.is_some()).then(|| ControlFlowDivergence {
            left: left_split.map(|i| event_ref(left, &lo, i)),
            right: right_split.map(|j| event_ref(right, &ro, j)),
        });
    Divergence {
        aligned,
        first_payload_difference: first,
        control_flow,
    }
}

/// Parse one JSONL line.
pub fn parse_event(line: &str) -> Result<Event> {
    let bad = |why: &str| HopperError::Invalid(format!("event line {line:?}: {why}"));
    let v: Value = serde_json::from_str(line).map_err(|e| bad(&e.to_string()))?;
    let Value::Object(mut obj) = v else {
        return Err(bad("not a JSON object"));
    };
    let site = match obj.remove("site") {
        Some(Value::String(s)) => s,
        _ => return Err(bad("no string `site`")),
    };
    let at = match obj.remove("at") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.parse::<PositionUrl>()?.0),
        Some(_) => return Err(bad("`at` is not a position URL")),
    };
    let payload = match obj.remove("payload") {
        Some(p) => p,
        None => Value::Object(obj),
    };
    Ok(Event { site, payload, at })
}

/// Read a JSONL event stream.
pub fn load_jsonl(reader: impl BufRead) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(parse_event(&line)?);
    }
    Ok(out)
}

/// Read a JSONL event stream from a file.
pub fn load_jsonl_file(path: &Path) -> Result<Vec<Event>> {
    let f = std::fs::File::open(path)
        .map_err(|e| HopperError::Io(format!("{}: {e}", path.display())))?;
    load_jsonl(std::io::BufReader::new(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(site: &str, v: Value) -> Event {
        Event::new(site, v)
    }

    #[test]
    fn identical_streams() {
        let a = vec![ev("a", json!(1)), ev("b", json!(2)), ev("a", json!(3))];
        let d = first_divergence(&a, &a);
        assert!(d.is_identical());
        assert_eq!(d.aligned, 3);
    }

    #[test]
    fn names_the_first_payload_difference_by_site_and_ordinal() {
        let a = vec![
            ev("a", json!(1)),
            ev("b", json!(2)),
            ev("a", json!(3)),
            ev("a", json!(4)),
        ];
        let b = vec![
            ev("a", json!(1)),
            ev("b", json!(2)),
            ev("a", json!(30)),
            ev("a", json!(40)),
        ];
        let d = first_divergence(&a, &b);
        let p = d.first_payload_difference.unwrap();
        assert_eq!(
            (p.left.event.site.as_str(), p.left.ordinal, p.left.index),
            ("a", 1, 2)
        );
        assert_eq!(p.right.event.payload, json!(30));
        assert!(d.control_flow.is_none());
        assert_eq!(d.aligned, 4);
    }

    #[test]
    fn tolerates_interleaving_of_different_sites() {
        let a = vec![ev("x", json!(1)), ev("y", json!(2))];
        let b = vec![ev("y", json!(2)), ev("x", json!(1))];
        assert!(first_divergence(&a, &b).is_identical());
    }

    #[test]
    fn stops_at_control_flow_divergence() {
        // The right run takes a different branch after the second event: site `c` instead of
        // `b`, and later payload differences are not compared.
        let a = vec![
            ev("a", json!(1)),
            ev("b", json!(2)),
            ev("d", json!(5)),
            ev("a", json!(9)),
        ];
        let b = vec![
            ev("a", json!(1)),
            ev("c", json!(2)),
            ev("d", json!(6)),
            ev("a", json!(10)),
        ];
        let d = first_divergence(&a, &b);
        let cf = d.control_flow.clone().unwrap();
        assert_eq!(
            cf.left.as_ref().map(|e| (e.event.site.as_str(), e.index)),
            Some(("b", 1))
        );
        assert_eq!(
            cf.right.as_ref().map(|e| (e.event.site.as_str(), e.index)),
            Some(("c", 1))
        );
        assert_eq!(d.aligned, 1);
        assert!(d.first_payload_difference.is_none());
        // A payload difference before the split is still reported.
        let b2 = vec![ev("a", json!(0)), ev("c", json!(2))];
        let d2 = first_divergence(&a, &b2);
        assert_eq!(d2.first_payload_difference.unwrap().left.index, 0);
        assert!(d2.control_flow.is_some());
    }

    #[test]
    fn one_stream_longer() {
        let a = vec![ev("a", json!(1)), ev("a", json!(2))];
        let b = vec![ev("a", json!(1))];
        let d = first_divergence(&a, &b);
        let cf = d.control_flow.unwrap();
        assert_eq!(cf.left.unwrap().ordinal, 1);
        assert!(cf.right.is_none());
        assert_eq!(d.aligned, 1);
    }

    #[test]
    fn loads_jsonl() {
        let rec = "ab".repeat(16);
        let text = format!(
            "{{\"site\":\"ops.boolean.classify\",\"payload\":{{\"lhs\":1.5}}}}\n\n\
             {{\"site\":\"decide/7\",\"kind\":\"decision\",\"class\":\"Within\",\"at\":\"hopper://{rec}/3.4.5.6\"}}\n"
        );
        let evs = load_jsonl(text.as_bytes()).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].payload, json!({"lhs": 1.5}));
        assert_eq!(
            evs[1].payload,
            json!({"kind": "decision", "class": "Within"})
        );
        assert_eq!(evs[1].at.unwrap().order_key(), (3, 5, 6));
        assert!(load_jsonl("{\"payload\":1}\n".as_bytes()).is_err());
        assert!(load_jsonl("[1]\n".as_bytes()).is_err());
        assert!(load_jsonl("{\"site\":\"a\",\"at\":5}\n".as_bytes()).is_err());
    }
}
