//! Packet framing: `$<payload>#<checksum>`, binary escapes and run-length decoding.
//!
//! The rules follow the gdb manual ("Overview" of the remote protocol) and rr's server
//! (`GdbServerConnection.cc`):
//! - the checksum is the sum of the payload bytes modulo 256, as two lowercase hex digits;
//! - in binary data `#`, `$`, `}` and `*` are sent as `}` followed by the byte XOR 0x20;
//! - a reply may be run-length encoded: `X*N` repeats `X` a further `N - 29` times. gdb expands
//!   runs on the raw bytes before any binary unescaping, and so does this module.

use hopper_core::{HopperError, Result};

/// The interrupt byte (Ctrl-C) a client sends while the target runs.
pub const INTERRUPT: u8 = 0x03;

/// Sum of `bytes` modulo 256.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Frame `payload` as `$payload#cc`. The payload must already be escaped where it carries binary
/// data (see [`escape_binary`]).
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(b'$');
    out.extend_from_slice(payload);
    out.push(b'#');
    out.extend_from_slice(format!("{:02x}", checksum(payload)).as_bytes());
    out
}

/// Escape binary data for a packet body.
pub fn escape_binary(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        match b {
            b'#' | b'$' | b'}' | b'*' => {
                out.push(b'}');
                out.push(b ^ 0x20);
            }
            _ => out.push(b),
        }
    }
    out
}

/// Undo [`escape_binary`].
pub fn unescape_binary(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        if b == b'}' {
            let next = it
                .next()
                .ok_or_else(|| HopperError::Protocol("binary data ends inside an escape".into()))?;
            out.push(next ^ 0x20);
        } else {
            out.push(b);
        }
    }
    Ok(out)
}

/// Expand run-length encoding (`X*N` means `X` repeated `N - 29` more times).
pub fn expand_rle(data: &[u8]) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == b'*' {
            let count_char = *data
                .get(i + 1)
                .ok_or_else(|| HopperError::Protocol("run-length marker without a count".into()))?;
            let repeat = (count_char as i32) - 29;
            let prev = *out
                .last()
                .ok_or_else(|| HopperError::Protocol("run-length marker at packet start".into()))?;
            if !(0..=255).contains(&repeat) {
                return Err(HopperError::Protocol(format!(
                    "bad run-length count byte {count_char:#04x}"
                )));
            }
            out.extend(std::iter::repeat_n(prev, repeat as usize));
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Ok(out)
}

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode an even-length hex string.
pub fn from_hex(s: &[u8]) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(HopperError::Protocol(format!(
            "odd-length hex string {:?}",
            String::from_utf8_lossy(s)
        )));
    }
    s.chunks(2)
        .map(|pair| match (hex_digit(pair[0]), hex_digit(pair[1])) {
            (Some(h), Some(l)) => Ok((h << 4) | l),
            _ => Err(HopperError::Protocol(format!(
                "bad hex digits {:?}",
                String::from_utf8_lossy(pair)
            ))),
        })
        .collect()
}

/// Parse a hex number with no prefix.
pub fn parse_hex_u64(s: &str) -> Result<u64> {
    u64::from_str_radix(s, 16)
        .map_err(|_| HopperError::Protocol(format!("bad hex number {s:?}")))
}

/// What came off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// `+`: the last packet arrived intact.
    Ack,
    /// `-`: the last packet must be sent again.
    Nack,
    /// A packet whose checksum matched. The payload is raw: not yet run-length expanded.
    Packet(Vec<u8>),
    /// A packet whose checksum did not match.
    Corrupt { payload: Vec<u8>, sent: u8, computed: u8 },
    /// `%...#cc`: an asynchronous notification (non-stop mode). hopper never enables non-stop,
    /// so these are skipped.
    Notification(Vec<u8>),
    /// The interrupt byte (only a server sees this; scripted servers use it).
    Interrupt,
}

