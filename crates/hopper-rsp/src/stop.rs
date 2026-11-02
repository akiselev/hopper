//! Stop replies: `T`/`S` (stopped), `W` (exited), `X` (killed by a signal).
//!
//! A `T` reply carries `n:r;` pairs: a hex register number and its value, or a named field.
//! hopper reads `thread:`, `core:`, `watch:`/`rwatch:`/`awatch:`, `swbreak:`, `hwbreak:` and
//! `replaylog:`. rr reports every data watchpoint as `watch:<addr>` whatever its kind, a
//! breakpoint as `swbreak:` (or `hwbreak:` for an execute watchpoint) and the start of the
//! recording while running backwards as signal 0 with `replaylog:begin`
//! (`GdbServer::maybe_notify_stop`). Signal numbers on the wire are gdb's, not Linux's; see
//! [`gdb_signal_to_linux`].

use hopper_core::target::BreakKind;
use hopper_core::{HopperError, Result};

use crate::packet::{from_hex, parse_hex_u64};
use crate::thread::ThreadId;

/// Why a stopped thread stopped, as far as the reply says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopCause {
    /// The reply names no reason (a single step, a signal, an interrupt).
    Unspecified,
    SoftwareBreak,
    HardwareBreak,
    Watchpoint {
        kind: BreakKind,
        addr: u64,
    },
    /// Reverse execution reached the start of the recording (`replaylog:begin`).
    HistoryBegin,
    /// Forward execution reached the end of the recording (`replaylog:end`).
    HistoryEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalStop {
    /// The signal in gdb's numbering.
    pub signal: u8,
    pub thread: Option<ThreadId>,
    pub cause: StopCause,
    /// Expedited registers: (gdb regnum, little-endian bytes).
    pub registers: Vec<(u32, Vec<u8>)>,
    pub core: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReply {
    Stopped(SignalStop),
    /// `W`: the process exited with this status.
    Exited {
        code: u8,
        pid: Option<i32>,
    },
    /// `X`: the process was killed by this signal (gdb numbering).
    Killed {
        signal: u8,
        pid: Option<i32>,
    },
}

fn parse_sig(s: &str) -> Result<u8> {
    u8::from_str_radix(s, 16).map_err(|_| HopperError::Protocol(format!("bad signal number {s:?}")))
}

fn parse_process_suffix(rest: &str) -> Result<Option<i32>> {
    if rest.is_empty() {
        return Ok(None);
    }
    let pid = rest
        .strip_prefix(";process:")
        .ok_or_else(|| HopperError::Protocol(format!("bad exit reply suffix {rest:?}")))?;
    i32::from_str_radix(pid, 16)
        .map(Some)
        .map_err(|_| HopperError::Protocol(format!("bad pid {pid:?}")))
}

impl StopReply {
    /// Whether `payload` (run-length expanded) is a stop reply at all.
    pub fn is_stop_reply(payload: &[u8]) -> bool {
        matches!(payload.first(), Some(b'T' | b'S' | b'W' | b'X'))
    }

    pub fn parse(payload: &[u8]) -> Result<StopReply> {
        let text = std::str::from_utf8(payload)
            .map_err(|_| HopperError::Protocol("stop reply is not UTF-8".into()))?;
        let bad = || HopperError::Protocol(format!("bad stop reply {text:?}"));
        let kind = text.chars().next().ok_or_else(bad)?;
        let body = &text[1..];
        match kind {
            'W' | 'X' => {
                // gdbserver sends the status without padding (`W0;process:1f`), rr pads it to
                // two digits (`W00`).
                let (num, rest) = match body.find(';') {
                    Some(i) => body.split_at(i),
                    None => (body, ""),
                };
                if num.is_empty() {
                    return Err(bad());
                }
                let n = parse_sig(num)?;
                let pid = parse_process_suffix(rest)?;
                Ok(if kind == 'W' {
                    StopReply::Exited { code: n, pid }
                } else {
                    StopReply::Killed { signal: n, pid }
                })
            }
            'S' => {
                if body.len() != 2 {
                    return Err(bad());
                }
                Ok(StopReply::Stopped(SignalStop {
                    signal: parse_sig(body)?,
                    thread: None,
                    cause: StopCause::Unspecified,
                    registers: Vec::new(),
                    core: None,
                }))
            }
            'T' => {
                if body.len() < 2 {
                    return Err(bad());
                }
                let (num, rest) = body.split_at(2);
                let mut stop = SignalStop {
                    signal: parse_sig(num)?,
                    thread: None,
                    cause: StopCause::Unspecified,
                    registers: Vec::new(),
                    core: None,
                };
                let mut causes: Vec<StopCause> = Vec::new();
                for field in rest.split(';').filter(|f| !f.is_empty()) {
                    let (name, value) = field.split_once(':').ok_or_else(bad)?;
                    match name {
                        "thread" => stop.thread = Some(ThreadId::parse(value)?),
                        "core" => {
                            stop.core = Some(u32::from_str_radix(value, 16).map_err(|_| bad())?)
                        }
                        "watch" | "rwatch" | "awatch" => {
                            let kind = match name {
                                "watch" => BreakKind::WriteWatch,
                                "rwatch" => BreakKind::ReadWatch,
                                _ => BreakKind::AccessWatch,
                            };
                            causes.push(StopCause::Watchpoint {
                                kind,
                                addr: parse_hex_u64(value)?,
                            });
                        }
                        "swbreak" => causes.push(StopCause::SoftwareBreak),
                        "hwbreak" => causes.push(StopCause::HardwareBreak),
                        "replaylog" => causes.push(match value {
                            "begin" => StopCause::HistoryBegin,
                            "end" => StopCause::HistoryEnd,
                            _ => return Err(bad()),
                        }),
                        _ if name.bytes().all(|b| b.is_ascii_hexdigit()) => {
                            let regnum = u32::from_str_radix(name, 16).map_err(|_| bad())?;
                            stop.registers.push((regnum, from_hex(value.as_bytes())?));
                        }
                        // Other named fields (library, fork, exec, create, ...) do not concern
                        // hopper's targets.
                        _ => {}
                    }
                }
                // One cause per stop, the most specific first.
                let rank = |c: &StopCause| match c {
                    StopCause::Watchpoint { .. } => 0,
                    StopCause::HistoryBegin | StopCause::HistoryEnd => 1,
                    StopCause::HardwareBreak => 2,
                    StopCause::SoftwareBreak => 3,
                    StopCause::Unspecified => 4,
                };
                causes.sort_by_key(rank);
                if let Some(c) = causes.into_iter().next() {
                    stop.cause = c;
                }
                Ok(StopReply::Stopped(stop))
            }
            _ => Err(bad()),
        }
    }
}

/// gdb's signal numbers (`gdb/signals.def`) mapped to Linux x86-64 ones, the inverse of rr's
/// `to_gdb_signum`. `None` for numbers with no Linux equivalent.
pub fn gdb_signal_to_linux(gdb: u8) -> Option<u8> {
    Some(match gdb {
        0 => 0,
        1 => 1,   // HUP
        2 => 2,   // INT
        3 => 3,   // QUIT
        4 => 4,   // ILL
        5 => 5,   // TRAP
        6 => 6,   // ABRT
        8 => 8,   // FPE
        9 => 9,   // KILL
        10 => 7,  // BUS
        11 => 11, // SEGV
        12 => 31, // SYS
        13 => 13, // PIPE
        14 => 14, // ALRM
        15 => 15, // TERM
        16 => 23, // URG
        17 => 19, // STOP
        18 => 20, // TSTP
        19 => 18, // CONT
        20 => 17, // CHLD
        21 => 21, // TTIN
        22 => 22, // TTOU
        23 => 29, // IO
        24 => 24, // XCPU
        25 => 25, // XFSZ
        26 => 26, // VTALRM
        27 => 27, // PROF
        28 => 28, // WINCH
        30 => 10, // USR1
        31 => 12, // USR2
        32 => 30, // PWR
        38 => 16, // STKFLT (gdb's DANGER, as rr maps it)
        77 => 32,
        45..=75 => gdb - 12, // REALTIME_33 .. REALTIME_63
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::Tid;

    #[test]
    fn parses_gdbserver_t_reply() {
        let r = StopReply::parse(
            b"T0506:0000000000000000;07:d0c2ffffff7f0000;thread:p21f9.21f9;core:3;",
        )
        .unwrap();
        let StopReply::Stopped(s) = r else { panic!() };
        assert_eq!(s.signal, 5);
        assert_eq!(s.thread, Some(ThreadId::new(Some(0x21f9), Tid(0x21f9))));
        assert_eq!(s.core, Some(3));
        assert_eq!(s.cause, StopCause::Unspecified);
        assert_eq!(s.registers.len(), 2);
        assert_eq!(s.registers[1].0, 7);
        assert_eq!(s.registers[1].1[0], 0xd0);
    }

    #[test]
    fn parses_rr_reasons() {
        let r = StopReply::parse(b"T05thread:p10.11;watch:7ffd1230;").unwrap();
        let StopReply::Stopped(s) = r else { panic!() };
        assert_eq!(
            s.cause,
            StopCause::Watchpoint {
                kind: BreakKind::WriteWatch,
                addr: 0x7ffd1230
            }
        );
        let r = StopReply::parse(b"T00thread:p10.10;replaylog:begin;").unwrap();
        let StopReply::Stopped(s) = r else { panic!() };
        assert_eq!(s.signal, 0);
        assert_eq!(s.cause, StopCause::HistoryBegin);
        let r = StopReply::parse(b"T05thread:10;swbreak:;").unwrap();
        let StopReply::Stopped(s) = r else { panic!() };
        assert_eq!(s.cause, StopCause::SoftwareBreak);
        assert_eq!(s.thread, Some(ThreadId::new(None, Tid(16))));
        let r = StopReply::parse(b"T05thread:10;hwbreak:;").unwrap();
        let StopReply::Stopped(s) = r else { panic!() };
        assert_eq!(s.cause, StopCause::HardwareBreak);
        for (text, kind) in [
            ("rwatch", BreakKind::ReadWatch),
            ("awatch", BreakKind::AccessWatch),
        ] {
            let r = StopReply::parse(format!("T05{text}:10;").as_bytes()).unwrap();
            let StopReply::Stopped(s) = r else { panic!() };
            assert_eq!(s.cause, StopCause::Watchpoint { kind, addr: 16 });
        }
    }

    #[test]
    fn parses_exit_forms() {
        assert_eq!(
            StopReply::parse(b"W00").unwrap(),
            StopReply::Exited { code: 0, pid: None }
        );
        assert_eq!(
            StopReply::parse(b"W03;process:21f9").unwrap(),
            StopReply::Exited {
                code: 3,
                pid: Some(0x21f9)
            }
        );
        assert_eq!(
            StopReply::parse(b"X09").unwrap(),
            StopReply::Killed {
                signal: 9,
                pid: None
            }
        );
        assert_eq!(
            StopReply::parse(b"W0;process:2a").unwrap(),
            StopReply::Exited {
                code: 0,
                pid: Some(0x2a)
            }
        );
        assert_eq!(
            StopReply::parse(b"X9;process:2a").unwrap(),
            StopReply::Killed {
                signal: 9,
                pid: Some(0x2a)
            }
        );
        assert!(
            matches!(StopReply::parse(b"S0b").unwrap(), StopReply::Stopped(s) if s.signal == 11)
        );
        assert!(StopReply::parse(b"W").is_err());
        assert!(StopReply::parse(b"W00;pid:1").is_err());
        assert!(StopReply::parse(b"OK").is_err());
        assert!(StopReply::parse(b"T05thread").is_err());
    }

    #[test]
    fn maps_signals() {
        assert_eq!(gdb_signal_to_linux(5), Some(5));
        assert_eq!(gdb_signal_to_linux(10), Some(7));
        assert_eq!(gdb_signal_to_linux(20), Some(17));
        assert_eq!(gdb_signal_to_linux(30), Some(10));
        assert_eq!(gdb_signal_to_linux(45), Some(33));
        assert_eq!(gdb_signal_to_linux(143), None);
    }
}
