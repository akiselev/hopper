//! Navigation: the debugger keys, and the GUI's mirror of the engine's navigation history.

use hopper_core::api::{Direction, StepKind};
use hopper_core::Moment;
use skelter_base::event::Modifiers;
use skelter_base::key::Key;

/// What a key (or a toolbar button) asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Step { kind: StepKind, dir: Direction },
    /// Run to the next breakpoint (or the end, or the start in reverse).
    Continue { dir: Direction },
    /// Navigation history: the previous position.
    Back,
    /// Navigation history: the next position.
    Forward,
    /// Opens the position-URL box.
    OpenUrlBox,
    /// Copies the current position URL.
    CopyUrl,
    /// Adds or removes a breakpoint on the source caret's line (the stopped line when the
    /// source has no caret).
    ToggleBreakpoint,
}

impl Command {
    /// Whether the command moves the cursor through the engine.
    pub fn moves(self) -> bool {
        !matches!(self, Command::OpenUrlBox | Command::CopyUrl | Command::ToggleBreakpoint)
    }
}

/// The debugger keymap: F10 over, F11 into, Shift+F11 out, F5 continue, each reversed with Ctrl
/// (Shift+F5 is reverse continue too); Alt+Left/Right back and forward; Ctrl+L the URL box;
/// Ctrl+Shift+C copies the position URL; F9 toggles a breakpoint.
pub fn command_for_key(key: Key, mods: Modifiers) -> Option<Command> {
    let ctrl = mods.contains(Modifiers::CTRL);
    let shift = mods.contains(Modifiers::SHIFT);
    let alt = mods.contains(Modifiers::ALT);
    let dir = if ctrl { Direction::Reverse } else { Direction::Forward };
    match (key, alt) {
        (Key::F9, false) if !ctrl && !shift => Some(Command::ToggleBreakpoint),
        (Key::F10, false) if !shift => Some(Command::Step { kind: StepKind::Over, dir }),
        (Key::F11, false) => Some(Command::Step { kind: if shift { StepKind::Out } else { StepKind::Into }, dir }),
        (Key::F5, false) => Some(Command::Continue { dir: if ctrl || shift { Direction::Reverse } else { Direction::Forward } }),
        (Key::ArrowLeft, true) if !ctrl && !shift => Some(Command::Back),
        (Key::ArrowRight, true) if !ctrl && !shift => Some(Command::Forward),
        (Key::L, false) if ctrl && !shift => Some(Command::OpenUrlBox),
        (Key::C, false) if ctrl && shift => Some(Command::CopyUrl),
        _ => None,
    }
}

/// How the cursor got where it is, as the history sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// A step, continue, seek or URL: a new history entry (forward entries are dropped).
    New,
    /// The engine's `back` answered with a stop.
    Back,
    /// The engine's `forward` answered with a stop.
    Forward,
}

/// The positions the cursor visited, as the engine's history holds them: the GUI's mirror of it,
/// for the toolbar's enabled state and the history list. The engine stays the authority: a back or
/// forward goes to it, and only its answer moves this.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NavState {
    entries: Vec<Moment>,
    /// Index of the current entry in `entries`.
    current: Option<usize>,
}

impl NavState {
    /// The first stop (the cursor when the recording opened).
    pub fn start(&mut self, at: Moment) {
        self.entries = vec![at];
        self.current = Some(0);
    }

    /// The cursor moved to `at` by `how`.
    pub fn moved(&mut self, at: Moment, how: Move) {
        match (how, self.current) {
            (Move::New, Some(i)) => {
                if self.entries[i] == at {
                    return;
                }
                self.entries.truncate(i + 1);
                self.entries.push(at);
                self.current = Some(i + 1);
            }
            (Move::New, None) => self.start(at),
            (Move::Back, Some(i)) if i > 0 => {
                self.current = Some(i - 1);
                // the engine is the authority: take its moment if the mirror drifted
                self.entries[i - 1] = at;
            }
            (Move::Forward, Some(i)) if i + 1 < self.entries.len() => {
                self.current = Some(i + 1);
                self.entries[i + 1] = at;
            }
            // the engine moved where the mirror had no entry: treat it as a new one
            (Move::Back | Move::Forward, _) => {
                let i = self.current.map_or(0, |i| i + 1).min(self.entries.len());
                self.entries.insert(i, at);
                self.current = Some(i);
            }
        }
    }

    pub fn can_back(&self) -> bool {
        self.current.is_some_and(|i| i > 0)
    }

    pub fn can_forward(&self) -> bool {
        self.current.is_some_and(|i| i + 1 < self.entries.len())
    }

    pub fn current(&self) -> Option<Moment> {
        self.current.map(|i| self.entries[i])
    }

