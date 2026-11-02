//! Experiment E2: latency of rr's RSP operations over hopper's own client.
//!
//! `cargo run -p hopper-rr --example rsp_latency -- <trace-dir> [samples]`
//!
//! Measures, on one `rr replay -s` server: `qRRCmd:when-ticks`, `vRun` seeks to events spread
//! over the recording, reverse single steps (`bs`), forward single steps, and reverse continues
//! (`bc`) from the middle of the recording back to its start. Prints a Markdown report.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use hopper_core::target::{Direction, Resume, Target};
use hopper_rr::{ReplayOptions, Rr, RrReplica};
use hopper_rsp::ResumeRequest;

fn stats(name: &str, mut v: Vec<Duration>) -> String {
    if v.is_empty() {
        return format!("| {name} | 0 | | | | |");
    }
    v.sort();
    let ms = |d: Duration| format!("{:.3}", d.as_secs_f64() * 1000.0);
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!(
        "| {name} | {} | {} | {} | {} | {} |",
        v.len(),
        ms(v[0]),
        ms(p(0.5)),
        ms(p(0.95)),
        ms(*v.last().unwrap())
    )
}

fn timed<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let t = Instant::now();
    let r = f();
    (t.elapsed(), r)
}

fn main() -> hopper_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let trace = PathBuf::from(
        args.next()
            .expect("usage: rsp_latency <trace-dir> [samples]"),
    );
    let samples: u64 = args
        .next()
        .map(|s| s.parse().expect("samples"))
        .unwrap_or(20);

    let rr = Rr::detect()?;
    let recording = rr.recording_id(&trace)?;
    let (startup, replica) =
        timed(|| RrReplica::start(&rr, &trace, &ReplayOptions::default(), recording));
    let mut r = replica?;
    let last = r.last_event()?;
    let tid = r.current_thread()?;

    let mut when_ticks = Vec::new();
    for _ in 0..samples {
        let (d, v) = timed(|| r.client().rr_when_ticks(None));
        v?;
        when_ticks.push(d);
    }

    // Seeks are timed as the vRun round trip. rr answers the first resume after a vRun in
    // place, so a reverse step through the Target API (which absorbs that stop) follows each
    // seek; the timed reverse steps after it are raw `bs` packets.
    let mut seeks = Vec::new();
    let mut bs = Vec::new();
    let thread = hopper_rsp::ThreadId::new(Some(r.info().process.pid), tid);
    for i in 0..samples {
        let event = 1 + last.saturating_sub(2) * (i + 1) / (samples + 1);
        let (d, v) = timed(|| r.seek_event(event));
        v?;
        seeks.push(d);
        let tid = r.current_thread()?;
        r.resume(Resume::Step { tid }, Direction::Reverse)?;
        let thread = hopper_rsp::ThreadId { tid, ..thread };
        for _ in 0..5 {
            let (d, v) = timed(|| r.client().resume(ResumeRequest::ReverseStep(thread)));
            v?;
            bs.push(d);
        }
    }

    // Forward steps (with hopper's clock bookkeeping) and reverse continues to the start.
    let mut steps = Vec::new();
    let mut bc = Vec::new();
    let reverse_continues = samples.clamp(1, 5);
    for i in 0..reverse_continues {
        let event = 1 + last.saturating_sub(2) * (i + 1) / (reverse_continues + 1);
        r.seek_event(event)?;
        let tid = r.current_thread()?;
        for _ in 0..10 {
            let (d, v) = timed(|| r.resume(Resume::Step { tid }, Direction::Forward));
            v?;
            steps.push(d);
        }
        let (d, v) = timed(|| r.resume(Resume::Continue, Direction::Reverse));
        v?;
        bc.push(d);
    }

    println!("| operation | samples | min ms | median ms | p95 ms | max ms |");
    println!("|---|---|---|---|---|---|");
    println!(
        "{}",
        stats("rr replay startup to first stop", vec![startup])
    );
    println!("{}", stats("qRRCmd:when-ticks", when_ticks));
    println!("{}", stats("vRun seek to an event", seeks));
    println!("{}", stats("bs (reverse step)", bs));
    println!("{}", stats("forward step (Target::resume)", steps));
    println!("{}", stats("bc to the start (Target::resume)", bc));
    println!();
    println!("Recording: {} events, rr {}.", last, rr.version);
    Ok(())
}