/// Incremental parser over a byte stream. Feed bytes with [`Deframer::push`] and take complete
/// items with [`Deframer::next`].
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete item, or `None` when more bytes are needed. Bytes outside any packet
    /// (other than acks and the interrupt byte) are discarded, as gdb does.
    pub fn next(&mut self) -> Result<Option<Incoming>> {
        loop {
            let Some(&first) = self.buf.first() else {
                return Ok(None);
            };
            match first {
                b'+' => {
                    self.buf.remove(0);
                    return Ok(Some(Incoming::Ack));
                }
                b'-' => {
                    self.buf.remove(0);
                    return Ok(Some(Incoming::Nack));
                }
                INTERRUPT => {
                    self.buf.remove(0);
                    return Ok(Some(Incoming::Interrupt));
                }
                b'$' | b'%' => {
                    let Some(hash) = self.buf.iter().position(|&b| b == b'#') else {
                        return Ok(None);
                    };
                    if self.buf.len() < hash + 3 {
                        return Ok(None);
                    }
                    let payload = self.buf[1..hash].to_vec();
                    let sent_text = [self.buf[hash + 1], self.buf[hash + 2]];
                    self.buf.drain(..hash + 3);
                    let sent = match (hex_digit(sent_text[0]), hex_digit(sent_text[1])) {
                        (Some(h), Some(l)) => (h << 4) | l,
                        _ => {
                            return Err(HopperError::Protocol(format!(
                                "bad checksum digits {:?}",
                                String::from_utf8_lossy(&sent_text)
                            )));
                        }
                    };
                    if first == b'%' {
                        return Ok(Some(Incoming::Notification(payload)));
                    }
                    let computed = checksum(&payload);
                    if computed != sent {
                        return Ok(Some(Incoming::Corrupt { payload, sent, computed }));
                    }
                    return Ok(Some(Incoming::Packet(payload)));
                }
                _ => {
                    // Noise between packets: drop it.
                    self.buf.remove(0);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_with_checksum() {
        assert_eq!(frame(b"OK"), b"$OK#9a");
        assert_eq!(frame(b""), b"$#00");
        assert_eq!(frame(b"qSupported"), b"$qSupported#37");
    }

    #[test]
    fn escapes_round_trip() {
        let data = vec![b'#', b'$', b'}', b'*', 0, 0xff, b'a'];
        let esc = escape_binary(&data);
        assert_eq!(esc, b"}\x03}\x04}]}\x0a\x00\xffa");
        assert_eq!(unescape_binary(&esc).unwrap(), data);
        assert!(unescape_binary(b"ab}").is_err());
    }

    #[test]
    fn expands_runs() {
        // "0* " is '0' followed by 32-29 = 3 more: "0000".
        assert_eq!(expand_rle(b"0* ").unwrap(), b"0000");
        // gdbserver's `g` reply style.
        assert_eq!(expand_rle(b"ab*\"c").unwrap(), b"abbbbbbc");
        assert!(expand_rle(b"*a").is_err());
        assert!(expand_rle(b"a*").is_err());
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(to_hex(&[0, 0xab, 0x10]), "00ab10");
        assert_eq!(from_hex(b"00AB10").unwrap(), vec![0, 0xab, 0x10]);
        assert!(from_hex(b"abc").is_err());
        assert!(from_hex(b"zz").is_err());
    }

    #[test]
    fn deframes_mixed_stream() {
        let mut d = Deframer::default();
        d.push(b"+$OK#9a");
        d.push(b"-junk$O");
        assert_eq!(d.next().unwrap(), Some(Incoming::Ack));
        assert_eq!(d.next().unwrap(), Some(Incoming::Packet(b"OK".to_vec())));
        assert_eq!(d.next().unwrap(), Some(Incoming::Nack));
        assert_eq!(d.next().unwrap(), None);
        d.push(b"K#00%Stop:T05#00");
        assert_eq!(
            d.next().unwrap(),
            Some(Incoming::Corrupt { payload: b"OK".to_vec(), sent: 0, computed: 0x9a })
        );
        assert_eq!(d.next().unwrap(), Some(Incoming::Notification(b"Stop:T05".to_vec())));
        d.push(&[INTERRUPT]);
        assert_eq!(d.next().unwrap(), Some(Incoming::Interrupt));
    }
}
