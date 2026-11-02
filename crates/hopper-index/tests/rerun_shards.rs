//! The rr rerun shard pipeline end to end with a stand-in `rr` that prints records laid out as
//! rr's `print_trace_fields` writes them (raw mode), plus `#[ignore]`d `rr_*` tests for a PMU
//! host.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use hopper_core::api::{Completeness, PageReq};
use hopper_core::{MomentRange, Place, ProcessId, RecordingId};
use hopper_index::writes::rerun::{
    CodeSource, RerunJob, ShardStart, plan_shards, run_rerun_shards,
};
use hopper_index::{Index, IndexControl, RecordingMeta, WriteValue};
use hopper_rr::{Rr, RrVersion};

struct Code(Vec<(u64, Vec<u8>)>);

impl CodeSource for Code {
    fn code_at(&self, pc: u64) -> Option<Vec<u8>> {
        self.0
            .iter()
            .find(|(a, _)| *a == pc)
            .map(|(_, b)| b.clone())
    }
}

fn record(event: u64, icount: u64, tid: i32, ticks: u64, ip: u64, rsp: u64, rax: u64) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [event, icount] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&(tid as u32).to_le_bytes());
    for v in [ticks, ip, 0x246, 0, 0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    let mut gp = [0u64; 16];
    gp[0] = rax;
    gp[4] = rsp;
    for g in gp {
        b.extend_from_slice(&g.to_le_bytes());
    }
    b.push(b'\n');
    b
}

#[test]
fn shards_from_a_stand_in_rr_land_in_the_index() {
    let root = hopper_symbols::fixtures::repo_root().join("target/index-tests");
    std::fs::create_dir_all(&root).unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("rerun")
        .tempdir_in(&root)
        .unwrap();
    // push rax at 0x1000 (rsp 0x8000 -> 0x7ff8), then mov [rsp], rax at 0x1001 with rax = 9.
    let mut out = record(4, 0, 77, 10, 0x1000, 0x8000, 5);
    out.extend(record(5, 1, 77, 10, 0x1001, 0x7ff8, 9));
    out.extend(record(5, 2, 77, 11, 0x1005, 0x7ff8, 9));
    let data = tmp.path().join("records.bin");
    std::fs::write(&data, &out).unwrap();
    let args = tmp.path().join("args.txt");
    let script = tmp.path().join("rr");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\ncat {}\n",
            args.display(),
            data.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let rec = RecordingId([5; 16]);
    let process = ProcessId::new(77);
    let index = Index::open(
        &tmp.path().join("index"),
        RecordingMeta {
            recording: rec,
            build_ids: vec![],
        },
    )
    .unwrap();
    let job = RerunJob {
        rr: Rr {
            binary: script,
            version: RrVersion::parse("rr version 5.9.0").unwrap(),
        },
        trace_dir: Path::new("/nonexistent/trace").to_path_buf(),
        recording: rec,
        process,
        shards: plan_shards(5, 9, 1),
        start: ShardStart::Replay,
        flush_every: 1,
    };
    let code = Code(vec![
        (0x1000, vec![0x50]),
        (0x1001, vec![0x48, 0x89, 0x04, 0x24]),
    ]);
    let s = run_rerun_shards(&job, &code, &index, &IndexControl::new()).unwrap();
    assert_eq!((s.instructions, s.writes, s.undecoded), (2, 2, 0));
    let argv = std::fs::read_to_string(&args).unwrap();
    assert!(argv.contains("rerun --singlestep=event,icount,tid,ticks,ip,flags,fsbase,gsbase,gp_x16 -r -s 5 -e 9 /nonexistent/trace"), "{argv}");

    let slot = Place::memory(process, 0x7ff8, 8);
    let ws = index
        .writes_in(slot, MomentRange::all(), PageReq::first(10))
        .unwrap();
    assert_eq!(ws.items.len(), 2);
    assert_eq!(
        ws.items[0].value,
        WriteValue::Bytes(5u64.to_le_bytes().to_vec())
    );
    assert_eq!(
        ws.items[1].value,
        WriteValue::Bytes(9u64.to_le_bytes().to_vec())
    );
    let last = index.last_write(slot, s.last.unwrap()).unwrap();
    assert_eq!(last.value.unwrap().pc, 0x1001);
    assert_eq!(last.completeness, Completeness::Complete);
}

#[test]
fn a_failing_rr_is_an_error() {
    let root = hopper_symbols::fixtures::repo_root().join("target/index-tests");
    std::fs::create_dir_all(&root).unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("rerun-fail")
        .tempdir_in(&root)
        .unwrap();
    let script = tmp.path().join("rr");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'rr: Unable to open performance counter' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rec = RecordingId([6; 16]);
    let index = Index::open(
        &tmp.path().join("index"),
        RecordingMeta {
            recording: rec,
            build_ids: vec![],
        },
    )
    .unwrap();
    let job = RerunJob {
        rr: Rr {
            binary: script,
            version: RrVersion::parse("rr version 5.9.0").unwrap(),
        },
        trace_dir: tmp.path().to_path_buf(),
        recording: rec,
        process: ProcessId::new(1),
        shards: plan_shards(1, 3, 2),
        start: ShardStart::Replay,
        flush_every: 16,
    };
    let err = run_rerun_shards(&job, &Code(vec![]), &index, &IndexControl::new()).unwrap_err();
    assert!(err.to_string().contains("performance counter"), "{err}");
}
