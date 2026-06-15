//! Construction of SCIP symbol strings for shell constructs.

use scip::symbol::format_symbol;
use scip::types::descriptor::Suffix;
use scip::types::{Descriptor, Package, Symbol};

/// Identifies the package that all symbols in an index belong to. Shell scripts
/// have no real package manager, so this is synthesised from a caller-supplied
/// name and version.
#[derive(Clone, Debug)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
}

impl PackageInfo {
    fn to_package(&self) -> Package {
        Package {
            manager: "shell".to_string(),
            name: self.name.clone(),
            version: self.version.clone(),
            ..Default::default()
        }
    }
}

/// The indexing scheme reported in every symbol string.
const SCHEME: &str = "scip-shell";

/// Build a global symbol string for a shell function definition.
///
/// Functions are the one shell construct with stable, file-spanning identity, so
/// they get a global symbol with a `Method` descriptor (the closest SCIP suffix
/// for a callable named entity).
pub fn function_symbol(pkg: &PackageInfo, name: &str) -> String {
    let descriptor = Descriptor {
        name: name.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    };
    format_symbol(Symbol {
        scheme: SCHEME.to_string(),
        package: Some(pkg.to_package()).into(),
        descriptors: vec![descriptor],
        ..Default::default()
    })
}

/// Build a local symbol string for a variable.
///
/// Shell variables are dynamically scoped and resolved at runtime, so they have
/// no meaningful cross-file identity. Each distinct variable occurrence stream
/// within a document is assigned a stable local id by the indexer.
pub fn local_symbol(id: usize) -> String {
    format!("local {id}")
}

/// Build a global symbol for an executable found on `$PATH`.
///
/// Binaries live in a synthetic `system` package keyed by the bare name (not the
/// resolved path), so the same command cross-references across scripts regardless
/// of where it sits on a given host.
pub fn binary_symbol(name: &str) -> String {
    let descriptor = Descriptor {
        name: name.to_string(),
        suffix: Suffix::Term.into(),
        ..Default::default()
    };
    format_symbol(Symbol {
        scheme: SCHEME.to_string(),
        package: Some(synthetic_package("system")).into(),
        descriptors: vec![descriptor],
        ..Default::default()
    })
}

/// Build a global symbol for an absolute filesystem path, in a synthetic
/// `filesystem` package keyed by the path string.
pub fn path_symbol(path: &str) -> String {
    let descriptor = Descriptor {
        name: path.to_string(),
        suffix: Suffix::Term.into(),
        ..Default::default()
    };
    format_symbol(Symbol {
        scheme: SCHEME.to_string(),
        package: Some(synthetic_package("filesystem")).into(),
        descriptors: vec![descriptor],
        ..Default::default()
    })
}

/// Build a global symbol for a file pulled in with `source` / `.`, in a
/// synthetic `source` package keyed by the path as written. This is what links a
/// `source lib.sh` occurrence to the file it pulls in -- the one construct that
/// crosses file boundaries in shell.
pub fn source_symbol(path: &str) -> String {
    let descriptor = Descriptor {
        name: path.to_string(),
        suffix: Suffix::Term.into(),
        ..Default::default()
    };
    format_symbol(Symbol {
        scheme: SCHEME.to_string(),
        package: Some(synthetic_package("source")).into(),
        descriptors: vec![descriptor],
        ..Default::default()
    })
}

/// A package for symbols that are not part of the indexed project (binaries,
/// filesystem paths). These have no meaningful name or version of their own.
fn synthetic_package(manager: &str) -> Package {
    Package {
        manager: manager.to_string(),
        name: ".".to_string(),
        version: ".".to_string(),
        ..Default::default()
    }
}