    pub fn entries(&self) -> &[Moment] {
        &self.entries
    }
}

/// The history move a navigation result is.
pub fn move_of(cmd: Command) -> Move {
    match cmd {
        Command::Back => Move::Back,
        Command::Forward => Move::Forward,
        _ => Move::New,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::{RecordingId, Tid};

    fn m(ft: u64) -> Moment {
        Moment { recording: RecordingId([1; 16]), frame_time: ft, tid: Tid(1), ticks: 0, sub: 0 }
    }

    #[test]
    fn keymap() {
        let none = Modifiers::default();
        let fwd = Direction::Forward;
        let rev = Direction::Reverse;
        assert_eq!(command_for_key(Key::F10, none), Some(Command::Step { kind: StepKind::Over, dir: fwd }));
        assert_eq!(command_for_key(Key::F10, Modifiers::CTRL), Some(Command::Step { kind: StepKind::Over, dir: rev }));
        assert_eq!(command_for_key(Key::F11, none), Some(Command::Step { kind: StepKind::Into, dir: fwd }));
        assert_eq!(command_for_key(Key::F11, Modifiers::CTRL), Some(Command::Step { kind: StepKind::Into, dir: rev }));
        assert_eq!(command_for_key(Key::F11, Modifiers::SHIFT), Some(Command::Step { kind: StepKind::Out, dir: fwd }));
        assert_eq!(command_for_key(Key::F11, Modifiers::SHIFT.union(Modifiers::CTRL)), Some(Command::Step { kind: StepKind::Out, dir: rev }));
        assert_eq!(command_for_key(Key::F5, none), Some(Command::Continue { dir: fwd }));
        assert_eq!(command_for_key(Key::F5, Modifiers::SHIFT), Some(Command::Continue { dir: rev }));
        assert_eq!(command_for_key(Key::F5, Modifiers::CTRL), Some(Command::Continue { dir: rev }));
        assert_eq!(command_for_key(Key::ArrowLeft, Modifiers::ALT), Some(Command::Back));
        assert_eq!(command_for_key(Key::ArrowRight, Modifiers::ALT), Some(Command::Forward));
        assert_eq!(command_for_key(Key::ArrowLeft, none), None, "plain arrows stay the panes'");
        assert_eq!(command_for_key(Key::L, Modifiers::CTRL), Some(Command::OpenUrlBox));
        assert_eq!(command_for_key(Key::C, Modifiers::CTRL.union(Modifiers::SHIFT)), Some(Command::CopyUrl));
        assert_eq!(command_for_key(Key::C, Modifiers::CTRL), None, "plain copy stays the panes'");
        assert_eq!(command_for_key(Key::F9, none), Some(Command::ToggleBreakpoint));
        assert!(!Command::ToggleBreakpoint.moves());
    }

    #[test]
    fn back_and_forward_walk_the_entries() {
        let mut n = NavState::default();
        n.start(m(1));
        assert!(!n.can_back());
        n.moved(m(2), Move::New);
        n.moved(m(3), Move::New);
        assert_eq!(n.entries(), [m(1), m(2), m(3)]);
        n.moved(m(2), Move::Back);
        assert_eq!(n.current(), Some(m(2)));
        assert!(n.can_back() && n.can_forward());
        n.moved(m(1), Move::Back);
        assert!(!n.can_back());
        n.moved(m(2), Move::Forward);
        n.moved(m(3), Move::Forward);
        assert_eq!(n.current(), Some(m(3)));
        assert!(!n.can_forward());
    }

    #[test]
    fn a_new_move_drops_the_forward_entries_and_repeats_are_one_entry() {
        let mut n = NavState::default();
        n.start(m(1));
        n.moved(m(2), Move::New);
        n.moved(m(3), Move::New);
        n.moved(m(2), Move::Back);
        n.moved(m(9), Move::New);
        assert_eq!(n.entries(), [m(1), m(2), m(9)]);
        assert!(!n.can_forward());
        n.moved(m(9), Move::New);
        assert_eq!(n.entries().len(), 3, "a step that did not move adds nothing");
    }

    #[test]
    fn the_engine_wins_when_the_mirror_disagrees() {
        let mut n = NavState::default();
        n.start(m(1));
        n.moved(m(2), Move::New);
        n.moved(m(7), Move::Back);
        assert_eq!(n.entries(), [m(7), m(2)]);
        // a forward with no forward entry becomes one
        let mut n = NavState::default();
        n.start(m(1));
        n.moved(m(5), Move::Forward);
        assert_eq!(n.entries(), [m(1), m(5)]);
        assert_eq!(n.current(), Some(m(5)));
    }
}
