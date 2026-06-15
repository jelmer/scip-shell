//! Classification of command words and arguments into binaries and paths.

use std::collections::HashSet;
use std::path::Path;

/// Decides whether a command name resolves to an executable, and whether a word
/// is a filesystem path. Kept behind a trait so tests can supply a deterministic
/// set of "known" binaries instead of depending on the host's `$PATH`.
pub trait Resolver {
    /// Whether `name` is an executable reachable without a path component (i.e.
    /// found on `$PATH`). `name` is the bare command word, e.g. `grep`.
    fn is_binary_on_path(&self, name: &str) -> bool;
}

/// Resolver backed by the process's real `$PATH`.
pub struct PathResolver {
    executables: HashSet<String>,
}

impl PathResolver {
    /// Scan every directory on `$PATH`, recording the names of executable files.
    pub fn from_env() -> Self {
        let mut executables = HashSet::new();
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                collect_executables(&dir, &mut executables);
            }
        }
        Self { executables }
    }
}

impl Resolver for PathResolver {
    fn is_binary_on_path(&self, name: &str) -> bool {
        self.executables.contains(name)
    }
}

fn collect_executables(dir: &Path, out: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if is_executable_file(&entry) {
            if let Some(name) = entry.file_name().to_str() {
                out.insert(name.to_string());
            }
        }
    }
}

#[cfg(unix)]
fn is_executable_file(entry: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    // A regular or symlinked file with any execute bit set.
    !metadata.is_dir() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(entry: &std::fs::DirEntry) -> bool {
    entry.metadata().map(|m| !m.is_dir()).unwrap_or(false)
}

/// Whether a command word refers to a program by an explicit path rather than a
/// `$PATH` lookup. Such a word should be treated as a filesystem path, not a
/// `$PATH` binary.
pub fn has_path_component(word: &str) -> bool {
    word.contains('/')
}

/// Whether a word denotes an absolute filesystem path.
///
/// Only absolute paths (starting with `/`) are recognised, to avoid mistaking
/// flags, regular expressions, or option values for paths. A word that begins
/// with `//` or that looks like a URL scheme (`http://`) is rejected.
pub fn is_absolute_path(word: &str) -> bool {
    if !word.starts_with('/') {
        return false;
    }
    // `//foo` is unusual and more likely a doubled separator or a comment-like
    // token than a real path reference.
    if word.starts_with("//") {
        return false;
    }
    // Reject anything carrying a URL-ish scheme separator.
    !word.contains("://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_recognised() {
        assert!(is_absolute_path("/usr/share/dict/words"));
        assert!(is_absolute_path("/etc/passwd"));
        assert!(is_absolute_path("/"));
    }

    #[test]
    fn non_absolute_rejected() {
        assert!(!is_absolute_path("relative/path"));
        assert!(!is_absolute_path("./local"));
        assert!(!is_absolute_path("-flag"));
        assert!(!is_absolute_path("plain"));
        assert!(!is_absolute_path("//doubled"));
        assert!(!is_absolute_path("file:///etc/passwd"));
    }

    #[test]
    fn path_component_detection() {
        assert!(has_path_component("/usr/bin/grep"));
        assert!(has_path_component("./script.sh"));
        assert!(!has_path_component("grep"));
    }
}
