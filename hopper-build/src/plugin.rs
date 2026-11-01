//! The rustc driver: a [`rustc_plugin::RustcPlugin`] whose callback writes a partial sidecar.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use hopper_sidecar::{Binding, CrateUnit, Header, SidecarData, UnitOutput, link::write_partial};
use rustc_middle::ty::TyCtxt;
use rustc_plugin::{CrateFilter, RustcPlugin, RustcPluginArgs, Utf8Path};
use rustc_session::config::{CrateType, OutFileName};
use rustc_span::def_id::LOCAL_CRATE;
use serde::{Deserialize, Serialize};

/// The name of the driver binary, beside `cargo-hopper-build`.
pub const DRIVER_NAME: &str = "hopper-build-driver";

/// The environment variable that carries the driver's identity ([`driver_identity`]). The driver
/// records it as an env dependency of every crate it analyses, so cargo rebuilds (and the driver
/// re-analyses) a crate whenever a different driver build runs, in every profile.
pub const DRIVER_IDENTITY_VAR: &str = "HOPPER_BUILD_DRIVER";

/// The directory, beside a profile's `deps/`, that holds its partial sidecars.
pub const PARTIALS_DIR_NAME: &str = "hopper-partials";

/// What the CLI tells every driver invocation (through rustc_plugin's `PLUGIN_ARGS`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DriverArgs {
    /// Where partials go when rustc has no `--out-dir` (cargo always passes one; then partials go
    /// to `<out-dir>/../hopper-partials`, one directory per profile and target).
    pub fallback_partials_dir: Option<PathBuf>,
}

/// A string that changes whenever the driver binary does: tool version, toolchain, path, size and
/// modification time.
pub fn driver_identity(driver: &Path) -> std::io::Result<String> {
    let md = std::fs::metadata(driver)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Ok(format!(
        "{} {} {} {} {mtime}",
        crate::tool_version(),
        rustc_plugin::CHANNEL,
        driver.display(),
        md.len()
    ))
}

/// The partials directory for a compilation whose `--out-dir` is `out_dir`
/// (`target/debug/deps` → `target/debug/hopper-partials`).
pub fn partials_dir_for_out_dir(out_dir: &Path) -> PathBuf {
    out_dir.parent().unwrap_or(out_dir).join(PARTIALS_DIR_NAME)
}

#[derive(Debug)]
pub struct HopperBuildPlugin;

impl RustcPlugin for HopperBuildPlugin {
    type Args = DriverArgs;

    fn version(&self) -> Cow<'static, str> {
        env!("CARGO_PKG_VERSION").into()
    }

    fn driver_name(&self) -> Cow<'static, str> {
        DRIVER_NAME.into()
    }

    /// Only `rustc_plugin::cli_main` calls this, which runs `cargo check`; `cargo-hopper-build`
    /// has its own CLI ([`crate::cli`]) because it must run `cargo build`.
    fn args(&self, target_dir: &Utf8Path) -> RustcPluginArgs<DriverArgs> {
        RustcPluginArgs {
            args: DriverArgs { fallback_partials_dir: Some(target_dir.join(PARTIALS_DIR_NAME).into()) },
            filter: CrateFilter::OnlyWorkspace,
        }
    }

    fn run(self, mut compiler_args: Vec<String>, args: DriverArgs) -> rustc_interface::interface::Result<()> {
        // A GNU build-id ties the linked binary to its sidecar. Harmless for libraries.
        compiler_args.extend(["-C".into(), "link-arg=-Wl,--build-id".into()]);
        let mut callbacks = SidecarCallbacks { args };
        rustc_driver::run_compiler(&compiler_args, &mut callbacks);
        Ok(())
    }
}

struct SidecarCallbacks {
    args: DriverArgs,
}

impl rustc_driver::Callbacks for SidecarCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        let identity = std::env::var(DRIVER_IDENTITY_VAR).ok();
        config.track_state = Some(Box::new(move |sess| {
            sess.env_depinfo.borrow_mut().insert((
                rustc_span::Symbol::intern(DRIVER_IDENTITY_VAR),
                identity.as_deref().map(rustc_span::Symbol::intern),
            ));
        }));
    }

    fn after_analysis(&mut self, _compiler: &rustc_interface::interface::Compiler, tcx: TyCtxt<'_>)
    -> rustc_driver::Compilation {
        if tcx.sess.opts.output_types.should_codegen() {
            let data = crate_partial(tcx);
            let dir = match (&tcx.sess.io.output_dir, &self.args.fallback_partials_dir) {
                (Some(out), _) => partials_dir_for_out_dir(&std::path::absolute(out).unwrap_or_else(|_| out.clone())),
                (None, Some(fallback)) => fallback.clone(),
                (None, None) => PathBuf::from(PARTIALS_DIR_NAME),
            };
            if let Err(e) = write_partial(&dir, &data) {
                tcx.dcx().warn(format!("hopper-build: could not write the ownership sidecar: {e}"));
            }
        }
        rustc_driver::Compilation::Continue
    }
}

/// This crate's partial sidecar.
fn crate_partial(tcx: TyCtxt<'_>) -> SidecarData {
    let name = tcx.crate_name(LOCAL_CRATE).to_string();
    let linked = tcx.crate_types().iter().copied().find(|ct| {
        matches!(ct, CrateType::Executable | CrateType::Dylib | CrateType::Cdylib | CrateType::ProcMacro)
    });
    let output = match linked {
        Some(ct) => {
            let out = rustc_session::output::out_filename(
                tcx.sess,
                ct,
                tcx.output_filenames(()),
                tcx.crate_name(LOCAL_CRATE),
            );
            match out {
                OutFileName::Real(path) => UnitOutput::Linked {
                    path: std::path::absolute(&path).unwrap_or(path).to_string_lossy().into_owned(),
                    upstream: tcx.crates(()).iter().map(|c| tcx.stable_crate_id(*c).as_u64()).collect(),
                },
                OutFileName::Stdout => UnitOutput::Library,
            }
        }
        None => UnitOutput::Library,
    };
    let unit = CrateUnit { name: name.clone(), stable_crate_id: tcx.stable_crate_id(LOCAL_CRATE).as_u64(), output };
    SidecarData {
        header: Header {
            binding: Binding::Partial(unit),
            rustc_version: format!("rustc {}", rustc_interface::util::rustc_version_str().unwrap_or("unknown")),
            crates: vec![name],
            tool: crate::tool_version(),
        },
        functions: crate::extract::crate_functions(tcx),
    }
}
