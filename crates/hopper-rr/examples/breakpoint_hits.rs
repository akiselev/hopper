//! Experiment E5: what a phase-A index pays per breakpoint hit during replay.
//!
//! `cargo run -p hopper-rr --example breakpoint_hits -- <trace-dir> <executable> <symbol>`
//!
//! Replays the whole recording twice on fresh `rr replay -s` servers: once with no breakpoints,
//! once with a breakpoint on `symbol` (a demangled path such as `hit` or `crate::f`), counting
//! hits. Prints both times and the cost per hit as a Markdown table row.

use std::path::{Path, PathBuf};
use std::time::Instant;

use hopper_core::target::{Breakpoint, Direction, Resume, StopReason, Target};
use hopper_core::{HopperError, Result};
use hopper_rr::{Auxv, ReplayOptions, Rr, RrReplica};
use object::{Object, ObjectSymbol};

fn symbol_and_entry(exe: &Path, path: &str) -> Result<(u64, u64)> {
    let data = std::fs::read(exe)?;
    let file = object::File::parse(&*data).map_err(|e| HopperError::Symbols(e.to_string()))?;
    let addr = file
        .symbols()
        .find(|s| {
            s.name()
                .is_ok_and(|n| format!("{:#}", rustc_demangle::demangle(n)) == path)
        })
        .map(|s| s.address())
        .ok_or_else(|| HopperError::NotFound(format!("symbol {path}")))?;
    Ok((addr, file.entry()))
}

fn run(rr: &Rr, trace: &Path, bp: Option<(u64, u64)>) -> Result<(f64, u64)> {
    let id = rr.recording_id(trace)?;
    let mut r = RrReplica::start(rr, trace, &ReplayOptions::default(), id)?;
    if let Some((link_addr, entry)) = bp {
        let bias = Auxv::parse(&r.auxv()?)?.load_bias_from_entry(entry)?;
        r.insert(Breakpoint::code(bias + link_addr))?;
    }
    let start = Instant::now();
    let mut hits = 0u64;
    loop {
        match r.resume(Resume::Continue, Direction::Forward)?.reason {
            StopReason::Breakpoint { .. } => hits += 1,
            StopReason::HistoryEnd => break,
            other => return Err(HopperError::Protocol(format!("unexpected stop {other:?}"))),
        }
    }
    Ok((start.elapsed().as_secs_f64(), hits))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: breakpoint_hits <trace-dir> <executable> <symbol>";
    let trace = PathBuf::from(args.next().expect(usage));
    let exe = PathBuf::from(args.next().expect(usage));
    let symbol = args.next().expect(usage);
    let rr = Rr::detect()?;
    let (base, _) = run(&rr, &trace, None)?;
    let (with, hits) = run(&rr, &trace, Some(symbol_and_entry(&exe, &symbol)?))?;
    let per_hit_us = if hits > 0 {
        (with - base) / hits as f64 * 1e6
    } else {
        0.0
    };
    println!("| {hits} | {base:.3} | {with:.3} | {per_hit_us:.1} |");
    Ok(())
}
