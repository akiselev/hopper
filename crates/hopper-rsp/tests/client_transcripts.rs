//! The client against scripted servers, one operation per test.

use hopper_core::target::BreakKind;
use hopper_core::{HopperError, Reg, Tid};
use hopper_rsp::script::{Script, ScriptLog, ScriptedConnection};
use hopper_rsp::{
    ResumeRequest, RrSeek, RspClient, StopCause, StopReply, ThreadId, ThreadOp, ThreadSelector,
};

const QSUPPORTED: &str =
    "qSupported:multiprocess+;swbreak+;hwbreak+;vContSupported+;xmlRegisters=i386";

/// rr's own qSupported reply (GdbServerConnection.cc, `query`, "Supported").
const RR_SUPPORTED: &str = "PacketSize=100000;QStartNoAckMode+;qXfer:features:read+;\
qXfer:auxv:read+;qXfer:exec-file:read+;qXfer:siginfo:read+;qXfer:siginfo:write+;multiprocess+;\
hwbreak+;swbreak+;ConditionalBreakpoints+;vContSupported+;QPassSignals+;ReverseContinue+;\
ReverseStep+";

fn handshake(supported: &str) -> Script {
    Script::new().exchange(QSUPPORTED, supported).exchange("QStartNoAckMode", "OK")
}

fn client(script: Script) -> (RspClient<ScriptedConnection>, ScriptLog) {
    let (conn, log) = ScriptedConnection::new(script);
    let c = RspClient::connect(conn).expect("handshake");
    (c, log)
}

fn tid(pid: i32, t: i32) -> ThreadId {
    ThreadId::new(Some(pid), Tid(t))
}

#[test]
fn handshake_switches_to_no_ack_mode() {
    let (c, log) = client(handshake(RR_SUPPORTED));
    assert!(!c.is_ack_mode());
    assert_eq!(c.features().packet_size(), 0x100000);
    assert!(c.features().supports("ReverseContinue"));
    assert!(!c.features().supports("fork-events"));
    log.assert_finished();
}

#[test]
fn stays_in_ack_mode_when_not_offered() {
    let (mut c, log) = client(
        Script::new().exchange(QSUPPORTED, "PacketSize=20;qXfer:auxv:read+").exchange("qC", "QC1f"),
    );
    assert!(c.is_ack_mode());
    assert_eq!(c.features().packet_size(), 0x20);
    assert_eq!(c.current_thread().unwrap(), ThreadId::new(None, Tid(0x1f)));
    log.assert_finished();
}

#[test]
fn resends_on_nack_and_rejects_corrupt_reply_without_acks() {
    // In ack mode the server's `-` makes the client resend; the script resends its reply to a
    // client `-` too. Here: a corrupt reply followed by the good one; the client nacks the first.
    let (mut c, log) = client(
        Script::new()
            .exchange(QSUPPORTED, "PacketSize=400")
            .exchange_raw("qC", b"$QC1#00$QC1#c5".as_slice()),
    );
    assert_eq!(c.current_thread().unwrap(), ThreadId::new(None, Tid(1)));
    assert!(log.failure().is_none());

    let (mut c, _log) = client(handshake("PacketSize=400;QStartNoAckMode+").exchange_raw("qC", b"$QC1#00".as_slice()));
    match c.current_thread() {
        Err(HopperError::Protocol(m)) => assert!(m.contains("bad checksum"), "{m}"),
        other => panic!("{other:?}"),
    }
}

