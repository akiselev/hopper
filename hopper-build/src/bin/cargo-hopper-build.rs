#![feature(rustc_private)]

//! `cargo hopper-build [cargo build args]`: build with ownership sidecars.

fn main() -> std::process::ExitCode {
    hopper_build::cli::main()
}
