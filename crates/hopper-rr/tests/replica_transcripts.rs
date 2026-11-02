//! `RrReplica` against literal packet transcripts, so the exact packets it sends are pinned.

use hopper_core::target::{Direction, Resume, StopReason, Target};
use hopper_core::{RecordingId, Tid};
use hopper_rr::RrReplica;
use hopper_rsp::packet::to_hex;
use hopper_rsp::script::{Script, ScriptedConnection};

const QSUP: &str = "qSupported:multiprocess+;swbreak+;hwbreak+;vContSupported+;xmlRegisters=i386";
const RR_SUPPORTED: &str = "PacketSize=100000;QStartNoAckMode+;qXfer:features:read+;\
qXfer:auxv:read+;qXfer:exec-file:read+;qXfer:siginfo:read+;qXfer:siginfo:write+;multiprocess+;\
hwbreak+;swbreak+;ConditionalBreakpoints+;vContSupported+;QPassSignals+;ReverseContinue+;\
ReverseStep+";
const XML: &str = r#"<target><reg name="rax" bitsize="64" regnum="0"/><reg name="rip" bitsize="64" regnum="16"/></target>"#;
const REGS: &str = "05000000000000003412000000000000";

fn rr(text: &str) -> String {
    to_hex(format!("{text}\n").as_bytes())
}

fn attached() -> Script {
    Script::new()
        .exchange(QSUP, RR_SUPPORTED)
        .exchange("QStartNoAckMode", "OK")
        .exchange("?", "T05thread:p10.10;")
        .exchange("qXfer:exec-file:read:10:0,7fff0", "l/tmp/p")
}

#[test]
fn now_measures_sub_with_one_event_seek() {
    let script = attached()
        .exchange("qRRCmd:when:-1", rr("Completed event: 3"))
        .exchange("qRRCmd:when-ticks:16", rr("Current tick: 77"))
        .exchange("Hgp10.10", "OK")
        .exchange("qXfer:features:read:target.xml:0,7fff0", format!("l{XML}"))
        .exchange("g", REGS)
        // vRun;;<hex "3">: the event seek. rr lands on (3, 77) at once, so no ticks seek.
        .exchange("vRun;;33", "T05thread:p10.10;")
        .exchange("qRRCmd:when:-1", rr("Completed event: 3"))
        .exchange("qRRCmd:when-ticks:16", rr("Current tick: 77"))
        .exchange("Hgp10.10", "OK")
        .exchange("g", REGS);
    let (conn, log) = ScriptedConnection::new(script);
    let mut r = RrReplica::attach(conn, RecordingId([1; 16]), None).unwrap();
    let m = r.now().unwrap();
    assert_eq!((m.frame_time, m.tid, m.ticks, m.sub), (3, Tid(16), 77, 0));
    log.assert_finished();
}

#[test]
fn a_step_after_a_restore_absorbs_rrs_in_place_stop() {
    let script = attached()
        .exchange(
            format!("qRRCmd:checkpoint:-1:{}", to_hex(b"x")),
            rr("Checkpoint 1 at x"),
        )
        .exchange("vRun;;6331", "T05thread:p10.10;")
        .exchange("qRRCmd:when:-1", rr("Completed event: 12"))
        .exchange("qRRCmd:when-ticks:16", rr("Current tick: 100"))
        .exchange("Hgp10.10", "OK")
        .exchange("qXfer:features:read:target.xml:0,7fff0", format!("l{XML}"))
        .exchange("g", REGS)
        .exchange("vCont?", "vCont;c;C;s;S;")
        .exchange("vCont;s:p10.10", "T00thread:p10.10;")
        .exchange("Hgp10.10", "OK")
        .exchange("g", REGS)
        .exchange("vCont;s:p10.10", "T05thread:p10.10;")
        .exchange("qRRCmd:when:-1", rr("Completed event: 12"))
        .exchange("qRRCmd:when-ticks:16", rr("Current tick: 101"));
    let (conn, log) = ScriptedConnection::new(script);
    let mut r = RrReplica::attach(conn, RecordingId([1; 16]), None).unwrap();
    // A checkpoint made outside the replica's knowledge has no remembered moment.
    let id = hopper_core::CheckpointId(r.client().rr_checkpoint("x").unwrap());
    r.restore(id).unwrap();
    let ev = r
        .resume(Resume::Step { tid: Tid(16) }, Direction::Forward)
        .unwrap();
    assert_eq!(ev.reason, StopReason::Step);
    // The step changed only the ticks: sub is 0 without measuring.
    let m = r.now().unwrap();
    assert_eq!((m.frame_time, m.ticks, m.sub), (12, 101, 0));
    log.assert_finished();
}