const CORE_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE feature SYSTEM "gdb-target.dtd">
<feature name="org.gnu.gdb.i386.core">
  <reg name="rax" bitsize="64" type="int64"/>
  <reg name="rbx" bitsize="64" type="int64"/>
  <reg name="rcx" bitsize="64" type="int64"/>
  <reg name="rdx" bitsize="64" type="int64"/>
  <reg name="rsi" bitsize="64" type="int64"/>
  <reg name="rdi" bitsize="64" type="int64"/>
  <reg name="rbp" bitsize="64" type="data_ptr"/>
  <reg name="rsp" bitsize="64" type="data_ptr"/>
  <reg name="r8" bitsize="64" type="int64"/>
  <reg name="r9" bitsize="64" type="int64"/>
  <reg name="r10" bitsize="64" type="int64"/>
  <reg name="r11" bitsize="64" type="int64"/>
  <reg name="r12" bitsize="64" type="int64"/>
  <reg name="r13" bitsize="64" type="int64"/>
  <reg name="r14" bitsize="64" type="int64"/>
  <reg name="r15" bitsize="64" type="int64"/>
  <reg name="rip" bitsize="64" type="code_ptr"/>
  <reg name="eflags" bitsize="32" type="i386_eflags"/>
</feature>"#;

const SEG_XML: &str = r#"<feature name="org.gnu.gdb.i386.segments">
  <reg name="fs_base" bitsize="64" type="data_ptr" regnum="18"/>
</feature>"#;

/// rr's target.xml shape: features pulled in with xi:include (TargetDescription.cc).
const RR_TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target>
<architecture>i386:x86-64</architecture>
<osabi>GNU/Linux</osabi>
<xi:include href="64bit-core.xml"/>
<xi:include href="64bit-seg.xml"/>
</target>"#;

fn xfer_whole(annex: &str, body: &str) -> (String, String) {
    (format!("qXfer:features:read:{annex}:0,7fff0"), format!("l{body}"))
}

fn tdesc_script() -> Script {
    let (q1, r1) = xfer_whole("target.xml", RR_TARGET_XML);
    let (q2, r2) = xfer_whole("64bit-core.xml", CORE_XML);
    let (q3, r3) = xfer_whole("64bit-seg.xml", SEG_XML);
    Script::new().exchange(q1, r1).exchange(q2, r2).exchange(q3, r3)
}

#[test]
fn reads_registers_from_included_description() {
    // 16 GPRs of zeros except rax=1 and rsp, rip, eflags, then fs_base; the zero runs are
    // run-length encoded the way gdbserver sends them ("0*}" is '0' plus 96 more).
    let mut g = String::new();
    g += "0100000000000000"; // rax
    g += &"0".repeat(16 * 6); // rbx..rbp
    g += "f0dfffffff7f0000"; // rsp
    g += &"0".repeat(16 * 8); // r8..r15
    g += "3412400000000000"; // rip
    g += "46020000"; // eflags
    // fs_base comes from `p12` because the g reply stops before it.
    let g_rle = g.replacen(&"0".repeat(97), "0*}", 1);
    assert!(g_rle.len() < g.len());
    let script = handshake(RR_SUPPORTED)
        .then(tdesc_script())
        .exchange("g", g_rle)
        .exchange("p12", "0070f7ffff7f0000");
    let (mut c, log) = client(script);
    let regs = c.read_registers().unwrap();
    assert_eq!(regs.get(Reg::RAX), Some(1));
    assert_eq!(regs.sp(), Some(0x7fffffffdff0));
    assert_eq!(regs.pc(), Some(0x401234));
    assert_eq!(regs.get(Reg::RFLAGS), Some(0x246));
    assert_eq!(regs.get(Reg::FS_BASE), Some(0x7fff_fff7_7000));
    assert_eq!(regs.raw.len(), 17 * 8 + 4);
    let tdesc = c.target_description().unwrap(); // cached: no packets
    assert_eq!(tdesc.by_name("fs_base").unwrap().offset, 17 * 8 + 4);
    log.assert_finished();
}

