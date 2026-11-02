//! A scripted remote stub for transcript tests.
//!
//! A [`Script`] lists what the client must send and what the server answers, in order. The
//! [`ScriptedConnection`] built from it plays the server side faithfully: it checks checksums,
//! acknowledges packets with `+` until `QStartNoAckMode` succeeds, resends its last reply on `-`,
//! and fails the client's next I/O call (with the reason) as soon as the client strays from the
//! script. [`ScriptLog::assert_finished`] then confirms the whole script ran.
//!
//! Transcripts can also be written as text, one packet per line:
//!
//! ```text
//! # comment
//! > qSupported:multiprocess+;swbreak+    (client sends)
//! < PacketSize=4000;QStartNoAckMode+     (server answers; several `<` lines send several)
//! >!                                     (client sends the interrupt byte)
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use crate::conn::Connection;
use crate::packet::{Deframer, Incoming, frame};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    Packet(Vec<u8>),
    Interrupt,
}

#[derive(Clone, Debug)]
struct Step {
    expect: Expect,
    /// Raw wire bytes to send back, one entry per packet.
    replies: Vec<Vec<u8>>,
}

/// The ordered exchange a test expects.
#[derive(Clone, Debug, Default)]
pub struct Script {
    steps: Vec<Step>,
}

impl Script {
    pub fn new() -> Self {
        Script::default()
    }

    /// The client sends `client`; the server answers with the packet `server`.
    pub fn exchange(self, client: impl AsRef<[u8]>, server: impl AsRef<[u8]>) -> Self {
        self.exchange_many(client, &[server.as_ref()])
    }

    /// The client sends `client`; the server answers with several packets in a row (console
    /// output followed by a stop reply, for example).
    pub fn exchange_many(mut self, client: impl AsRef<[u8]>, servers: &[&[u8]]) -> Self {
        self.steps.push(Step {
            expect: Expect::Packet(client.as_ref().to_vec()),
            replies: servers.iter().map(|s| frame(s)).collect(),
        });
        self
    }

    /// The client sends `client`; the server answers with these exact bytes (to test corrupt
    /// checksums and the like).
    pub fn exchange_raw(mut self, client: impl AsRef<[u8]>, raw: impl AsRef<[u8]>) -> Self {
        self.steps.push(Step {
            expect: Expect::Packet(client.as_ref().to_vec()),
            replies: vec![raw.as_ref().to_vec()],
        });
        self
    }

    /// The client sends `client` and the server sends nothing back.
    pub fn silent(mut self, client: impl AsRef<[u8]>) -> Self {
        self.steps.push(Step {
            expect: Expect::Packet(client.as_ref().to_vec()),
            replies: vec![],
        });
        self
    }

    /// The client sends the interrupt byte; the server answers with `server`.
    pub fn interrupt(mut self, server: impl AsRef<[u8]>) -> Self {
        self.steps.push(Step {
            expect: Expect::Interrupt,
            replies: vec![frame(server.as_ref())],
        });
        self
    }

    /// Append another script.
    pub fn then(mut self, other: Script) -> Self {
        self.steps.extend(other.steps);
        self
    }

    /// Parse the text form described in the module docs.
    pub fn parse(text: &str) -> Result<Script, String> {
        let mut steps: Vec<Step> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if line == ">!" {
                steps.push(Step {
                    expect: Expect::Interrupt,
                    replies: vec![],
                });
            } else if let Some(p) = line.strip_prefix("> ") {
                steps.push(Step {
                    expect: Expect::Packet(p.as_bytes().to_vec()),
                    replies: vec![],
                });
            } else if line == ">" {
                steps.push(Step {
                    expect: Expect::Packet(Vec::new()),
                    replies: vec![],
                });
            } else if let Some(p) =
                line.strip_prefix("< ")
                    .or(if line == "<" { Some("") } else { None })
            {
                let last = steps
                    .last_mut()
                    .ok_or_else(|| format!("line {}: a reply before any request", n + 1))?;
                last.replies.push(frame(p.as_bytes()));
            } else {
                return Err(format!(
                    "line {}: expected '> ', '< ' or '>!': {line:?}",
                    n + 1
                ));
            }
        }
        Ok(Script { steps })
    }
}

#[derive(Debug)]
struct State {
    steps: VecDeque<Step>,
    deframer: Deframer,
    outgoing: VecDeque<u8>,
    ack_mode: bool,
    last_reply: Vec<u8>,
    failure: Option<String>,
    exchanges_done: usize,
}

impl State {
    fn fail(&mut self, msg: String) {
        if self.failure.is_none() {
            self.failure = Some(msg);
        }
    }

