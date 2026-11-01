// Values of many std types, for the evaluator. `stop_here` is the breakpoint the tests use;
// every local in `main` is live when it is called.
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug)]
#[allow(dead_code)]
enum Shape {
    Circle { r: f64 },
    Rect(u32, u32),
    Empty,
}

#[derive(Debug)]
#[allow(dead_code)]
struct Point {
    x: i32,
    y: i32,
}

#[inline(never)]
fn stop_here(counter: u64) -> u64 {
    counter + 1
}

fn main() {
    let small: u8 = 7;
    let neg: i64 = -42;
    let flag = true;
    let pi = 3.25f64;
    let ch = 'λ';
    let text: &str = "hello";
    let owned = String::from("world");
    let nums = vec![1u32, 2, 3];
    let pt = Point { x: 3, y: -4 };
    let tuple = (1u16, "two", 3.0f32);
    let arr = [10i8, 20, 30, 40];
    let some: Option<u32> = Some(5);
    let none: Option<u32> = None;
    let boxed = Box::new(Point { x: 1, y: 2 });
    let ok: Result<i32, String> = Ok(9);
    let err: Result<i32, String> = Err(String::from("bad"));
    let shapes = vec![Shape::Circle { r: 1.5 }, Shape::Rect(2, 3), Shape::Empty];
    let mut map = HashMap::new();
    map.insert(String::from("one"), 1i32);
    map.insert(String::from("two"), 2);
    let mut tree = BTreeMap::new();
    tree.insert(3u8, 'c');
    tree.insert(1, 'a');
    let deque: VecDeque<i32> = (1..=4).collect();
    let rc = Rc::new(String::from("shared"));
    let rc2 = Rc::clone(&rc);
    let arc = Arc::new(17u64);
    let cell = RefCell::new(vec![5u8]);
    let slice: &[u32] = &nums[1..];
    let r = stop_here(small as u64);
    println!(
        "{small} {neg} {flag} {pi} {ch} {text} {owned} {nums:?} {pt:?} {tuple:?} {arr:?} {some:?} \
         {none:?} {boxed:?} {ok:?} {err:?} {shapes:?} {} {tree:?} {deque:?} {rc} {rc2} {arc} \
         {cell:?} {slice:?} {r}",
        map.len()
    );
}