#[test]
fn reads_memory_in_packet_sized_chunks() {
    // PacketSize 0x48 = 72: chunks of (72 - 32) / 2 = 20 bytes.
    let data: Vec<u8> = (0..50u8).collect();
    let hex = |r: std::ops::Range<usize>| hopper_rsp::packet::to_hex(&data[r]);
    let script = handshake("PacketSize=48;QStartNoAckMode+")
        .exchange("m1000,14", hex(0..20))
        // A short reply: the client continues where it stopped.
        .exchange("m1014,14", hex(20..30))
        .exchange("m101e,14", hex(30..50))
        .exchange("m2000,8", "E14");
    let (mut c, log) = client(script);
    let mut buf = vec![0u8; 50];
    c.read_memory(0x1000, &mut buf).unwrap();
    assert_eq!(buf, data);
    let mut small = [0u8; 8];
    assert_eq!(
        c.read_memory(0x2000, &mut small),
        Err(HopperError::UnreadableMemory { addr: 0x2000, len: 8 })
    );
    log.assert_finished();
}

#[test]
fn reads_binary_memory_when_offered() {
    let data = [b'#', 0x00, b'}', b'*', b'$', 0x41];
    let mut reply = b"b".to_vec();
    reply.extend(hopper_rsp::packet::escape_binary(&data));
    let script = handshake("PacketSize=4000;QStartNoAckMode+;binary-upload+")
        .exchange("x400000,6", reply)
        .exchange("x500000,1", "E01");
    let (mut c, log) = client(script);
    let mut buf = [0u8; 6];
    c.read_memory(0x400000, &mut buf).unwrap();
    assert_eq!(buf, data);
    let mut one = [0u8; 1];
    assert!(matches!(c.read_memory(0x500000, &mut one), Err(HopperError::UnreadableMemory { .. })));
    log.assert_finished();
}

#[test]
fn sets_and_clears_breakpoints_and_watchpoints() {
    let script = handshake(RR_SUPPORTED)
        .exchange("Z0,401000,1", "OK")
        .exchange("Z1,401004,1", "OK")
        .exchange("Z2,7ffd0000,8", "OK")
        .exchange("Z3,7ffd0010,4", "OK")
        .exchange("Z4,7ffd0020,1", "OK")
        .exchange("Z2,7ffd0040,8", "E01")
        .exchange("z0,401000,1", "OK")
        .exchange("z2,7ffd0000,8", "OK")
        .exchange("Z1,0,1", "");
    let (mut c, log) = client(script);
    c.insert_breakpoint(BreakKind::Software, 0x401000, 1).unwrap();
    c.insert_breakpoint(BreakKind::Hardware, 0x401004, 1).unwrap();
    c.insert_breakpoint(BreakKind::WriteWatch, 0x7ffd0000, 8).unwrap();
    c.insert_breakpoint(BreakKind::ReadWatch, 0x7ffd0010, 4).unwrap();
    c.insert_breakpoint(BreakKind::AccessWatch, 0x7ffd0020, 1).unwrap();
    // A fifth watchpoint: rr runs out of debug registers and refuses.
    match c.insert_breakpoint(BreakKind::WriteWatch, 0x7ffd0040, 8) {
        Err(HopperError::TargetRefused { request, reply }) => {
            assert_eq!(request, "Z2,7ffd0040,8");
            assert_eq!(reply, "E01");
        }
        other => panic!("{other:?}"),
    }
    c.remove_breakpoint(BreakKind::Software, 0x401000, 1).unwrap();
    c.remove_breakpoint(BreakKind::WriteWatch, 0x7ffd0000, 8).unwrap();
    assert!(matches!(
        c.insert_breakpoint(BreakKind::Hardware, 0, 1),
        Err(HopperError::Unsupported(_))
    ));
    log.assert_finished();
}

