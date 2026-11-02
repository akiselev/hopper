//! `hsc-dump <file.hsc> [--binary <path>] [--function <name>]`: print a sidecar as text.
//! With `--binary`, the build-id is checked first.

use std::path::PathBuf;
use std::process::ExitCode;

use hopper_sidecar::{Sidecar, SidecarData, dump::dump};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut file = None;
    let mut binary = None;
    let mut function = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--binary" => binary = args.next().map(PathBuf::from),
            "--function" => function = args.next(),
            "-h" | "--help" => {
                println!("hsc-dump <file.hsc> [--binary <path>] [--function <name>]");
                return ExitCode::SUCCESS;
            }
            _ => file = Some(PathBuf::from(a)),
        }
    }
    let Some(file) = file else {
        eprintln!("hsc-dump: no sidecar file given");
        return ExitCode::FAILURE;
    };
    let sc = match &binary {
        Some(b) => Sidecar::open_for_binary(&file, b),
        None => Sidecar::read(&file),
    };
    let sc = match sc {
        Ok(sc) => sc,
        Err(e) => {
            eprintln!("hsc-dump: {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let mut data: SidecarData = sc.into_data();
    if let Some(name) = function {
        data.functions
            .retain(|f| f.demangled.contains(&name) || f.def_path.contains(&name));
    }
    print!("{}", dump(&data));
    ExitCode::SUCCESS
}
