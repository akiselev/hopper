//! The blocking client.

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use hopper_core::target::BreakKind;
use hopper_core::{HopperError, RegisterSet, Result, Tid};

use crate::conn::Connection;
use crate::packet::{
    Deframer, INTERRUPT, Incoming, expand_rle, frame, from_hex, to_hex, unescape_binary,
};
use crate::stop::StopReply;
use crate::tdesc::{RegisterDesc, TargetDescription};
use crate::thread::{ThreadId, ThreadSelector};
use crate::xml::elements;

/// The features hopper asks for in `qSupported`: thread ids with pids, the precise breakpoint
/// stop reasons, the x86 register descriptions and `vCont?`.
pub const CLIENT_FEATURES: &str =
    "multiprocess+;swbreak+;hwbreak+;vContSupported+;xmlRegisters=i386";

/// The packet size gdb assumes before `qSupported` says otherwise.
const DEFAULT_PACKET_SIZE: usize = 400;
/// Room for framing and a reply prefix when sizing chunks from the packet size.
const PACKET_OVERHEAD: usize = 32;
/// Resends of one packet the server rejects with `-` before giving up.
const MAX_RESENDS: u32 = 5;

/// One entry of a `qSupported` reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureValue {
    Supported,
    Unsupported,
    /// `name?`: may be supported; probe it.
    Probe,
    Value(String),
}

/// The server's `qSupported` reply.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ServerFeatures {
    pub entries: Vec<(String, FeatureValue)>,
}

impl ServerFeatures {
    pub fn parse(reply: &str) -> ServerFeatures {
        let entries = reply
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| {
                if let Some((k, v)) = e.split_once('=') {
                    (k.to_string(), FeatureValue::Value(v.to_string()))
                } else if let Some(k) = e.strip_suffix('+') {
                    (k.to_string(), FeatureValue::Supported)
                } else if let Some(k) = e.strip_suffix('-') {
                    (k.to_string(), FeatureValue::Unsupported)
                } else if let Some(k) = e.strip_suffix('?') {
                    (k.to_string(), FeatureValue::Probe)
                } else {
                    (e.to_string(), FeatureValue::Supported)
                }
            })
            .collect();
        ServerFeatures { entries }
    }

    pub fn get(&self, name: &str) -> Option<&FeatureValue> {
        self.entries.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn supports(&self, name: &str) -> bool {
        matches!(self.get(name), Some(FeatureValue::Supported))
    }

    /// `PacketSize`, in bytes.
    pub fn packet_size(&self) -> usize {
        match self.get("PacketSize") {
            Some(FeatureValue::Value(v)) => {
                usize::from_str_radix(v, 16).unwrap_or(DEFAULT_PACKET_SIZE)
            }
            _ => DEFAULT_PACKET_SIZE,
        }
    }
}

/// Which `H` selection to make.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThreadOp {
    /// `Hg`: the thread later register and memory requests apply to.
    General,
    /// `Hc`: the thread `s`, `c`, `bs` and `bc` resume.
    Continue,
}

/// How to resume the target.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResumeRequest {
    /// `vCont;c` (or `c` when the server has no `vCont`).
    Continue,
    /// `vCont;s:<thread>` (or `Hc<thread>` then `s`).
    Step(ThreadId),
    /// `bc`.
    ReverseContinue,
    /// `Hc<thread>` then `bs`.
    ReverseStep(ThreadId),
}

/// The resume actions `vCont?` lists.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct VContActions {
    pub actions: Vec<String>,
}

impl VContActions {
    pub fn has(&self, action: &str) -> bool {
        self.actions.iter().any(|a| a == action)
    }
}

/// A thread from `qXfer:threads:read`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub name: Option<String>,
    pub core: Option<u32>,
}

/// The vRun forms rr's server accepts (`GdbServerConnection::process_vpacket`): restart from the
/// previous restart point, run to an event, to a tick count of the current thread, or restore a
/// checkpoint made with `qRRCmd:checkpoint`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RrSeek {
    Previous,
    Event(u64),
    Ticks(u64),
    Checkpoint(u64),
}

