//! Binary-level tests for the file-collection and reading behaviour in `main`.

use std::io::Write;
use std::process::Command;

/// A file carrying a shell shebang but holding binary (non-UTF-8) data after it
/// must be skipped, not abort the run.
#[test]
fn non_utf8_shebang_file_is_skipped() {
    let dir = tempfile::tempdir().unwrap();

    // A valid script that should be indexed.
    std::fs::write(dir.path().join("good.sh"), "echo hi\n").unwrap();

    // A shebang file whose body is not valid UTF-8 (e.g. a self-extracting
    // script with an appended archive).
    let mut bad = std::fs::File::create(dir.path().join("bad")).unwrap();
    bad.write_all(b"#!/bin/bash\n\xff\xfe\x00binary\n").unwrap();
    drop(bad);

    let out = dir.path().join("out.scip");
    let status = Command::new(env!("CARGO_BIN_EXE_scip-shell"))
        .arg("--project-root")
        .arg(dir.path())
        .arg("-o")
        .arg(&out)
        .arg(dir.path())
        .status()
        .unwrap();

    assert!(
        status.success(),
        "run should succeed, skipping the binary file"
    );
    assert!(out.exists(), "an index should still be written");
}