    fn take_input(&mut self, bytes: &[u8]) {
        self.deframer.push(bytes);
        loop {
            let item = match self.deframer.next_item() {
                Ok(Some(i)) => i,
                Ok(None) => return,
                Err(e) => {
                    self.fail(format!("client sent a malformed frame: {e}"));
                    return;
                }
            };
            let got = match item {
                Incoming::Ack => continue,
                Incoming::Nack => {
                    let again = self.last_reply.clone();
                    self.outgoing.extend(again);
                    continue;
                }
                Incoming::Corrupt { payload, .. } => {
                    self.fail(format!(
                        "client sent a bad checksum on {:?}",
                        String::from_utf8_lossy(&payload)
                    ));
                    return;
                }
                Incoming::Notification(_) => {
                    self.fail("client sent a notification".into());
                    return;
                }
                Incoming::Packet(p) => Expect::Packet(p),
                Incoming::Interrupt => Expect::Interrupt,
            };
            let Some(step) = self.steps.pop_front() else {
                self.fail(format!(
                    "client sent {} after the script ended",
                    describe(&got)
                ));
                return;
            };
            if step.expect != got {
                self.fail(format!(
                    "exchange {}: client sent {}, script expected {}",
                    self.exchanges_done + 1,
                    describe(&got),
                    describe(&step.expect)
                ));
                return;
            }
            self.exchanges_done += 1;
            if self.ack_mode && got != Expect::Interrupt {
                self.outgoing.push_back(b'+');
            }
            let mut sent = Vec::new();
            for r in &step.replies {
                sent.extend_from_slice(r);
            }
            self.outgoing.extend(sent.iter().copied());
            self.last_reply = sent;
            if got == Expect::Packet(b"QStartNoAckMode".to_vec())
                && step.replies.first().map(Vec::as_slice) == Some(frame(b"OK").as_slice())
            {
                self.ack_mode = false;
            }
        }
    }
}

fn describe(e: &Expect) -> String {
    match e {
        Expect::Packet(p) => format!("{:?}", String::from_utf8_lossy(p)),
        Expect::Interrupt => "the interrupt byte".into(),
    }
}

/// The server end of a [`Script`], as a [`Connection`].
#[derive(Clone)]
pub struct ScriptedConnection {
    state: Arc<Mutex<State>>,
}

impl fmt::Debug for ScriptedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ScriptedConnection")
    }
}

/// Inspects a [`ScriptedConnection`] after the client is done with it.
#[derive(Clone, Debug)]
pub struct ScriptLog {
    state: Arc<Mutex<State>>,
}

impl ScriptLog {
    /// The first deviation from the script, if any.
    pub fn failure(&self) -> Option<String> {
        self.state.lock().expect("script state").failure.clone()
    }

    /// Panic unless every scripted exchange happened and nothing went wrong.
    pub fn assert_finished(&self) {
        let st = self.state.lock().expect("script state");
        if let Some(f) = &st.failure {
            panic!("transcript failed: {f}");
        }
        if let Some(next) = st.steps.front() {
            panic!(
                "transcript unfinished after {} exchanges: next expected {}",
                st.exchanges_done,
                describe(&next.expect)
            );
        }
    }
}

impl ScriptedConnection {
    pub fn new(script: Script) -> (ScriptedConnection, ScriptLog) {
        let state = Arc::new(Mutex::new(State {
            steps: script.steps.into(),
            deframer: Deframer::default(),
            outgoing: VecDeque::new(),
            ack_mode: true,
            last_reply: Vec::new(),
            failure: None,
            exchanges_done: 0,
        }));
        (
            ScriptedConnection {
                state: state.clone(),
            },
            ScriptLog { state },
        )
    }
}

fn script_error(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("transcript: {msg}"))
}

impl Read for ScriptedConnection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut st = self.state.lock().expect("script state");
        if let Some(f) = &st.failure {
            return Err(script_error(f));
        }
        if st.outgoing.is_empty() {
            let msg = "the client waits for a reply the script does not have".to_string();
            st.fail(msg.clone());
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg));
        }
        let n = buf.len().min(st.outgoing.len());
        for (slot, b) in buf.iter_mut().zip(st.outgoing.drain(..n)) {
            *slot = b;
        }
        Ok(n)
    }
}

impl Write for ScriptedConnection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut st = self.state.lock().expect("script state");
        if let Some(f) = &st.failure {
            return Err(script_error(f));
        }
        st.take_input(buf);
        if let Some(f) = &st.failure {
            return Err(script_error(f));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Connection for ScriptedConnection {
    fn interrupt_writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plays_a_transcript_with_acks() {
        let (mut c, log) = ScriptedConnection::new(
            Script::new()
                .exchange("QStartNoAckMode", "OK")
                .exchange("?", "S05"),
        );
        c.write_all(&frame(b"QStartNoAckMode")).unwrap();
        let mut buf = [0u8; 64];
        let n = c.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"+$OK#9a");
        c.write_all(b"+").unwrap();
        c.write_all(&frame(b"?")).unwrap();
        let n = c.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"$S05#b8");
        log.assert_finished();
    }

    #[test]
    fn reports_deviation() {
        let (mut c, log) = ScriptedConnection::new(Script::new().exchange("g", "00"));
        assert!(c.write_all(&frame(b"m0,1")).is_err());
        assert!(log.failure().unwrap().contains("script expected \"g\""));
    }

    #[test]
    fn parses_text_transcripts() {
        let s = Script::parse("# hello\n> qC\n< QC1\n>!\n< T02\n< W00\n>\n").unwrap();
        assert_eq!(s.steps.len(), 3);
        assert_eq!(s.steps[1].expect, Expect::Interrupt);
        assert_eq!(s.steps[1].replies.len(), 2);
        assert_eq!(s.steps[2].expect, Expect::Packet(vec![]));
        assert!(Script::parse("< OK").is_err());
        assert!(Script::parse("? x").is_err());
    }
}
