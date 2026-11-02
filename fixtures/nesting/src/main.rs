// Values nested deeper than the summary depth, for comparing hopper's elision with gdb's
// `print max-depth` (20 by default). `stop_here` is the breakpoint the tests use.

#[derive(Debug)]
#[allow(dead_code)]
enum Chain {
    Link(Box<Chain>),
    End(u32),
}

#[derive(Debug)]
#[allow(dead_code)]
struct Level<T> {
    depth: u32,
    inner: T,
}

#[inline(never)]
fn stop_here(n: u32) -> u32 {
    n + 1
}

fn level<T>(depth: u32, inner: T) -> Level<T> {
    Level { depth, inner }
}

fn main() {
    // Nine levels: deeper than the old summary depth (8), within gdb's 20.
    let nine = level(1, level(2, level(3, level(4, level(5, level(6, level(7, level(8, level(9, 0u8)))))))));
    // Twenty-four levels of tuples: past gdb's max-depth, where both write `{...}`.
    let deep = ((((((((((((((((((((((((7u8,),),),),),),),),),),),),),),),),),),),),),),),);
    let wide = [[[[[[[[[[1u16; 2]; 1]; 1]; 1]; 1]; 1]; 1]; 1]; 1]; 1];
    let r = stop_here(nine.depth + deep.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0 as u32 + wide[0][0][0][0][0][0][0][0][0][0] as u32);
    println!("{nine:?} {deep:?} {wide:?} {r}");
}
