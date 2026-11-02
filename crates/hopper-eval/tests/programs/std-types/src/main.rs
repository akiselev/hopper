// More std types and enum layouts for hopper-eval's tests. `stop_here` is the breakpoint;
// every local of `main` is live when it is called.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum Colour {
    Red,
    Green,
    Blue,
}

#[derive(Debug)]
struct Unit;

#[derive(Debug)]
struct Pair(i16, u8);

#[derive(Debug)]
#[allow(dead_code)]
enum Tagged {
    Small(u8),
    Big { value: u64, label: &'static str },
}

static GREETING: &str = "hi there";
static mut COUNTER: u32 = 41;

#[inline(never)]
fn stop_here(n: usize) -> usize {
    n * 2
}

fn main() {
    let set: HashSet<u16> = [5u16, 6, 7].into_iter().collect();
    let bset: BTreeSet<i32> = [3, 1, 2].into_iter().collect();
    let big_tree: BTreeMap<u32, u32> = (0..40).map(|i| (i, i * i)).collect();
    let big_map: HashMap<u64, i8> = (0..20u64).map(|i| (i, i as i8 - 10)).collect();
    let mut ring: VecDeque<u8> = VecDeque::with_capacity(4);
    ring.extend([1, 2, 3, 4]);
    ring.pop_front();
    ring.pop_front();
    ring.push_back(5);
    ring.push_back(6);
    let strong = Rc::new(10u8);
    let weak = Rc::downgrade(&strong);
    let dangling: std::rc::Weak<u8> = std::rc::Weak::new();
    let shared = Arc::new(String::from("arc"));
    let aweak = Arc::downgrade(&shared);
    let cell = Cell::new(-3i32);
    let refcell = RefCell::new(String::from("cell"));
    let borrowed = refcell.borrow();
    let nonzero = NonZeroU32::new(12).unwrap();
    let os = OsString::from("os-string");
    let path = PathBuf::from("/tmp/x.txt");
    let colour = Colour::Green;
    let unit = Unit;
    let pair = Pair(-2, 9);
    let opt_ref: Option<&u8> = Some(&*strong);
    let opt_box: Option<Box<i32>> = None;
    let opt_nz: Option<NonZeroU32> = Some(nonzero);
    let tagged = Tagged::Big { value: 77, label: "lbl" };
    let small = Tagged::Small(4);
    let zeros = [0u8; 16];
    let empty: Vec<u32> = Vec::new();
    let unit_value = ();
    let r = stop_here(set.len());
    unsafe { COUNTER += 1 };
    println!(
        "{set:?} {bset:?} {} {} {ring:?} {strong} {weak:?} {dangling:?} {shared} {aweak:?} {cell:?} \
         {borrowed} {nonzero} {os:?} {path:?} {colour:?} {unit:?} {pair:?} {opt_ref:?} {opt_box:?} \
         {opt_nz:?} {tagged:?} {small:?} {zeros:?} {empty:?} {unit_value:?} {r} {GREETING} {}",
        big_tree.len(),
        big_map.len(),
        unsafe { COUNTER }
    );
}
