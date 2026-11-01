// Two threads writing a shared counter under a mutex, plus a recursive function: call trees,
// several tids, and a watched place written from more than one thread.
use std::sync::{Arc, Mutex};
use std::thread;

#[inline(never)]
fn fib(n: u32) -> u64 {
    if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) }
}

#[inline(never)]
fn bump(counter: &Mutex<u64>, by: u64) {
    let mut g = counter.lock().unwrap();
    *g += by;
}

fn main() {
    let counter = Arc::new(Mutex::new(0u64));
    let mut handles = Vec::new();
    for t in 0..2u64 {
        let c = Arc::clone(&counter);
        handles.push(thread::spawn(move || {
            for i in 0..3 {
                bump(&c, t * 10 + i);
            }
            fib(6 + t as u32)
        }));
    }
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    println!("{} {}", *counter.lock().unwrap(), total);
}