impl RrSeek {
    /// The packet: `vRun;` for the previous point, else `vRun;;<hex of the decimal argument>`
    /// (the empty first argument is the program name rr refuses to change).
    pub fn packet(&self) -> String {
        let arg = match self {
            RrSeek::Previous => return "vRun;".to_string(),
            RrSeek::Event(e) => e.to_string(),
            RrSeek::Ticks(t) => format!("t{t}"),
            RrSeek::Checkpoint(c) => format!("c{c}"),
        };
        format!("vRun;;{}", to_hex(arg.as_bytes()))
    }
}

/// Sends the interrupt byte from another thread while a resume request blocks.
#[derive(Clone)]
pub struct InterruptHandle {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl fmt::Debug for InterruptHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InterruptHandle")
    }
}

impl InterruptHandle {
    pub fn interrupt(&self) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| HopperError::Disconnected("interrupt writer poisoned".into()))?;
        w.write_all(&[INTERRUPT]).map_err(io_error)?;
        w.flush().map_err(io_error)
    }
}

fn io_error(e: io::Error) -> HopperError {
    match e.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::NotConnected => HopperError::Disconnected(e.to_string()),
        io::ErrorKind::InvalidData => HopperError::Protocol(e.to_string()),
        _ => HopperError::Io(e.to_string()),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Turn an error reply (`Enn`, `E.text`) or an empty reply into the matching error.
fn check_reply(request: &str, reply: &[u8]) -> Result<()> {
    if reply.is_empty() {
        return Err(HopperError::Unsupported(format!(
            "the server does not handle {request}"
        )));
    }
    let is_error = reply[0] == b'E'
        && (reply.get(1) == Some(&b'.')
            || (reply.len() == 3 && reply[1..].iter().all(u8::is_ascii_hexdigit)));
    if is_error {
        return Err(HopperError::TargetRefused {
            request: request.to_string(),
            reply: lossy(reply),
        });
    }
    Ok(())
}

fn short(request: &[u8]) -> String {
    let s = lossy(request);
    if s.len() > 48 {
        format!("{}...", &s[..48])
    } else {
        s
    }
}

/// A blocking GDB remote serial protocol client over one connection.
pub struct RspClient<C: Connection> {
    conn: C,
    deframer: Deframer,
    ack_mode: bool,
    features: ServerFeatures,
    tdesc: Option<TargetDescription>,
    vcont: Option<VContActions>,
    /// Console output (`O` packets) received while waiting for stop replies.
    console: Vec<u8>,
    transcript: Option<Vec<String>>,
}

impl<C: Connection> fmt::Debug for RspClient<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RspClient")
            .field("conn", &self.conn)
            .field("ack_mode", &self.ack_mode)
            .field("packet_size", &self.features.packet_size())
            .finish()
    }
}

impl<C: Connection> RspClient<C> {
    /// Wrap a connection without any handshake. [`RspClient::connect`] is the usual entry point.
    pub fn new(conn: C) -> Self {
        RspClient {
            conn,
            deframer: Deframer::default(),
            ack_mode: true,
            features: ServerFeatures::default(),
            tdesc: None,
            vcont: None,
            console: Vec::new(),
            transcript: None,
        }
    }

    /// Connect: acknowledge, negotiate `qSupported` and switch to no-ack mode when offered.
    pub fn connect(conn: C) -> Result<Self> {
        let mut c = RspClient::new(conn);
        c.handshake()?;
        Ok(c)
    }

    fn handshake(&mut self) -> Result<()> {
        self.write_raw(b"+")?;
        let reply = self.request(format!("qSupported:{CLIENT_FEATURES}").as_bytes())?;
        check_reply("qSupported", &reply)?;
        self.features = ServerFeatures::parse(&lossy(&reply));
        if self.features.supports("QStartNoAckMode") {
            self.expect_ok("QStartNoAckMode")?;
            self.ack_mode = false;
        }
        Ok(())
    }

    pub fn features(&self) -> &ServerFeatures {
        &self.features
    }

    pub fn is_ack_mode(&self) -> bool {
        self.ack_mode
    }

    pub fn connection(&self) -> &C {
        &self.conn
    }