#[test]
fn lists_and_selects_threads() {
    let script = handshake(RR_SUPPORTED)
        .exchange("qfThreadInfo", "mp10.10,p10.11")
        .exchange("qsThreadInfo", "mp10.12")
        .exchange("qsThreadInfo", "l")
        .exchange("qC", "QCp10.11")
        .exchange("Hgp10.11", "OK")
        .exchange("Hc-1", "OK")
        .exchange("Hg0", "OK")
        .exchange("Hgp10.99", "E01");
    let (mut c, log) = client(script);
    assert_eq!(c.thread_list().unwrap(), [tid(0x10, 0x10), tid(0x10, 0x11), tid(0x10, 0x12)]);
    assert_eq!(c.current_thread().unwrap(), tid(0x10, 0x11));
    c.select_thread(ThreadOp::General, ThreadSelector::Thread(tid(0x10, 0x11))).unwrap();
    c.select_thread(ThreadOp::Continue, ThreadSelector::All).unwrap();
    c.select_thread(ThreadOp::General, ThreadSelector::Any).unwrap();
    assert!(c.select_thread(ThreadOp::General, ThreadSelector::Thread(tid(0x10, 0x99))).is_err());
    log.assert_finished();
}

#[test]
fn reads_threads_auxv_and_exec_file_with_qxfer() {
    let threads = r#"<threads>
<thread id="p21f9.21f9" core="3" name="worked-example"/>
<thread id="p21f9.21fa" core="1" name="worker"/>
</threads>
"#;
    let auxv: Vec<u8> = (0..40u8).chain([b'#', b'*']).collect();
    let mut auxv_tail = b"l".to_vec();
    auxv_tail.extend(hopper_rsp::packet::escape_binary(&auxv[20..]));
    let mut auxv_head = b"m".to_vec();
    auxv_head.extend(hopper_rsp::packet::escape_binary(&auxv[..20]));
    let script = handshake("PacketSize=1000;QStartNoAckMode+;qXfer:threads:read+")
        .exchange("qXfer:threads:read::0,7f0", format!("l{threads}"))
        .exchange("qXfer:auxv:read::0,7f0", auxv_head)
        .exchange("qXfer:auxv:read::14,7f0", auxv_tail)
        .exchange("qXfer:exec-file:read:21f9:0,7f0", "l/tmp/prog")
        .exchange("qXfer:exec-file:read::0,7f0", "E01");
    let (mut c, log) = client(script);
    let t = c.thread_info().unwrap();
    assert_eq!(t.len(), 2);
    assert_eq!(t[1].id, tid(0x21f9, 0x21fa));
    assert_eq!(t[1].name.as_deref(), Some("worker"));
    assert_eq!(t[0].core, Some(3));
    assert_eq!(c.auxv().unwrap(), auxv);
    assert_eq!(c.exec_file(Some(0x21f9)).unwrap(), "/tmp/prog");
    assert!(matches!(c.exec_file(None), Err(HopperError::TargetRefused { .. })));
    log.assert_finished();
}

#[test]
fn resumes_forward_with_vcont_and_reads_stop_replies() {
    let script = handshake(RR_SUPPORTED)
        .exchange("?", "T05thread:p10.10;")
        // rr's vCont? reply (GdbServerConnection.cc).
        .exchange("vCont?", "vCont;c;C;s;S;")
        .exchange_many(
            "vCont;c",
            &[b"O68690a".as_slice(), b"T05thread:p10.10;swbreak:;".as_slice()],
        )
        .exchange("vCont;s:p10.10", "T05thread:p10.10;")
        .exchange("vCont;c", "T05thread:p10.11;watch:7ffd1230;")
        .exchange("vCont;c", "W00;process:10");
    let (mut c, log) = client(script);
    assert!(matches!(c.stop_reason().unwrap(), StopReply::Stopped(s) if s.signal == 5));
    let StopReply::Stopped(s) = c.resume(ResumeRequest::Continue).unwrap() else { panic!() };
    assert_eq!(s.cause, StopCause::SoftwareBreak);
    assert_eq!(c.take_console(), b"hi\n");
    let StopReply::Stopped(s) = c.resume(ResumeRequest::Step(tid(0x10, 0x10))).unwrap() else {
        panic!()
    };
    assert_eq!(s.cause, StopCause::Unspecified);
    let StopReply::Stopped(s) = c.resume(ResumeRequest::Continue).unwrap() else { panic!() };
    assert_eq!(s.thread, Some(tid(0x10, 0x11)));
    assert_eq!(s.cause, StopCause::Watchpoint { kind: BreakKind::WriteWatch, addr: 0x7ffd1230 });
    assert_eq!(
        c.resume(ResumeRequest::Continue).unwrap(),
        StopReply::Exited { code: 0, pid: Some(0x10) }
    );
    log.assert_finished();
}

