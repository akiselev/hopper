#![feature(rustc_private)]

//! rustc with hopper's sidecar callback; `cargo hopper-build` sets it as the workspace wrapper.

fn main() -> std::process::ExitCode {
    rustc_plugin::driver_main(hopper_build::plugin::HopperBuildPlugin)
}