    /// Start recording every packet sent and received, in [`crate::script::Script::parse`]'s
    /// text form.
    pub fn record_transcript(&mut self) {
        self.transcript = Some(Vec::new());
    }

    /// The recorded transcript so far; recording continues.
    pub fn take_transcript(&mut self) -> String {
        match &mut self.transcript {
            Some(lines) => {
                let mut s = lines.join("\n");
                s.push('\n');
                lines.clear();
                s
            }
            None => String::new(),
        }
    }

    /// Console output the server sent with `O` packets since the last call.
    pub fn take_console(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.console)
    }

    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        let writer = self.conn.interrupt_writer().map_err(io_error)?;
        Ok(InterruptHandle {
            writer: Arc::new(Mutex::new(writer)),
        })
    }

    // ------------------------------------------------------------------ framing

    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.conn.write_all(bytes).map_err(io_error)?;
        self.conn.flush().map_err(io_error)
    }

    fn read_item(&mut self) -> Result<Incoming> {
        loop {
            if let Some(item) = self.deframer.next_item()? {
                return Ok(item);
            }
            let mut buf = [0u8; 65536];
            let n = self.conn.read(&mut buf).map_err(io_error)?;
            if n == 0 {
                return Err(HopperError::Disconnected(
                    "the server closed the connection".into(),
                ));
            }
            self.deframer.push(&buf[..n]);
        }
    }

    /// Send one packet (payload already escaped where binary).
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        if let Some(t) = &mut self.transcript {
            t.push(format!("> {}", lossy(payload)));
        }
        let framed = frame(payload);
        self.write_raw(&framed)?;
        if !self.ack_mode {
            return Ok(());
        }
        let mut resends = 0;
        loop {
            match self.read_item()? {
                Incoming::Ack => return Ok(()),
                Incoming::Nack => {
                    resends += 1;
                    if resends > MAX_RESENDS {
                        return Err(HopperError::Protocol(format!(
                            "the server rejected {} {MAX_RESENDS} times",
                            short(payload)
                        )));
                    }
                    self.write_raw(&framed)?;
                }
                Incoming::Notification(_) => {}
                other => {
                    return Err(HopperError::Protocol(format!(
                        "expected an ack for {}, got {other:?}",
                        short(payload)
                    )));
                }
            }
        }
    }

    /// Receive one reply packet, run-length expanded (binary data still escaped).
    pub fn receive(&mut self) -> Result<Vec<u8>> {
        loop {
            match self.read_item()? {
                Incoming::Ack | Incoming::Notification(_) => {}
                Incoming::Nack => {
                    return Err(HopperError::Protocol(
                        "unexpected nack while waiting for a reply".into(),
                    ));
                }
                Incoming::Interrupt => {}
                Incoming::Corrupt {
                    payload,
                    sent,
                    computed,
                } => {
                    if self.ack_mode {
                        self.write_raw(b"-")?;
                    } else {
                        return Err(HopperError::Protocol(format!(
                            "bad checksum {sent:02x} (computed {computed:02x}) on {}",
                            short(&payload)
                        )));
                    }
                }
                Incoming::Packet(p) => {
                    if self.ack_mode {
                        self.write_raw(b"+")?;
                    }
                    if let Some(t) = &mut self.transcript {
                        t.push(format!("< {}", lossy(&p)));
                    }
                    return expand_rle(&p);
                }
            }
        }
    }

    /// Send a request and return its reply.
    pub fn request(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        self.send(payload)?;
        self.receive()
    }

    fn expect_ok(&mut self, request: &str) -> Result<()> {
        let reply = self.request(request.as_bytes())?;
        check_reply(request, &reply)?;
        if reply != b"OK" {
            return Err(HopperError::Protocol(format!(
                "{request}: expected OK, got {:?}",
                lossy(&reply)
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ stops and threads

    /// Wait for a stop reply, collecting `O` console packets on the way.
    pub fn wait_stop(&mut self) -> Result<StopReply> {
        loop {
            let reply = self.receive()?;
            if reply.first() == Some(&b'O')
                && reply.len() > 1
                && !reply.len().is_multiple_of(2)
                && let Ok(text) = from_hex(&reply[1..])
            {
                self.console.extend(text);
                continue;
            }
            if reply.first() == Some(&b'E') {
                check_reply("resume", &reply)?;
            }
            return StopReply::parse(&reply);
        }
    }

    /// `QThreadEvents:1`: report thread creation (`create:` stops) and thread exits (`w`
    /// replies) from now on. `Unsupported` when the server does not know the packet (rr).
    pub fn enable_thread_events(&mut self) -> Result<()> {
        self.expect_ok("QThreadEvents:1")
    }

    /// `?`: why the target last stopped.
    pub fn stop_reason(&mut self) -> Result<StopReply> {
        self.send(b"?")?;
        self.wait_stop()
    }

    /// `Hg` / `Hc`.
    pub fn select_thread(&mut self, op: ThreadOp, thread: ThreadSelector) -> Result<()> {
        let letter = match op {
            ThreadOp::General => 'g',
            ThreadOp::Continue => 'c',
        };
        self.expect_ok(&format!("H{letter}{thread}"))
    }

    /// `qC`.
    pub fn current_thread(&mut self) -> Result<ThreadId> {
        let reply = self.request(b"qC")?;
        check_reply("qC", &reply)?;
        let text = lossy(&reply);
        let id = text
            .strip_prefix("QC")
            .ok_or_else(|| HopperError::Protocol(format!("bad qC reply {text:?}")))?;
        ThreadId::parse(id)
    }

    /// `qfThreadInfo` / `qsThreadInfo`.
    pub fn thread_list(&mut self) -> Result<Vec<ThreadId>> {
        let mut out = Vec::new();
        let mut req: &[u8] = b"qfThreadInfo";
        loop {
            let reply = self.request(req)?;
            check_reply(&lossy(req), &reply)?;
            match reply.first() {
                Some(b'l') => return Ok(out),
                Some(b'm') => {
                    for id in lossy(&reply[1..]).split(',').filter(|s| !s.is_empty()) {
                        out.push(ThreadId::parse(id)?);
                    }
                }
                _ => {
                    return Err(HopperError::Protocol(format!(
                        "bad thread list reply {:?}",
                        lossy(&reply)
                    )));
                }
            }
            req = b"qsThreadInfo";
        }
    }

    /// `qXfer:threads:read`.
    pub fn thread_info(&mut self) -> Result<Vec<ThreadInfo>> {
        let doc = self.xfer_read("threads", "")?;
        let doc = String::from_utf8(doc)
            .map_err(|_| HopperError::Protocol("thread list is not UTF-8".into()))?;
        let mut out = Vec::new();
        for el in elements(&doc)? {
            if el.name != "thread" {
                continue;
            }
            let id = el
                .attr("id")
                .ok_or_else(|| HopperError::Protocol("<thread> without id".into()))?;
            out.push(ThreadInfo {
                id: ThreadId::parse(id)?,
                name: el.attr("name").map(str::to_string),
                core: el.attr("core").and_then(|c| c.parse().ok()),
            });
        }
        Ok(out)
    }

    /// `vCont?`, cached. A server without `vCont` yields an empty list.
    pub fn vcont_actions(&mut self) -> Result<VContActions> {
        if let Some(v) = &self.vcont {
            return Ok(v.clone());
        }
        let reply = self.request(b"vCont?")?;
        let actions = match lossy(&reply).strip_prefix("vCont") {
            Some(rest) => VContActions {
                actions: rest
                    .split(';')
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
                    .collect(),
            },
            None => VContActions::default(),
        };
        self.vcont = Some(actions.clone());
        Ok(actions)
    }

    /// Resume and wait for the stop.
    pub fn resume(&mut self, req: ResumeRequest) -> Result<StopReply> {
        self.start_resume(req)?;
        self.wait_stop()
    }

    /// Send the resume packet without waiting. Pair with [`RspClient::wait_stop`]; another
    /// thread may call [`InterruptHandle::interrupt`] in between.
    pub fn start_resume(&mut self, req: ResumeRequest) -> Result<()> {
        match req {
            ResumeRequest::Continue => {
                if self.vcont_actions()?.has("c") {
                    self.send(b"vCont;c")
                } else {
                    self.send(b"c")
                }
            }
            ResumeRequest::Step(t) => {
                if self.vcont_actions()?.has("s") {
                    self.send(format!("vCont;s:{t}").as_bytes())
                } else {
                    self.select_thread(ThreadOp::Continue, ThreadSelector::Thread(t))?;
                    self.send(b"s")
                }
            }
            ResumeRequest::ReverseContinue => {
                if !self.features.supports("ReverseContinue") {
                    return Err(HopperError::Unsupported("reverse continue".into()));
                }
                self.send(b"bc")
            }
            ResumeRequest::ReverseStep(t) => {
                if !self.features.supports("ReverseStep") {
                    return Err(HopperError::Unsupported("reverse step".into()));
                }
                self.select_thread(ThreadOp::Continue, ThreadSelector::Thread(t))?;
                self.send(b"bs")
            }
        }
    }

    // ------------------------------------------------------------------ registers

    /// The target description from `qXfer:features:read:target.xml`, with its includes, cached.
    pub fn target_description(&mut self) -> Result<TargetDescription> {
        if let Some(t) = &self.tdesc {
            return Ok(t.clone());
        }
        let root = self.xfer_read_text("features", "target.xml")?;
        let tdesc = TargetDescription::parse_with_includes(&root, &mut |href| {
            self.xfer_read_text("features", href)
        })?;
        self.tdesc = Some(tdesc.clone());
        Ok(tdesc)
    }

    /// `g` for the selected general thread, decoded with the target description. Modelled
    /// registers the `g` reply does not cover are fetched with `p` (when the server has `p`).
    pub fn read_registers(&mut self) -> Result<RegisterSet> {
        let tdesc = self.target_description()?;
        let reply = self.request(b"g")?;
        check_reply("g", &reply)?;
        let mut set = tdesc.decode_g(&reply)?;
        let g_len = set.raw.len();
        for d in tdesc.beyond_g(g_len).cloned().collect::<Vec<_>>() {
            match self.read_register(&d) {
                Ok(Some(bytes)) => {
                    if let Some(reg) = d.reg {
                        set.set_bytes(reg, bytes);
                    }
                }
                Ok(None) => {}
                Err(HopperError::Unsupported(_)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(set)
    }

    /// `p<regnum>`: one register's bytes, or `None` when the server says it is unavailable.
    pub fn read_register(&mut self, desc: &RegisterDesc) -> Result<Option<Vec<u8>>> {
        let req = format!("p{:x}", desc.regnum);
        let reply = self.request(req.as_bytes())?;
        check_reply(&req, &reply)?;
        if reply.iter().all(|b| *b == b'x') {
            return Ok(None);
        }
        let bytes = from_hex(&reply)?;
        if bytes.len() != desc.size() {
            return Err(HopperError::Protocol(format!(
                "{req}: {} bytes for {}-bit register {}",
                bytes.len(),
                desc.bitsize,
                desc.name
            )));
        }
        Ok(Some(bytes))
    }

    // ------------------------------------------------------------------ memory

    fn chunk_len(&self) -> usize {
        (self.features.packet_size().saturating_sub(PACKET_OVERHEAD) / 2).max(1)
    }

    fn uses_binary_memory(&self) -> bool {
        self.features.supports("binary-upload")
    }

    /// Fill `buf` from `addr`, in packet-sized chunks with `m` (or `x` when the server offers
    /// `binary-upload+`). A short reply continues from where it stopped; an error before every
    /// byte is read is [`HopperError::UnreadableMemory`] for the remainder.
    pub fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        let chunk = self.chunk_len();
        while done < buf.len() {
            let want = (buf.len() - done).min(chunk);
            let at = addr.wrapping_add(done as u64);
            let unreadable = || HopperError::UnreadableMemory {
                addr: at,
                len: (buf.len() - done) as u64,
            };
            let bytes = if self.uses_binary_memory() {
                let req = format!("x{at:x},{want:x}");
                let reply = self.request(req.as_bytes())?;
                if reply.first() == Some(&b'E') {
                    return Err(unreadable());
                }
                check_reply(&req, &reply)?;
                let body = reply.strip_prefix(b"b").unwrap_or(&reply);
                unescape_binary(body)?
            } else {
                let req = format!("m{at:x},{want:x}");
                let reply = self.request(req.as_bytes())?;
                if reply.first() == Some(&b'E') || reply.is_empty() {
                    return Err(unreadable());
                }
                from_hex(&reply)?
            };
            if bytes.is_empty() {
                return Err(unreadable());
            }
            if bytes.len() > want {
                return Err(HopperError::Protocol(format!(
                    "asked for {want} bytes at {at:#x}, got {}",
                    bytes.len()
                )));
            }
            buf[done..done + bytes.len()].copy_from_slice(&bytes);
            done += bytes.len();
        }
        Ok(())
    }

    // ------------------------------------------------------------------ breakpoints

    fn z_type(kind: BreakKind) -> u8 {
        match kind {
            BreakKind::Software => 0,
            BreakKind::Hardware => 1,
            BreakKind::WriteWatch => 2,
            BreakKind::ReadWatch => 3,
            BreakKind::AccessWatch => 4,
        }
    }

    /// `Z<type>,<addr>,<kind>`. For code breakpoints `len` is the instruction-size hint (1 on
    /// x86); for watchpoints it is the watched length.
    pub fn insert_breakpoint(&mut self, kind: BreakKind, addr: u64, len: u64) -> Result<()> {
        self.expect_ok(&format!("Z{},{addr:x},{len:x}", Self::z_type(kind)))
    }

    /// `z<type>,<addr>,<kind>`.
    pub fn remove_breakpoint(&mut self, kind: BreakKind, addr: u64, len: u64) -> Result<()> {
        self.expect_ok(&format!("z{},{addr:x},{len:x}", Self::z_type(kind)))
    }

    // ------------------------------------------------------------------ qXfer

    /// Read a whole `qXfer` object (`features`, `auxv`, `exec-file`, `threads`, ...) in chunks.
    pub fn xfer_read(&mut self, object: &str, annex: &str) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let len = self.chunk_len();
        loop {
            let req = format!("qXfer:{object}:read:{annex}:{:x},{len:x}", out.len());
            let reply = self.request(req.as_bytes())?;
            check_reply(&req, &reply)?;
            let data = unescape_binary(&reply[1..])?;
            match reply[0] {
                b'l' => {
                    out.extend(data);
                    return Ok(out);
                }
                b'm' => {
                    if data.is_empty() {
                        return Err(HopperError::Protocol(format!("{req}: empty 'm' chunk")));
                    }
                    out.extend(data);
                }
                _ => {
                    return Err(HopperError::Protocol(format!(
                        "{req}: bad reply {:?}",
                        short(&reply)
                    )));
                }
            }
        }
    }

    fn xfer_read_text(&mut self, object: &str, annex: &str) -> Result<String> {
        String::from_utf8(self.xfer_read(object, annex)?)
            .map_err(|_| HopperError::Protocol(format!("qXfer:{object}:{annex} is not UTF-8")))
    }

    /// `qXfer:auxv:read`.
    pub fn auxv(&mut self) -> Result<Vec<u8>> {
        self.xfer_read("auxv", "")
    }

    /// `qXfer:exec-file:read` for `pid` (the current process when `None`).
    pub fn exec_file(&mut self, pid: Option<i32>) -> Result<String> {
        let annex = pid.map(|p| format!("{p:x}")).unwrap_or_default();
        self.xfer_read_text("exec-file", &annex)
    }

    // ------------------------------------------------------------------ rr extensions

    /// `vRun` as rr reads it: seek, and return the stop reply rr sends when it gets there.
    pub fn rr_seek(&mut self, to: RrSeek) -> Result<StopReply> {
        let packet = to.packet();
        self.send(packet.as_bytes())?;
        let reply = self.receive()?;
        check_reply(&packet, &reply)?;
        StopReply::parse(&reply)
    }

    /// `qRRCmd:<name>:<tid>[:<hex arg>]*`, as rr's own gdb macros send it
    /// (`DebuggerExtensionCommandHandler.cc`): the name in plain text, the target thread in
    /// decimal (`-1` for the current one), each argument hex-encoded. rr answers with the
    /// hex-encoded text plus a newline, which this strips.
    pub fn rr_cmd(&mut self, name: &str, tid: Option<Tid>, args: &[&str]) -> Result<String> {
        let mut req = format!("qRRCmd:{name}:{}", tid.map_or(-1, |t| t.0));
        for a in args {
            req.push(':');
            req.push_str(&to_hex(a.as_bytes()));
        }
        let reply = self.request(req.as_bytes())?;
        if reply.is_empty() {
            return Err(HopperError::Unsupported(format!(
                "{name}: not an rr server"
            )));
        }
        if reply == b"RRCmd_EndDiversion" {
            return Err(HopperError::TargetRefused {
                request: req,
                reply: "the command ended a diversion session".into(),
            });
        }
        let text = String::from_utf8(from_hex(&reply)?)
            .map_err(|_| HopperError::Protocol(format!("{name}: reply is not UTF-8")))?;
        let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
        if text.starts_with("Command '") && text.ends_with("' not found.") {
            return Err(HopperError::Unsupported(text));
        }
        Ok(text)
    }

    /// rr's `when`: the number of the last completely replayed event.
    pub fn rr_when(&mut self) -> Result<u64> {
        let t = self.rr_cmd("when", None, &[])?;
        parse_rr_number(&t, "Completed event: ")
    }

    /// rr's `when-ticks` for `tid` (the current thread when `None`).
    pub fn rr_when_ticks(&mut self, tid: Option<Tid>) -> Result<u64> {
        let t = self.rr_cmd("when-ticks", tid, &[])?;
        parse_rr_number(&t, "Current tick: ")
    }

    /// rr's `when-tid`: the real tid of the current thread.
    pub fn rr_when_tid(&mut self) -> Result<Tid> {
        let t = self.rr_cmd("when-tid", None, &[])?;
        let n = parse_rr_number(&t, "Current tid: ")?;
        i32::try_from(n)
            .map(Tid)
            .map_err(|_| HopperError::Protocol(format!("tid out of range: {t:?}")))
    }

    /// rr's `when-end`: the last event of the recording.
    pub fn rr_when_end(&mut self) -> Result<u64> {
        let t = self.rr_cmd("when-end", None, &[])?;
        parse_rr_number(&t, "Event at end of recording: ")
    }

    /// rr's `checkpoint`. `label` fills the `where` text rr's gdb macros take from `frame 0`
    /// (rr requires that argument). Returns the checkpoint number.
    pub fn rr_checkpoint(&mut self, label: &str) -> Result<u64> {
        let t = self.rr_cmd("checkpoint", None, &[label])?;
        let rest = t
            .strip_prefix("Checkpoint ")
            .ok_or_else(|| HopperError::TargetRefused {
                request: "qRRCmd:checkpoint".into(),
                reply: t.clone(),
            })?;
        let num = rest.split(' ').next().unwrap_or_default();
        num.parse()
            .map_err(|_| HopperError::Protocol(format!("bad checkpoint reply {t:?}")))
    }

    /// rr's `delete checkpoint`.
    pub fn rr_delete_checkpoint(&mut self, id: u64) -> Result<()> {
        let t = self.rr_cmd("delete checkpoint", None, &[&id.to_string()])?;
        if t.starts_with("Deleted checkpoint ") {
            Ok(())
        } else {
            Err(HopperError::TargetRefused {
                request: "qRRCmd:delete checkpoint".into(),
                reply: t,
            })
        }
    }

    // ------------------------------------------------------------------ ending

    /// `vKill;<pid>`.
    pub fn kill(&mut self, pid: i32) -> Result<()> {
        self.expect_ok(&format!("vKill;{pid:x}"))
    }

    /// `D`.
    pub fn detach(&mut self) -> Result<()> {
        self.expect_ok("D")
    }
}

/// Parse `<prefix><decimal>` from an rr command reply.
pub fn parse_rr_number(text: &str, prefix: &str) -> Result<u64> {
    text.strip_prefix(prefix)
        .and_then(|n| n.trim().parse().ok())
        .ok_or_else(|| {
            HopperError::Protocol(format!("expected {prefix:?} and a number, got {text:?}"))
        })
}