#[test]
fn falls_back_to_plain_packets_without_vcont() {
    let script = handshake("PacketSize=400;QStartNoAckMode+")
        .exchange("vCont?", "")
        .exchange("c", "S05")
        .exchange("Hc1f", "OK")
        .exchange("s", "S05");
    let (mut c, log) = client(script);
    c.resume(ResumeRequest::Continue).unwrap();
    c.resume(ResumeRequest::Step(ThreadId::new(None, Tid(0x1f)))).unwrap();
    log.assert_finished();
}

#[test]
fn reverses_with_bc_and_bs() {
    let script = handshake(RR_SUPPORTED)
        .exchange("bc", "T00thread:p10.10;replaylog:begin;")
        .exchange("Hcp10.10", "OK")
        .exchange("bs", "T05thread:p10.10;");
    let (mut c, log) = client(script);
    let StopReply::Stopped(s) = c.resume(ResumeRequest::ReverseContinue).unwrap() else { panic!() };
    assert_eq!(s.cause, StopCause::HistoryBegin);
    c.resume(ResumeRequest::ReverseStep(tid(0x10, 0x10))).unwrap();
    log.assert_finished();

    // gdbserver offers no reverse execution: the client refuses without sending anything.
    let (mut c, log) = client(handshake("PacketSize=400;QStartNoAckMode+"));
    assert!(matches!(c.resume(ResumeRequest::ReverseContinue), Err(HopperError::Unsupported(_))));
    assert!(matches!(
        c.resume(ResumeRequest::ReverseStep(tid(1, 1))),
        Err(HopperError::Unsupported(_))
    ));
    log.assert_finished();
}

#[test]
fn interrupts_a_running_target() {
    let script = handshake(RR_SUPPORTED)
        .exchange("vCont?", "vCont;c;C;s;S;")
        .silent("vCont;c")
        .interrupt("T02thread:p10.10;");
    let (mut c, log) = client(script);
    c.start_resume(ResumeRequest::Continue).unwrap();
    let handle = c.interrupt_handle().unwrap();
    std::thread::spawn(move || handle.interrupt().unwrap()).join().unwrap();
    let StopReply::Stopped(s) = c.wait_stop().unwrap() else { panic!() };
    assert_eq!(s.signal, 2);
    log.assert_finished();
}

#[test]
fn seeks_with_rr_vrun_forms() {
    // vRun;;<hex of "1234">, vRun;;<hex of "t5678">, vRun;;<hex of "c2">, vRun;
    assert_eq!(RrSeek::Event(1234).packet(), "vRun;;31323334");
    assert_eq!(RrSeek::Ticks(5678).packet(), "vRun;;7435363738");
    assert_eq!(RrSeek::Checkpoint(2).packet(), "vRun;;6332");
    assert_eq!(RrSeek::Previous.packet(), "vRun;");
    let script = handshake(RR_SUPPORTED)
        .exchange("vRun;;31323334", "T05thread:p10.10;")
        .exchange("vRun;;6339", "E01");
    let (mut c, log) = client(script);
    assert!(matches!(c.rr_seek(RrSeek::Event(1234)).unwrap(), StopReply::Stopped(_)));
    assert!(matches!(c.rr_seek(RrSeek::Checkpoint(9)), Err(HopperError::TargetRefused { .. })));
    log.assert_finished();
}

