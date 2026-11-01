//! Time in a recording.
//!
//! rr names a point in time by a global event number (the "frame time": events are totally
//! ordered because rr runs one thread at a time) plus the running thread's tick count, a count of
//! retired conditional branches that resets at clone and exec. rr itself says those two do not
//! uniquely name a program state: several instructions can retire without a conditional branch.
//! hopper adds `sub`, the number of instructions executed since the tick count last changed, so
//! that every instruction boundary has its own [`Moment`].
//!
//! Moments are ordered by `(frame_time, ticks, sub)` and only within one recording.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::HopperError;
use crate::ids::{RecordingId, Tid};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Moment {
    pub recording: RecordingId,
    /// rr's event number (`FrameTime`). For live targets, a synthetic stop counter.
    pub frame_time: u64,
    /// The thread that is running at this moment.
    pub tid: Tid,
    /// That thread's retired-conditional-branch count.
    pub ticks: u64,
    /// Instructions executed since `ticks` last changed.
    pub sub: u32,
}

impl Moment {
    /// Order two moments of the same recording. `None` when they belong to different
    /// recordings, which have no common timeline.
    pub fn cmp_in_recording(&self, other: &Moment) -> Option<Ordering> {
        if self.recording != other.recording {
            return None;
        }
        Some(
            (self.frame_time, self.ticks, self.sub).cmp(&(
                other.frame_time,
                other.ticks,
                other.sub,
            )),
        )
    }

    pub fn is_before(&self, other: &Moment) -> bool {
        self.cmp_in_recording(other) == Some(Ordering::Less)
    }

    /// The key that orders moments inside one recording.
    pub fn order_key(&self) -> (u64, u64, u32) {
        (self.frame_time, self.ticks, self.sub)
    }

    pub fn url(&self) -> PositionUrl {
        PositionUrl(*self)
    }
}

impl PartialOrd for Moment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.cmp_in_recording(other)
    }
}

impl fmt::Display for Moment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.frame_time, self.tid.0, self.ticks, self.sub)
    }
}

/// A half-open range of moments in one recording. A missing bound is open.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct MomentRange {
    pub start: Option<Moment>,
    pub end: Option<Moment>,
}

impl MomentRange {
    pub fn all() -> Self {
        MomentRange { start: None, end: None }
    }

    pub fn between(start: Moment, end: Moment) -> Self {
        MomentRange { start: Some(start), end: Some(end) }
    }

    pub fn contains(&self, m: &Moment) -> bool {
        let after_start = match &self.start {
            Some(s) => matches!(s.cmp_in_recording(m), Some(Ordering::Less | Ordering::Equal)),
            None => true,
        };
        let before_end = match &self.end {
            Some(e) => m.cmp_in_recording(e) == Some(Ordering::Less),
            None => true,
        };
        after_start && before_end
    }
}

/// A shareable link to one moment: `hopper://<recording>/<frame_time>.<tid>.<ticks>.<sub>`.
/// The GUI, the CLI and agents all accept it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PositionUrl(pub Moment);

impl fmt::Display for PositionUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hopper://{}/{}", self.0.recording, self.0)
    }
}

impl FromStr for PositionUrl {
    type Err = HopperError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || HopperError::Invalid(format!("not a hopper position url: {s:?}"));
        let rest = s.strip_prefix("hopper://").ok_or_else(bad)?;
        let (rec, pos) = rest.split_once('/').ok_or_else(bad)?;
        let recording: RecordingId = rec.parse()?;
        let parts: Vec<&str> = pos.split('.').collect();
        if parts.len() != 4 {
            return Err(bad());
        }
        let frame_time = parts[0].parse().map_err(|_| bad())?;
        let tid = Tid(parts[1].parse().map_err(|_| bad())?);
        let ticks = parts[2].parse().map_err(|_| bad())?;
        let sub = parts[3].parse().map_err(|_| bad())?;
        Ok(PositionUrl(Moment { recording, frame_time, tid, ticks, sub }))
    }
}

impl Serialize for PositionUrl {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PositionUrl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(b: u8) -> RecordingId {
        RecordingId([b; 16])
    }

    fn m(r: u8, ft: u64, ticks: u64, sub: u32) -> Moment {
        Moment { recording: rec(r), frame_time: ft, tid: Tid(7), ticks, sub }
    }

    #[test]
    fn orders_within_one_recording_only() {
        assert!(m(1, 3, 0, 0) < m(1, 3, 1, 0));
        assert!(m(1, 3, 1, 0) < m(1, 3, 1, 2));
        assert!(m(1, 2, 99, 9) < m(1, 3, 0, 0));
        assert_eq!(m(1, 3, 0, 0).partial_cmp(&m(2, 3, 0, 0)), None);
    }

    #[test]
    fn url_round_trips() {
        let at = m(0xab, 120, 4567, 3);
        let url = at.url().to_string();
        assert_eq!(url, format!("hopper://{}/120.7.4567.3", "ab".repeat(16)));
        let back: PositionUrl = url.parse().unwrap();
        assert_eq!(back.0, at);
    }

    #[test]
    fn url_rejects_garbage() {
        assert!("hopper://zz/1.2.3.4".parse::<PositionUrl>().is_err());
        assert!(format!("hopper://{}/1.2.3", "ab".repeat(16)).parse::<PositionUrl>().is_err());
        assert!("http://x/1.2.3.4".parse::<PositionUrl>().is_err());
    }

    #[test]
    fn range_is_half_open() {
        let r = MomentRange::between(m(1, 2, 0, 0), m(1, 4, 0, 0));
        assert!(r.contains(&m(1, 2, 0, 0)));
        assert!(r.contains(&m(1, 3, 5, 0)));
        assert!(!r.contains(&m(1, 4, 0, 0)));
        assert!(!r.contains(&m(2, 3, 0, 0)));
        assert!(MomentRange::all().contains(&m(9, 0, 0, 0)));
    }
}
