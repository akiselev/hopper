//! Thread ids as the remote protocol writes them: `p<pid>.<tid>` with the multiprocess
//! extension, bare `<tid>` without it, all in hex.

use std::fmt;

use hopper_core::{HopperError, Result, Tid};

/// One thread named on the wire. `pid` is present when the server speaks the multiprocess
/// extension.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadId {
    pub pid: Option<i32>,
    pub tid: Tid,
}

impl ThreadId {
    pub fn new(pid: Option<i32>, tid: Tid) -> Self {
        ThreadId { pid, tid }
    }

    /// Parse `p<pid>.<tid>` or `<tid>` (hex). The special values `0` and `-1` are not threads;
    /// they are rejected here and handled by [`ThreadSelector`].
    pub fn parse(s: &str) -> Result<ThreadId> {
        let bad = || HopperError::Protocol(format!("bad thread id {s:?}"));
        let parse_num = |t: &str| -> Result<i32> {
            if t == "-1" {
                return Ok(-1);
            }
            i32::from_str_radix(t, 16).map_err(|_| bad())
        };
        let (pid, tid) = if let Some(rest) = s.strip_prefix('p') {
            match rest.split_once('.') {
                Some((p, t)) => (Some(parse_num(p)?), parse_num(t)?),
                None => return Err(bad()),
            }
        } else {
            (None, parse_num(s)?)
        };
        if tid <= 0 || pid.is_some_and(|p| p <= 0) {
            return Err(bad());
        }
        Ok(ThreadId { pid, tid: Tid(tid) })
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "p{:x}.{:x}", pid, self.tid.0),
            None => write!(f, "{:x}", self.tid.0),
        }
    }
}

/// The thread argument of `H`, `vCont` actions and similar packets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ThreadSelector {
    /// `0`: any thread, the server's choice.
    Any,
    /// `-1`: every thread.
    All,
    Thread(ThreadId),
}

impl fmt::Display for ThreadSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadSelector::Any => f.write_str("0"),
            ThreadSelector::All => f.write_str("-1"),
            ThreadSelector::Thread(t) => t.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms() {
        assert_eq!(
            ThreadId::parse("p21f9.21fa").unwrap(),
            ThreadId::new(Some(0x21f9), Tid(0x21fa))
        );
        assert_eq!(
            ThreadId::parse("1a").unwrap(),
            ThreadId::new(None, Tid(0x1a))
        );
        assert!(ThreadId::parse("0").is_err());
        assert!(ThreadId::parse("-1").is_err());
        assert!(ThreadId::parse("p12").is_err());
        assert!(ThreadId::parse("xyz").is_err());
    }

    #[test]
    fn formats_round_trip() {
        let t = ThreadId::new(Some(0x21f9), Tid(0x21fa));
        assert_eq!(t.to_string(), "p21f9.21fa");
        assert_eq!(ThreadId::parse(&t.to_string()).unwrap(), t);
        assert_eq!(ThreadSelector::Any.to_string(), "0");
        assert_eq!(ThreadSelector::All.to_string(), "-1");
        assert_eq!(
            ThreadSelector::Thread(ThreadId::new(None, Tid(255))).to_string(),
            "ff"
        );
    }
}