fn hex(s: &str) -> String {
    hopper_rsp::packet::to_hex(s.as_bytes())
}

#[test]
fn runs_rr_commands_hex_encoded() {
    let script = handshake(RR_SUPPORTED)
        .exchange("qRRCmd:when:-1", hex("Completed event: 42\n"))
        .exchange("qRRCmd:when-ticks:4096", hex("Current tick: 123456\n"))
        .exchange("qRRCmd:when-tid:-1", hex("Current tid: 4096\n"))
        .exchange("qRRCmd:when-end:-1", hex("Event at end of recording: 900\n"))
        .exchange(
            format!("qRRCmd:checkpoint:-1:{}", hex("main at main.rs:12")),
            hex("Checkpoint 3 at main at main.rs:12\n"),
        )
        .exchange(format!("qRRCmd:delete checkpoint:-1:{}", hex("3")), hex("Deleted checkpoint 3.\n"))
        .exchange(format!("qRRCmd:delete checkpoint:-1:{}", hex("7")), hex("No checkpoint number 7.\n"))
        .exchange("qRRCmd:bogus:-1", hex("Command 'bogus' not found.\n"))
        .exchange("qRRCmd:when:-1", "RRCmd_EndDiversion")
        .exchange(
            format!("qRRCmd:checkpoint:-1:{}", hex("x")),
            hex("The program is not being run.\n"),
        );
    let (mut c, log) = client(script);
    assert_eq!(c.rr_when().unwrap(), 42);
    assert_eq!(c.rr_when_ticks(Some(Tid(4096))).unwrap(), 123456);
    assert_eq!(c.rr_when_tid().unwrap(), Tid(4096));
    assert_eq!(c.rr_when_end().unwrap(), 900);
    assert_eq!(c.rr_checkpoint("main at main.rs:12").unwrap(), 3);
    c.rr_delete_checkpoint(3).unwrap();
    assert!(c.rr_delete_checkpoint(7).is_err());
    assert!(matches!(c.rr_cmd("bogus", None, &[]), Err(HopperError::Unsupported(_))));
    assert!(matches!(c.rr_when(), Err(HopperError::TargetRefused { .. })));
    assert!(matches!(c.rr_checkpoint("x"), Err(HopperError::TargetRefused { .. })));
    log.assert_finished();
}

#[test]
fn rr_commands_on_a_plain_server_are_unsupported() {
    let script = handshake("PacketSize=400;QStartNoAckMode+").exchange("qRRCmd:when:-1", "");
    let (mut c, log) = client(script);
    assert!(matches!(c.rr_when(), Err(HopperError::Unsupported(_))));
    log.assert_finished();
}

#[test]
fn records_a_transcript_that_replays() {
    let script = handshake(RR_SUPPORTED).exchange("qC", "QCp10.10").exchange("m10,2", "abcd");
    let (mut c, log) = client(script);
    c.record_transcript();
    c.current_thread().unwrap();
    let mut b = [0u8; 2];
    c.read_memory(0x10, &mut b).unwrap();
    let text = c.take_transcript();
    assert_eq!(text, "> qC\n< QCp10.10\n> m10,2\n< abcd\n");
    log.assert_finished();

    let replay = Script::parse(&format!("> {QSUPPORTED}\n< {RR_SUPPORTED}\n> QStartNoAckMode\n< OK\n{text}"))
        .unwrap();
    let (mut c, log) = client(replay);
    c.current_thread().unwrap();
    c.read_memory(0x10, &mut b).unwrap();
    assert_eq!(b, [0xab, 0xcd]);
    log.assert_finished();
}

#[test]
fn disconnect_is_typed() {
    let (mut c, _log) = client(handshake(RR_SUPPORTED));
    // The script has nothing more: the client sees the stream end.
    assert!(c.current_thread().is_err());
}
