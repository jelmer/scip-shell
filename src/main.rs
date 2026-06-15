//! `scip-shell`: generate a SCIP code-intelligence index for shell scripts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser as ClapParser;
use scip::types::{Index, Metadata, ProtocolVersion, TextEncoding, ToolInfo};
use scip_shell::indexer::index_document;
use scip_shell::resolve::PathResolver;
use scip_shell::symbols::PackageInfo;
use walkdir::WalkDir;

/// Generate a SCIP index for shell scripts.
#[derive(ClapParser, Debug)]
#[command(name = "scip-shell", version, about)]
struct Args {
    /// Files or directories to index. Directories are searched recursively for
    /// shell scripts.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Project root. Document paths are recorded relative to this. Defaults to
    /// the current directory.
    #[arg(long)]
    project_root: Option<PathBuf>,

    /// Output path for the SCIP index.
    #[arg(short, long, default_value = "index.scip")]
    output: PathBuf,

    /// Package name recorded in emitted symbols.
    #[arg(long, default_value = "shell-project")]
    package_name: String,

    /// Package version recorded in emitted symbols.
    #[arg(long, default_value = "0.0.0")]
    package_version: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let project_root = match &args.project_root {
        Some(root) => root.clone(),
        None => std::env::current_dir().context("determining current directory")?,
    };
    let project_root = project_root
        .canonicalize()
        .with_context(|| format!("resolving project root {}", project_root.display()))?;

    let pkg = PackageInfo {
        name: args.package_name.clone(),
        version: args.package_version.clone(),
    };
    let resolver = PathResolver::from_env();

    let files = collect_files(&args.inputs)?;

    let mut documents = Vec::new();
    for file in &files {
        let relative = relative_path(&project_root, file);
        // A file can carry a shell shebang yet hold binary data after it (e.g.
        // self-extracting scripts), so reading it as UTF-8 may fail. Skip such
        // files rather than aborting the whole run.
        // TODO: support non-UTF-8 files, e.g. scripts in legacy encodings, by
        // reading bytes and decoding with a fallback rather than skipping them.
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("skipping {}: {e}", file.display());
                continue;
            }
        };

        match index_document(&pkg, &resolver, &relative, &text) {
            Ok(document) => documents.push(document),
            Err(e) => eprintln!("skipping {}: {e:#}", file.display()),
        }
    }

    let index = Index {
        metadata: Some(Metadata {
            version: ProtocolVersion::UnspecifiedProtocolVersion.into(),
            tool_info: Some(ToolInfo {
                name: env!("CARGO_PKG_NAME").to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                arguments: std::env::args().skip(1).collect(),
                ..Default::default()
            })
            .into(),
            project_root: path_to_uri(&project_root),
            text_document_encoding: TextEncoding::UTF8.into(),
            ..Default::default()
        })
        .into(),
        documents,
        ..Default::default()
    };

    scip::write_message_to_file(&args.output, index)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", args.output.display()))?;

    eprintln!(
        "wrote {} document(s) to {}",
        files.len(),
        args.output.display()
    );
    Ok(())
}

/// Walk the input paths, collecting shell script files. Files passed explicitly
/// are always included; directories are searched for recognised extensions.
fn collect_files(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_dir() {
            for entry in WalkDir::new(input).into_iter().filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file() && is_shell_script(path) {
                    files.push(path.to_path_buf());
                }
            }
        } else {
            files.push(input.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Heuristic for recognising shell scripts found during directory traversal.
/// Files with a known shell extension are accepted outright; extensionless files
/// are accepted when they begin with a shell shebang.
fn is_shell_script(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("sh" | "bash" | "ksh" | "zsh") => true,
        Some(_) => false,
        None => has_shell_shebang(path),
    }
}

/// Whether a file's first line is a `#!` line naming a shell interpreter.
fn has_shell_shebang(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut first_line = String::new();
    if std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first_line).is_err() {
        return false;
    }
    let Some(rest) = first_line.strip_prefix("#!") else {
        return false;
    };
    // The interpreter is the last path component of the first shebang token,
    // accounting for `/usr/bin/env bash`.
    let mut tokens = rest.split_whitespace();
    let interpreter = tokens.next().unwrap_or_default();
    let basename = interpreter.rsplit('/').next().unwrap_or(interpreter);
    let candidate = if basename == "env" {
        tokens.next().unwrap_or("")
    } else {
        basename
    };
    matches!(candidate, "sh" | "bash" | "ksh" | "zsh" | "dash")
}

/// Compute the document path relative to the project root, falling back to the
/// original path (lossily) if it lies outside the root.
fn relative_path(root: &Path, file: &Path) -> String {
    let canonical = file.canonicalize();
    let resolved = canonical.as_deref().unwrap_or(file);
    resolved
        .strip_prefix(root)
        .unwrap_or(resolved)
        .to_string_lossy()
        .into_owned()
}

/// Render a filesystem path as a `file://` URI for `Metadata.project_root`.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.to_string_lossy())
}
