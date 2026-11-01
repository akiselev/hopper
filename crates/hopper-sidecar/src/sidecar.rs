//! A loaded sidecar with its indexes, and the build-id check against a binary.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use hopper_core::{HopperError, Result};

use crate::codec;
use crate::model::{Binding, BuildId, FunctionRecord, Header, OwnershipEvent, SidecarData, files_match};

/// An event's address inside a [`Sidecar`]: function index, then event index.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventRef {
    pub function: usize,
    pub event: usize,
}

/// A sidecar in memory, indexed by function and by (file, line).
#[derive(Debug, Clone)]
pub struct Sidecar {
    data: SidecarData,
    /// Mangled symbol, demangled name and def path → function indexes. The def path can name
    /// several instances of one generic function.
    by_name: HashMap<String, Vec<usize>>,
    /// File → line → events whose span starts on that line, in function then MIR order.
    by_line: BTreeMap<String, BTreeMap<u32, Vec<EventRef>>>,
}

impl Sidecar {
    pub fn new(data: SidecarData) -> Sidecar {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_line: BTreeMap<String, BTreeMap<u32, Vec<EventRef>>> = BTreeMap::new();
        for (fi, f) in data.functions.iter().enumerate() {
            for key in [&f.symbol, &f.demangled, &f.def_path] {
                let slot = by_name.entry(key.clone()).or_default();
                if !slot.contains(&fi) {
                    slot.push(fi);
                }
            }
            for (ei, e) in f.events.iter().enumerate() {
                by_line
                    .entry(e.span.file.clone())
                    .or_default()
                    .entry(e.span.lo.line)
                    .or_default()
                    .push(EventRef { function: fi, event: ei });
            }
        }
        Sidecar { data, by_name, by_line }
    }

    /// Decode a sidecar from bytes without checking it against a binary.
    pub fn from_bytes(bytes: &[u8]) -> Result<Sidecar> {
        Ok(Sidecar::new(codec::decode(bytes)?))
    }

    /// Read a sidecar file without checking it against a binary.
    pub fn read(path: &Path) -> Result<Sidecar> {
        Sidecar::from_bytes(&std::fs::read(path)?)
    }

    /// Read the sidecar of `binary` and check that it describes that binary: its binding must be
    /// [`Binding::Binary`] with the binary's GNU build-id, or the read fails with
    /// [`HopperError::StaleSidecar`].
    pub fn open_for_binary(sidecar: &Path, binary: &Path) -> Result<Sidecar> {
        let sc = Sidecar::read(sidecar)?;
        let actual = read_build_id(binary)?;
        sc.check_build_id(&actual)?;
        Ok(sc)
    }

    /// Fail with [`HopperError::StaleSidecar`] unless this sidecar was linked for a binary with
    /// this build-id.
    pub fn check_build_id(&self, actual: &BuildId) -> Result<()> {
        match &self.data.header.binding {
            Binding::Binary { build_id } if build_id == actual => Ok(()),
            Binding::Binary { build_id } => Err(HopperError::StaleSidecar(format!(
                "sidecar is for build-id {build_id}, the binary has {actual}"
            ))),
            Binding::Partial(unit) => Err(HopperError::StaleSidecar(format!(
                "sidecar is an unlinked partial for crate {}, not a linked binary",
                unit.name
            ))),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        codec::encode(&self.data)
    }

    /// Write atomically: to `<path>.tmp`, then rename over `path`.
    pub fn write(&self, path: &Path) -> Result<()> {
        write_data(&self.data, path)
    }

    pub fn header(&self) -> &Header {
        &self.data.header
    }

    pub fn functions(&self) -> &[FunctionRecord] {
        &self.data.functions
    }

    pub fn data(&self) -> &SidecarData {
        &self.data
    }

    pub fn into_data(self) -> SidecarData {
        self.data
    }

    pub fn function(&self, index: usize) -> &FunctionRecord {
        &self.data.functions[index]
    }

    pub fn event(&self, r: EventRef) -> &OwnershipEvent {
        &self.data.functions[r.function].events[r.event]
    }

    /// The functions a name denotes: a mangled symbol, a demangled symbol (with or without its
    /// `::h<hash>` suffix) or a def path. A def path of a generic function can name several.
    pub fn functions_named(&self, name: &str) -> Vec<usize> {
        if let Some(v) = self.by_name.get(name) {
            return v.clone();
        }
        let demangled = format!("{:#}", rustc_demangle::demangle(name));
        if let Some(v) = self.by_name.get(&demangled) {
            return v.clone();
        }
        if let Some(stripped) = strip_hash(name)
            && let Some(v) = self.by_name.get(stripped)
        {
            return v.clone();
        }
        Vec::new()
    }

    /// The single function a name denotes, when it denotes exactly one.
    pub fn function_named(&self, name: &str) -> Option<usize> {
        match self.functions_named(name).as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }

    /// Every event whose span starts on `line` of a file matching `file` (by path suffix).
    pub fn events_at_line(&self, file: &str, line: u32) -> Vec<EventRef> {
        let mut out = Vec::new();
        for (f, lines) in &self.by_line {
            if files_match(f, file)
                && let Some(refs) = lines.get(&line)
            {
                out.extend_from_slice(refs);
            }
        }
        out.sort();
        out
    }

    /// The events of one function whose span starts on `line` of a file matching `file`.
    pub fn function_events_at_line(&self, function: usize, file: &str, line: u32) -> Vec<EventRef> {
        self.events_at_line(file, line).into_iter().filter(|r| r.function == function).collect()
    }
}

/// `foo::bar::h0123456789abcdef` → `foo::bar`.
fn strip_hash(name: &str) -> Option<&str> {
    let (head, tail) = name.rsplit_once("::")?;
    (tail.len() == 17 && tail.starts_with('h') && tail[1..].bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(head)
}

pub(crate) fn write_data(data: &SidecarData, path: &Path) -> Result<()> {
    let bytes = codec::encode(data)?;
    let mut tmp: PathBuf = path.as_os_str().to_owned().into();
    tmp.as_mut_os_string().push(".tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The sidecar path for a binary: `<binary>.hsc` beside it.
pub fn sidecar_path(binary: &Path) -> PathBuf {
    let mut p = binary.as_os_str().to_owned();
    p.push(".hsc");
    p.into()
}

/// Read the GNU build-id note of an ELF (or other object) file.
pub fn read_build_id(binary: &Path) -> Result<BuildId> {
    let bytes = std::fs::read(binary)?;
    build_id_of(&bytes).map_err(|e| match e {
        HopperError::NotFound(m) => HopperError::NotFound(format!("{}: {m}", binary.display())),
        other => other,
    })
}

/// The GNU build-id of an object file held in memory.
pub fn build_id_of(bytes: &[u8]) -> Result<BuildId> {
    use object::Object;
    let file = object::File::parse(bytes).map_err(|e| HopperError::Symbols(e.to_string()))?;
    match file.build_id().map_err(|e| HopperError::Symbols(e.to_string()))? {
        Some(id) => Ok(BuildId(id.to_vec())),
        None => Err(HopperError::NotFound("no GNU build-id note (link with -Wl,--build-id)".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_legacy_hash() {
        assert_eq!(strip_hash("worked_example::load::h0123456789abcdef"), Some("worked_example::load"));
        assert_eq!(strip_hash("worked_example::load"), None);
    }

    #[test]
    fn sidecar_path_appends_extension() {
        assert_eq!(sidecar_path(Path::new("/t/debug/worked-example")), PathBuf::from("/t/debug/worked-example.hsc"));
    }
}
