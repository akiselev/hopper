//! The debuggee fixtures: built once per test binary with `scripts/build-fixtures.sh`, and
//! symbol addresses read from their ELF files.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use object::{Object, ObjectSection, ObjectSymbol};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The path of fixture `name`, building every fixture the first time.
pub fn fixture(name: &str) -> PathBuf {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        let status = std::process::Command::new(repo_root().join("scripts/build-fixtures.sh"))
            .stdout(std::process::Stdio::null())
            .status()
            .expect("run scripts/build-fixtures.sh");
        assert!(status.success(), "building the fixtures failed");
    });
    let p = repo_root().join(format!("target/fixtures/{name}/debug/{name}"));
    assert!(p.is_file(), "{} was not built", p.display());
    p
}

/// What the tests need from an ELF file.
#[allow(dead_code, reason = "each test binary uses a different subset")]
pub struct Elf {
    pub entry: u64,
    data: Vec<u8>,
}

#[allow(dead_code, reason = "each test binary uses a different subset")]
impl Elf {
    pub fn open(path: &Path) -> Elf {
        let data = std::fs::read(path).unwrap();
        let entry = object::File::parse(&*data).unwrap().entry();
        Elf { entry, data }
    }

    /// The link-time address of the function whose demangled path (without hash) is `path`.
    pub fn symbol(&self, path: &str) -> u64 {
        let file = object::File::parse(&*self.data).unwrap();
        file.symbols()
            .find(|s| {
                s.name()
                    .map(|n| format!("{:#}", rustc_demangle::demangle(n)) == path)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no symbol {path}"))
            .address()
    }

    /// `len` bytes of the file's contents at link-time address `addr`.
    pub fn bytes_at(&self, addr: u64, len: usize) -> Vec<u8> {
        let file = object::File::parse(&*self.data).unwrap();
        for s in file.sections() {
            if addr >= s.address() && addr + len as u64 <= s.address() + s.size() {
                let d = s.data().unwrap();
                let off = (addr - s.address()) as usize;
                return d[off..off + len].to_vec();
            }
        }
        panic!("no section holds {addr:#x}");
    }
}
