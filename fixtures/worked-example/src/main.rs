// The plan's worked example (docs/plan/2026-11-01-first-implementation.md). Line numbers are
// part of the test contract: tests name `main.rs:<line>`, so keep every line where it is.
#[derive(Clone)]
struct Config { name: String, retries: u32 }

fn load() -> Config {
    let name = String::from("alpha");     // line 7
    Config { name, retries: 3 }           // line 8
}

fn main() {
    let a = load();                       // line 12
    let b = a;                            // line 13: move
    let c = b.clone();                    // line 14: clone (a call)
    let mut v = Vec::new();               // line 15
    v.push(c);                            // line 16: move into the Vec's heap buffer
    let n = &v[0].name;                   // line 17: index + field projection + borrow
    println!("{n} {}", b.retries);        // line 18
}
