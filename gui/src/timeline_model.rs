//! The timeline's view model: moments along x, and where a click or drag seeks.
//!
//! x is the event number (`frame_time`) between the recording's first and last moments. A click
//! snaps to the nearest moment hopper knows to be valid (an event mark, the start, the end or the
//! cursor): a `Moment` needs a thread and tick count that only the engine can name, so the GUI
//! never makes one up from a pixel.

use hopper_core::api::{TimelineEvent, TimelineKind};
use hopper_core::Moment;

/// What kind of mark an event draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkKind {
    Syscall,
    Signal,
    Process,
    Thread,
    Anchor,
}

impl MarkKind {
    pub fn of(kind: &TimelineKind) -> MarkKind {
        match kind {
            TimelineKind::Syscall { .. } => MarkKind::Syscall,
            TimelineKind::Signal { .. } => MarkKind::Signal,
            TimelineKind::Exec { .. } | TimelineKind::ProcessExit { .. } => MarkKind::Process,
            TimelineKind::ThreadStart | TimelineKind::ThreadExit => MarkKind::Thread,
            TimelineKind::Anchor { .. } => MarkKind::Anchor,
        }
    }
}

/// The recording along one axis.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineModel {
    pub start: Option<Moment>,
    pub end: Option<Moment>,
    pub events: Vec<TimelineEvent>,
    pub cursor: Option<Moment>,
    /// Bumped on every change, so the canvas knows to redraw.
    pub version: u64,
}

impl TimelineModel {
    fn span(&self) -> Option<(f64, f64)> {
        let first = self.start.map(|m| m.frame_time).or_else(|| self.events.iter().map(|e| e.at.frame_time).min())?;
        let last = self.end.map(|m| m.frame_time).or_else(|| self.events.iter().map(|e| e.at.frame_time).max())?;
        Some((first as f64, (last as f64).max(first as f64 + 1.0)))
    }

    /// Where `m` sits, 0 at the start to 1 at the end.
    pub fn fraction(&self, m: &Moment) -> Option<f64> {
        let (a, b) = self.span()?;
        Some(((m.frame_time as f64 - a) / (b - a)).clamp(0.0, 1.0))
    }

    /// The moment a press at `fraction` (0..1) seeks to: the nearest known moment.
    pub fn seek_target(&self, fraction: f64) -> Option<Moment> {
        let candidates = self.events.iter().map(|e| e.at).chain(self.start).chain(self.end).chain(self.cursor);
        candidates
            .filter_map(|m| Some((m, (self.fraction(&m)? - fraction).abs())))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(m, _)| m)
    }

    /// The marks to draw: each event's fraction and kind.
    pub fn marks(&self) -> Vec<(f64, MarkKind)> {
        self.events.iter().filter_map(|e| Some((self.fraction(&e.at)?, MarkKind::of(&e.kind)))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::{RecordingId, Tid};

    fn m(ft: u64) -> Moment {
        Moment { recording: RecordingId([3; 16]), frame_time: ft, tid: Tid(1), ticks: 0, sub: 0 }
    }

    fn model() -> TimelineModel {
        TimelineModel {
            start: Some(m(0)),
            end: Some(m(100)),
            events: vec![
                TimelineEvent { at: m(10), kind: TimelineKind::Syscall { name: "write".into() } },
                TimelineEvent { at: m(60), kind: TimelineKind::Anchor { name: "site".into() } },
            ],
            cursor: Some(m(40)),
            version: 1,
        }
    }

    #[test]
    fn moments_map_to_fractions() {
        let t = model();
        assert_eq!(t.fraction(&m(0)), Some(0.0));
        assert_eq!(t.fraction(&m(25)), Some(0.25));
        assert_eq!(t.fraction(&m(100)), Some(1.0));
        assert_eq!(t.marks(), vec![(0.1, MarkKind::Syscall), (0.6, MarkKind::Anchor)]);
    }

    #[test]
    fn a_click_snaps_to_the_nearest_known_moment() {
        let t = model();
        assert_eq!(t.seek_target(0.12), Some(m(10)));
        assert_eq!(t.seek_target(0.45), Some(m(40)), "the cursor is a target too");
        assert_eq!(t.seek_target(0.9), Some(m(100)));
        assert_eq!(t.seek_target(0.0), Some(m(0)));
        assert_eq!(TimelineModel::default().seek_target(0.5), None);
    }

    #[test]
    fn without_bounds_the_events_make_the_span() {
        let t = TimelineModel { start: None, end: None, ..model() };
        assert_eq!(t.fraction(&m(10)), Some(0.0));
        assert_eq!(t.fraction(&m(60)), Some(1.0));
    }
}
