//! DAP's base protocol: `Content-Length: N\r\n\r\n` followed by N bytes of JSON.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// Read one message. `Ok(None)` at a clean end of input.
pub(crate) fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return if saw_header {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "end of input inside a header",
                ))
            } else {
                Ok(None)
            };
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if saw_header {
                break;
            }
            continue;
        }
        saw_header = true;
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad Content-Length: {line}"),
                )
            })?);
        }
    }
    let length = length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "message without Content-Length")
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad JSON body: {e}")))
}

/// Write one message.
pub(crate) fn write_message(writer: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Standard base64 (RFC 4648, with padding), for `readMemory`.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, &serde_json::json!({"seq": 1, "x": "é"})).unwrap();
        write_message(&mut buf, &serde_json::json!({"seq": 2})).unwrap();
        let mut r = io::Cursor::new(buf);
        assert_eq!(read_message(&mut r).unwrap().unwrap()["x"], "é");
        assert_eq!(read_message(&mut r).unwrap().unwrap()["seq"], 2);
        assert!(read_message(&mut r).unwrap().is_none());
    }

    #[test]
    fn rejects_missing_length() {
        let mut r = io::Cursor::new(b"X-Other: 1\r\n\r\n{}".to_vec());
        assert!(read_message(&mut r).is_err());
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
