//! Linking per-crate partial sidecars into one sidecar per binary.
//!
//! The driver writes one partial per crate compilation into a partials directory, named
//! `<crate>-<stable crate id, 16 hex digits>.hsc` ([`partial_file_name`]). A partial for an
//! executable records the path rustc linked and the stable crate ids of every crate it links.
//! After the build, [`PartialSet::link_binary`] finds the executable's partial (by file identity,
//! because cargo hard-links `deps/<name>-<hash>` to `<profile>/<name>`), reads the binary's
//! build-id and merges the executable's partial with the partials of the crates it links.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use hopper_core::{HopperError, Result};

use crate::codec;
use crate::model::{Binding, BuildId, CrateUnit, FunctionRecord, Header, SidecarData, UnitOutput};
use crate::sidecar::{read_build_id, sidecar_path, write_data};

/// The file name a crate's partial is written under.
pub fn partial_file_name(unit: &CrateUnit) -> String {
    format!("{}-{:016x}.hsc", unit.name, unit.stable_crate_id)
}

/// Write a partial into `dir` under its canonical name; returns the path.
pub fn write_partial(dir: &Path, data: &SidecarData) -> Result<PathBuf> {
    let Binding::Partial(unit) = &data.header.binding else {
        return Err(HopperError::Invalid(
            "write_partial needs a partial binding".into(),
        ));
    };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(partial_file_name(unit));
    write_data(data, &path)?;
    Ok(path)
}

/// Merge an executable's partial with its libraries' partials into the sidecar of the binary
/// with `build_id`. Functions are deduplicated by symbol (shared generics can be codegen'd in
/// more than one crate) and sorted by symbol. Libraries the executable does not link are
/// ignored; every partial must come from the same rustc.
pub fn link(
    executable: &SidecarData,
    libraries: &[SidecarData],
    build_id: BuildId,
    tool: &str,
) -> Result<SidecarData> {
    let Binding::Partial(CrateUnit {
        name,
        output: UnitOutput::Linked { upstream, .. },
        ..
    }) = &executable.header.binding
    else {
        return Err(HopperError::Invalid(
            "link needs an executable's partial".into(),
        ));
    };
    let wanted: HashSet<u64> = upstream.iter().copied().collect();
    let mut crates = vec![name.clone()];
    let mut functions: BTreeMap<String, FunctionRecord> = BTreeMap::new();
    for f in &executable.functions {
        functions
            .entry(f.symbol.clone())
            .or_insert_with(|| f.clone());
    }
    for lib in libraries {
        let Binding::Partial(unit) = &lib.header.binding else {
            continue;
        };
        if !wanted.contains(&unit.stable_crate_id) {
            continue;
        }
        if lib.header.rustc_version != executable.header.rustc_version {
            return Err(HopperError::Invalid(format!(
                "partial for {} was built by {}, the executable by {}",
                unit.name, lib.header.rustc_version, executable.header.rustc_version
            )));
        }
        if !crates.contains(&unit.name) {
            crates.push(unit.name.clone());
        }
        for f in &lib.functions {
            functions
                .entry(f.symbol.clone())
                .or_insert_with(|| f.clone());
        }
    }
    crates[1..].sort();
    Ok(SidecarData {
        header: Header {
            binding: Binding::Binary { build_id },
            rustc_version: executable.header.rustc_version.clone(),
            crates,
            tool: tool.to_owned(),
        },
        functions: functions.into_values().collect(),
    })
}

/// Every partial in a directory.
#[derive(Debug, Default)]
pub struct PartialSet {
    pub partials: Vec<(PathBuf, SidecarData)>,
}

impl PartialSet {
    /// Read every `*.hsc` partial in `dir` (a missing directory is an empty set). Files that are
    /// not sidecars, or are linked sidecars, are skipped.
    pub fn load_dir(dir: &Path) -> Result<PartialSet> {
        let mut partials = Vec::new();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PartialSet::default()),
            Err(e) => return Err(e.into()),
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "hsc"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(data) = codec::decode(&std::fs::read(&path)?) else {
                continue;
            };
            if matches!(data.header.binding, Binding::Partial(_)) {
                partials.push((path, data));
            }
        }
        Ok(PartialSet { partials })
    }

    /// The executable partial whose linked output is `binary` (the same file, or a hard link
    /// to it).
    pub fn executable_for(&self, binary: &Path) -> Option<&SidecarData> {
        self.partials
            .iter()
            .map(|(_, d)| d)
            .find(|d| match &d.header.binding {
                Binding::Partial(CrateUnit {
                    output: UnitOutput::Linked { path, .. },
                    ..
                }) => same_file(Path::new(path), binary),
                _ => false,
            })
    }

    /// Link the sidecar for `binary` and write it to `<binary>.hsc`. Returns the written path, or
    /// `None` when no partial in the set produced this binary.
    pub fn link_binary(&self, binary: &Path, tool: &str) -> Result<Option<PathBuf>> {
        let Some(exe) = self.executable_for(binary) else {
            return Ok(None);
        };
        let build_id = read_build_id(binary)?;
        let libs: Vec<SidecarData> = self.partials.iter().map(|(_, d)| d.clone()).collect();
        let linked = link(exe, &libs, build_id, tool)?;
        let out = sidecar_path(binary);
        write_data(&linked, &out)?;
        Ok(Some(out))
    }
}

/// Whether two paths name the same file (hard links included).
pub fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (a.canonicalize(), b.canonicalize()) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}
