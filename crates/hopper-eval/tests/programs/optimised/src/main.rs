// Built at opt-level 2 with full debuginfo: `mix`'s parameters and locals live in callee-saved
// registers and location lists, and `stop_here` is the breakpoint.
use std::hint::black_box;

#[inline(never)]
fn stop_here(x: u64) {
    black_box(x);
}

#[inline(never)]
fn mix(seed: u64, rounds: u32, name: &str) -> u64 {
    let mut acc = seed;
    for i in 0..rounds {
        acc = acc.wrapping_mul(31).wrapping_add(i as u64 ^ name.len() as u64);
        stop_here(acc);
    }
    acc
}

fn main() {
    let name = black_box("optimised");
    let out = mix(black_box(7), black_box(3), name);
    println!("{out}");
}
