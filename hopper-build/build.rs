#![feature(rustc_private)]

// Adds an rpath to the toolchain's target libdir so the driver finds librustc_driver at run time.
fn main() {
    rustc_plugin::build_main();
}
