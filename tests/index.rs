//! End-to-end tests: index a script and inspect the emitted SCIP document.

use scip::types::symbol_information::Kind;
use scip::types::{SymbolRole, SyntaxKind};
use scip_shell::indexer::index_document;
use scip_shell::resolve::Resolver;
use scip_shell::symbols::PackageInfo;

fn pkg() -> PackageInfo {
    PackageInfo {
        name: "test-pkg".to_string(),
        version: "1.0".to_string(),
    }
}

/// A resolver with a fixed set of known binaries, so tests do not depend on the
/// host's `$PATH`.
struct FakeResolver(&'static [&'static str]);

impl Resolver for FakeResolver {
    fn is_binary_on_path(&self, name: &str) -> bool {
        self.0.contains(&name)
    }
}

/// Index a document with no known binaries.
fn index(text: &str) -> scip::types::Document {
    index_document(&pkg(), &FakeResolver(&[]), "script.sh", text).unwrap()
}

/// Index a document with the given set of known binaries.
fn index_with_binaries(text: &str, binaries: &'static [&'static str]) -> scip::types::Document {
    index_document(&pkg(), &FakeResolver(binaries), "script.sh", text).unwrap()
}

/// Find the single occurrence whose range starts at the given 0-based line and
/// character. Panics if there is not exactly one.
fn occurrence_at(
    doc: &scip::types::Document,
    line: i32,
    character: i32,
) -> &scip::types::Occurrence {
    let matches: Vec<_> = doc
        .occurrences
        .iter()
        .filter(|o| o.range.first() == Some(&line) && o.range.get(1) == Some(&character))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one occurrence at {line}:{character}, found {}: {:#?}",
        matches.len(),
        doc.occurrences
    );
    matches[0]
}

#[test]
fn function_definition_and_call() {
    let text = "greet() {\n    echo hi\n}\ngreet\n";
    let doc = index(text);

    // The function name on line 0 is a definition.
    let def = occurrence_at(&doc, 0, 0);
    assert_eq!(
        def.symbol_roles & SymbolRole::Definition as i32,
        SymbolRole::Definition as i32
    );
    let func_symbol = def.symbol.clone();
    assert!(
        func_symbol.contains("greet"),
        "symbol should mention the function name: {func_symbol}"
    );

    // The call on line 3 references the same symbol and is not a definition.
    let call = occurrence_at(&doc, 3, 0);
    assert_eq!(call.symbol, func_symbol);
    assert_eq!(call.symbol_roles & SymbolRole::Definition as i32, 0);

    // The function is described once in the document's symbol table.
    let described: Vec<_> = doc
        .symbols
        .iter()
        .filter(|s| s.symbol == func_symbol)
        .collect();
    assert_eq!(described.len(), 1);
    assert_eq!(described[0].display_name, "greet");
}

#[test]
fn call_before_definition_is_linked() {
    // A call that precedes the definition still resolves: shell looks the
    // function up at call time, by which point it has been defined.
    let text = "main\ngreet() {\n    echo hi\n}\nmain() {\n    greet\n}\n";
    let doc = index(text);

    let def = occurrence_at(&doc, 4, 0);
    assert_eq!(
        def.symbol_roles & SymbolRole::Definition as i32,
        SymbolRole::Definition as i32
    );
    // The forward call on line 0 links to the `main` defined on line 4.
    let call = occurrence_at(&doc, 0, 0);
    assert_eq!(call.symbol, def.symbol);
    assert_eq!(
        call.syntax_kind.enum_value(),
        Ok(SyntaxKind::IdentifierFunction)
    );
}

#[test]
fn function_shadows_a_same_named_binary() {
    // A function takes precedence over a binary of the same name, even when the
    // call comes before the definition.
    let text = "echo hi\necho() {\n    :\n}\n";
    let doc = index_with_binaries(text, &["echo"]);

    let def = occurrence_at(&doc, 1, 0).symbol.clone();
    let call = occurrence_at(&doc, 0, 0);
    assert_eq!(call.symbol, def);
    assert!(
        call.symbol.contains("echo()"),
        "call should link to the function, not the binary: {}",
        call.symbol
    );
}

#[test]
fn variable_assignment_and_reference() {
    let text = "NAME=world\necho \"$NAME\"\n";
    let doc = index(text);

    // Assignment of NAME at line 0, column 0.
    let def = occurrence_at(&doc, 0, 0);
    assert_eq!(
        def.symbol_roles & SymbolRole::Definition as i32,
        SymbolRole::Definition as i32
    );
    assert_eq!(
        def.symbol_roles & SymbolRole::WriteAccess as i32,
        SymbolRole::WriteAccess as i32
    );
    let var_symbol = def.symbol.clone();
    assert!(var_symbol.starts_with("local "));

    // Reference inside the echo on line 1. `echo "$NAME"` -> $ at column 6,
    // name starts at column 7.
    let read = occurrence_at(&doc, 1, 7);
    assert_eq!(read.symbol, var_symbol);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn reference_inside_assignment_value() {
    // The value word of an assignment carries no location of its own, so
    // references in it are recovered from the full `name=value` word. `local
    // x="$Y"` -> $ at column 9, name starts at column 10.
    let text = "Y=1\nlocal x=\"$Y\"\n";
    let doc = index(text);

    let y_def = occurrence_at(&doc, 0, 0);
    let y_symbol = y_def.symbol.clone();

    let y_read = occurrence_at(&doc, 1, 10);
    assert_eq!(y_read.symbol, y_symbol);
    assert_eq!(
        y_read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn absolute_path_on_assignment_value_is_indexed() {
    // `CONF=/etc/app.conf` -> the value starts at column 5.
    let doc = index("CONF=/etc/app.conf\n");

    let path = occurrence_at(&doc, 0, 5);
    assert!(
        path.symbol.contains("filesystem") && path.symbol.contains("/etc/app.conf"),
        "expected a filesystem path symbol, got {}",
        path.symbol
    );
    assert_eq!(path.syntax_kind.enum_value(), Ok(SyntaxKind::StringLiteral));
}

#[test]
fn document_round_trips_through_protobuf() {
    use protobuf::Message;

    let text = "X=1\necho $X\n";
    let doc = index(text);

    let index = scip::types::Index {
        documents: vec![doc],
        ..Default::default()
    };
    let bytes = index.write_to_bytes().unwrap();
    let parsed = scip::types::Index::parse_from_bytes(&bytes).unwrap();

    assert_eq!(parsed.documents.len(), 1);
    assert_eq!(parsed.documents[0].relative_path, "script.sh");
    assert!(!parsed.documents[0].occurrences.is_empty());
}

#[test]
fn occurrences_carry_syntax_kinds() {
    let text = "greet() {\n    echo hi\n}\nNAME=x\necho \"$NAME\"\n";
    let doc = index(text);

    // Function definition.
    let def = occurrence_at(&doc, 0, 0);
    assert_eq!(
        def.syntax_kind.enum_value(),
        Ok(SyntaxKind::IdentifierFunctionDefinition)
    );

    // Variable definition and reference are both local identifiers.
    let var_def = occurrence_at(&doc, 3, 0);
    assert_eq!(
        var_def.syntax_kind.enum_value(),
        Ok(SyntaxKind::IdentifierLocal)
    );
    let var_read = occurrence_at(&doc, 4, 7);
    assert_eq!(
        var_read.syntax_kind.enum_value(),
        Ok(SyntaxKind::IdentifierLocal)
    );
}

#[test]
fn binary_on_path_is_indexed() {
    let text = "grep foo file\n";
    let doc = index_with_binaries(text, &["grep"]);

    let occ = occurrence_at(&doc, 0, 0);
    assert!(
        occ.symbol.contains("system") && occ.symbol.contains("grep"),
        "expected a system binary symbol, got {}",
        occ.symbol
    );
    assert_eq!(
        occ.syntax_kind.enum_value(),
        Ok(SyntaxKind::IdentifierBuiltin)
    );

    let described = doc.symbols.iter().find(|s| s.symbol == occ.symbol).unwrap();
    assert_eq!(described.display_name, "grep");
}

#[test]
fn unknown_command_is_not_a_binary() {
    // A command word that is neither a known function nor a known binary, and
    // has no path component, produces no command-name occurrence.
    let doc = index_with_binaries("definitelynotreal arg\n", &["grep"]);
    assert!(doc.occurrences.is_empty(), "{:#?}", doc.occurrences);
}

#[test]
fn absolute_path_argument_is_indexed() {
    let text = "cat /usr/share/dict/words\n";
    let doc = index_with_binaries(text, &["cat"]);

    // The path argument starts at column 4.
    let occ = occurrence_at(&doc, 0, 4);
    assert!(
        occ.symbol.contains("filesystem") && occ.symbol.contains("/usr/share/dict/words"),
        "expected a filesystem path symbol, got {}",
        occ.symbol
    );
    assert_eq!(occ.syntax_kind.enum_value(), Ok(SyntaxKind::StringLiteral));

    let described = doc.symbols.iter().find(|s| s.symbol == occ.symbol).unwrap();
    assert_eq!(described.kind.enum_value(), Ok(Kind::File));
    assert_eq!(described.display_name, "/usr/share/dict/words");
}

#[test]
fn absolute_path_as_command_is_indexed() {
    // A program invoked by absolute path is treated as a filesystem path.
    let doc = index("/bin/ls -l\n");
    let occ = occurrence_at(&doc, 0, 0);
    assert!(
        occ.symbol.contains("filesystem") && occ.symbol.contains("/bin/ls"),
        "expected a filesystem path symbol, got {}",
        occ.symbol
    );
}

#[test]
fn relative_path_is_not_indexed() {
    // `./build.sh` has a path component but is not absolute, so it gets no
    // command-name occurrence.
    let doc = index("./build.sh\n");
    assert!(doc.occurrences.is_empty(), "{:#?}", doc.occurrences);
}

#[test]
fn sourced_file_is_a_source_reference() {
    let doc = index("source ./lib.sh\n. /etc/profile\n");

    let relative = occurrence_at(&doc, 0, 7);
    assert!(
        relative.symbol.contains("source") && relative.symbol.contains("./lib.sh"),
        "expected a source symbol, got {}",
        relative.symbol
    );
    assert_eq!(
        relative.syntax_kind.enum_value(),
        Ok(SyntaxKind::StringLiteral)
    );
    let described = doc
        .symbols
        .iter()
        .find(|s| s.symbol == relative.symbol)
        .unwrap();
    assert_eq!(described.kind.enum_value(), Ok(Kind::File));
    assert_eq!(described.display_name, "./lib.sh");

    // The `.` form is also a source reference.
    let dotted = occurrence_at(&doc, 1, 2);
    assert!(dotted.symbol.contains("source") && dotted.symbol.contains("/etc/profile"));
}

#[test]
fn sourcing_a_variable_is_not_a_static_reference() {
    // `source "$lib"` cannot be linked to a file, so no source symbol; the
    // variable read is still recorded.
    let doc = index("lib=/tmp/x\nsource \"$lib\"\n");
    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("source .")),
        "{:#?}",
        doc.occurrences
    );
    // `$lib` is read at line 1; `source "$lib"` -> $ at col 8, name at col 9.
    let read = occurrence_at(&doc, 1, 9);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn redirect_target_is_indexed() {
    let text = "echo hi > /tmp/out.log\n";
    let doc = index_with_binaries(text, &["echo"]);

    // `/tmp/out.log` begins at column 10.
    let occ = occurrence_at(&doc, 0, 10);
    assert!(
        occ.symbol.contains("filesystem") && occ.symbol.contains("/tmp/out.log"),
        "expected a filesystem path symbol, got {}",
        occ.symbol
    );
}

#[test]
fn redirect_target_variable_is_indexed() {
    let text = "LOG=/tmp/x\necho hi >> \"$LOG\"\n";
    let doc = index_with_binaries(text, &["echo"]);

    let def = occurrence_at(&doc, 0, 0);
    // `echo hi >> "$LOG"` -> $ at col 12, name at col 13.
    let read = occurrence_at(&doc, 1, 13);
    assert_eq!(read.symbol, def.symbol);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn function_local_variable_is_scoped() {
    // `name` is local to the function and must not share a symbol with a
    // file-level `name` assignment.
    let text = "name=outer\nf() {\n    local name=inner\n    echo \"$name\"\n}\n";
    let doc = index(text);

    let outer = occurrence_at(&doc, 0, 0).symbol.clone();
    let inner_def = occurrence_at(&doc, 2, 10).symbol.clone();
    assert_ne!(
        outer, inner_def,
        "function-local name should differ from the global name"
    );

    // The reference inside the function resolves to the local, not the global.
    // `echo "$name"` indented 4 -> $ at col 10, name at col 11.
    let read = occurrence_at(&doc, 3, 11);
    assert_eq!(read.symbol, inner_def);
    assert_ne!(read.symbol, outer);
}

#[test]
fn declare_at_global_scope_links_to_the_global() {
    // `declare`/`typeset` only introduce locals inside a function. At file
    // scope they touch the global, so a reference either side must share the
    // symbol, including a read that precedes the declaration.
    let text = "echo \"$X\"\ndeclare X=1\necho \"$X\"\n";
    let doc = index(text);

    // `echo "$X"` -> $ at col 6, name at col 7.
    let read_before = occurrence_at(&doc, 0, 7).symbol.clone();
    let def = occurrence_at(&doc, 1, 8).symbol.clone();
    let read_after = occurrence_at(&doc, 2, 7).symbol.clone();
    assert_eq!(read_before, def);
    assert_eq!(def, read_after);
}

#[test]
fn global_assignment_after_function_links_to_inner_reference() {
    // A reference to a non-local name inside a function resolves to the global.
    let text = "f() {\n    echo \"$G\"\n}\nG=value\n";
    let doc = index(text);

    // `echo "$G"` -> $ at col 10, name at col 11.
    let read = occurrence_at(&doc, 1, 11).symbol.clone();
    let def = occurrence_at(&doc, 3, 0).symbol.clone();
    assert_eq!(read, def);
}

#[test]
fn shebang_interpreter_is_indexed() {
    let doc = index("#!/bin/bash\necho hi\n");
    // `/bin/bash` starts at column 2.
    let occ = occurrence_at(&doc, 0, 2);
    assert!(
        occ.symbol.contains("filesystem") && occ.symbol.contains("/bin/bash"),
        "expected a filesystem path symbol, got {}",
        occ.symbol
    );
}

#[test]
fn command_substitution_is_indexed() {
    let text = "x=$(grep foo /etc/passwd)\n";
    let doc = index_with_binaries(text, &["grep"]);

    // `grep` inside `$(...)` starts at column 4.
    let grep = occurrence_at(&doc, 0, 4);
    assert!(
        grep.symbol.contains("system") && grep.symbol.contains("grep"),
        "expected grep to be indexed inside the substitution, got {}",
        grep.symbol
    );

    // The path argument inside the substitution is indexed too.
    let path = occurrence_at(&doc, 0, 13);
    assert!(path.symbol.contains("/etc/passwd"), "{}", path.symbol);
}

#[test]
fn backtick_substitution_is_indexed() {
    let doc = index_with_binaries("echo `date`\n", &["echo", "date"]);
    // `date` inside backticks starts at column 6.
    let occ = occurrence_at(&doc, 0, 6);
    assert!(occ.symbol.contains("date"), "{}", occ.symbol);
}

#[test]
fn arithmetic_variable_is_read() {
    let text = "n=1\necho $((n + count))\n";
    let doc = index_with_binaries(text, &["echo"]);

    let n_def = occurrence_at(&doc, 0, 0).symbol.clone();
    // `echo $((n + count))` -> `n` at col 8.
    let n_read = occurrence_at(&doc, 1, 8);
    assert_eq!(n_read.symbol, n_def);
    assert_eq!(
        n_read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
    // `count` at col 12 is a fresh global read.
    let count_read = occurrence_at(&doc, 1, 12);
    assert_eq!(
        count_read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn standalone_arithmetic_command_reads_variables() {
    // `(( ... ))` as a command also has its variable reads indexed.
    let text = "count=0\n(( count += 1 ))\n(( total = count * 2 ))\n";
    let doc = index(text);

    let count_def = occurrence_at(&doc, 0, 0).symbol.clone();
    // `(( count += 1 ))` -> `count` at col 3.
    let count_read = occurrence_at(&doc, 1, 3);
    assert_eq!(count_read.symbol, count_def);
    assert_eq!(
        count_read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
    // `(( total = count * 2 ))` -> the second `count` at col 11 links too.
    let count_read2 = occurrence_at(&doc, 2, 11);
    assert_eq!(count_read2.symbol, count_def);
}

#[test]
fn standalone_arithmetic_command_reads_dollar_names() {
    // A `$name` inside a standalone `(( ... ))` is read like any reference.
    let text = "n=1\n(( x = $n + 1 ))\n";
    let doc = index(text);

    let n_def = occurrence_at(&doc, 0, 0).symbol.clone();
    // `(( x = $n + 1 ))` -> `$n` has `$` at col 7, name at col 8.
    let n_read = occurrence_at(&doc, 1, 8);
    assert_eq!(n_read.symbol, n_def);
}

#[test]
fn function_doc_comment_is_captured() {
    let text = "# Does a thing.\n# Across two lines.\nwork() {\n    echo hi\n}\n";
    let doc = index(text);

    let described = doc
        .symbols
        .iter()
        .find(|s| s.display_name == "work")
        .unwrap();
    assert_eq!(
        described.documentation,
        vec!["Does a thing.\nAcross two lines.".to_string()]
    );
}

#[test]
fn here_document_body_references_are_indexed() {
    let text = "cat <<EOF\nhello $NAME\nEOF\n";
    let doc = index_with_binaries(text, &["cat"]);

    // The body line `hello $NAME` is line 1; `$NAME` -> $ at col 6, name at 7.
    let read = occurrence_at(&doc, 1, 7);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn local_inside_command_substitution_does_not_leak() {
    // `$(...)` runs in a subshell, so a `local` declared inside it must not be
    // visible to a later read in the enclosing function body.
    let text = "f() {\n  y=$(local z; echo $z)\n  echo $z\n}\n";
    let doc = index(text);

    // The `local z` definition is inside the substitution on line 1.
    let inner = occurrence_at(&doc, 1, 12).symbol.clone();
    // `echo $z` on line 2 (outside the substitution): $ at col 7, name at 8.
    let outer = occurrence_at(&doc, 2, 8).symbol.clone();
    assert_ne!(
        inner, outer,
        "a subshell-local must not leak into the enclosing scope"
    );
}

#[test]
fn function_in_command_substitution_does_not_leak() {
    // A function defined inside `$(...)` is confined to the subshell and must not
    // resolve a later top-level call.
    let text = "x=$(g() { :; }; g)\ng\n";
    let doc = index_with_binaries(text, &["g"]);

    // The top-level `g` on line 1 is not a known function, so it falls back to
    // the binary resolver rather than linking to the in-substitution definition.
    let call = occurrence_at(&doc, 1, 0);
    assert!(
        call.symbol.contains("system"),
        "top-level g should resolve to a binary, not the subshell function: {}",
        call.symbol
    );
}

#[test]
fn source_target_after_a_redirect_is_indexed() {
    // A redirect can precede the file argument; the source link must still fire.
    let doc = index("source 2>/dev/null /etc/profile\n");

    let target = doc
        .occurrences
        .iter()
        .find(|o| o.symbol.contains("source") && o.symbol.contains("/etc/profile"));
    assert!(
        target.is_some(),
        "expected a source symbol for /etc/profile, got {:#?}",
        doc.occurrences
    );
}

#[test]
fn duplicate_redirect_target_is_not_a_path() {
    // `>&/dev/fd/3` names a file descriptor, not a file to index as a path.
    let doc = index_with_binaries("echo hi >&/dev/fd/3\n", &["echo"]);

    let spurious = doc
        .occurrences
        .iter()
        .any(|o| o.symbol.contains("filesystem") && o.symbol.contains("/dev/fd/3"));
    assert!(
        !spurious,
        "fd duplicate target must not be indexed as a filesystem path: {:#?}",
        doc.occurrences
    );
}

#[test]
fn line_continuation_keeps_reference_on_the_right_line() {
    // brush collapses `\<newline>` in the word value; the reference must still be
    // anchored to its real source line and column.
    let text = "echo foo\\\nbar$VAR\n";
    let doc = index_with_binaries(text, &["echo"]);

    // `bar$VAR` is on physical line 1; `$VAR` -> $ at col 3, name at col 4.
    let read = occurrence_at(&doc, 1, 4);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn nested_multiline_substitution_is_positioned_correctly() {
    // The inner `$(date)` is re-parsed against the fragment, but its occurrence
    // must land at its real document position, not drift onto another line.
    let text = "result=$(\n  echo $(date)\n)\n";
    let doc = index_with_binaries(text, &["echo", "date"]);

    // `  echo $(date)` is line 1; `date` runs cols 9..13.
    let date = occurrence_at(&doc, 1, 9);
    assert!(date.symbol.contains("date"), "{}", date.symbol);
    let echo = occurrence_at(&doc, 1, 2);
    assert!(echo.symbol.contains("echo"), "{}", echo.symbol);
}

#[test]
fn command_substitution_inside_arithmetic_command_is_indexed() {
    // `$(...)` inside a standalone `(( ... ))` must be re-parsed and indexed.
    let text = "(( x = $(id -u) ))\n";
    let doc = index_with_binaries(text, &["id"]);

    // `id` inside the substitution starts at col 9.
    let id = occurrence_at(&doc, 0, 9);
    assert!(
        id.symbol.contains("system") && id.symbol.contains("id"),
        "expected id to be indexed inside the arithmetic command, got {}",
        id.symbol
    );
}

#[test]
fn arithmetic_command_with_line_continuation_is_indexed() {
    // A `\<newline>` continuation collapses the expression value; the read must
    // still be located rather than silently dropped.
    let text = "(( count\\\n += 1 ))\n";
    let doc = index(text);

    // `count` is on line 0 at cols 3..8.
    let count = occurrence_at(&doc, 0, 3);
    assert_eq!(
        count.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn process_substitution_does_not_leak_locals() {
    // `<(...)` runs in a subshell, so a `local` inside it must not be visible to
    // a later read in the enclosing function.
    let text = "f() {\n  cat <(local z=1; echo $z)\n  echo $z\n}\n";
    let doc = index_with_binaries(text, &["cat", "echo"]);

    // `local z=1` is the definition inside the process substitution on line 1
    // (`  cat <(local z...` -> `z` at col 14).
    let inner = occurrence_at(&doc, 1, 14).symbol.clone();
    // `echo $z` on line 2 (outside) -> $ at col 7, name at 8.
    let outer = occurrence_at(&doc, 2, 8).symbol.clone();
    assert_ne!(
        inner, outer,
        "a process-substitution local must not leak into the enclosing scope"
    );
}

#[test]
fn quoted_absolute_path_on_assignment_value_is_indexed() {
    // `CONF="/etc/app.conf"` -> the path inside the quotes starts at col 6.
    let doc = index("CONF=\"/etc/app.conf\"\n");

    let path = occurrence_at(&doc, 0, 6);
    assert!(
        path.symbol.contains("filesystem") && path.symbol.contains("/etc/app.conf"),
        "expected a filesystem path symbol, got {}",
        path.symbol
    );
    // The range covers the path only, not the surrounding quotes.
    assert_eq!(path.range, vec![0, 6, 19]);
}

#[test]
fn quoted_source_target_excludes_the_quotes() {
    // `. "lib.sh"` -> the source symbol is the bare path, and the range covers
    // the path inside the quotes (col 3..9), not the quotes themselves.
    let doc = index(". \"lib.sh\"\n");

    let target = occurrence_at(&doc, 0, 3);
    assert!(
        target.symbol.contains("source") && target.symbol.ends_with("`lib.sh`."),
        "expected a clean source symbol, got {}",
        target.symbol
    );
    assert_eq!(target.range, vec![0, 3, 9]);
}

#[test]
fn occurrences_are_sorted_by_range() {
    // A forward call is resolved at the end of the document, but the emitted
    // occurrences must still come out in ascending range order (SCIP canonical
    // form), not appended after everything else.
    let text = "foo\nX=1\nfoo() {\n    :\n}\n";
    let doc = index(text);

    let keys: Vec<&Vec<i32>> = doc.occurrences.iter().map(|o| &o.range).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "occurrences should be sorted by range");
}

#[test]
fn multibyte_character_before_a_reference_does_not_corrupt_ranges() {
    // brush's source index is a character offset, not a byte offset. A multibyte
    // character on an earlier line must not shift or panic the range walk: the
    // `$x` read on the ASCII line 2 still lands at its (byte == char) column.
    let text = "# \u{e9}\u{e9}\nx=1\n(( x = $x + 1 ))\n";
    let doc = index(text);

    // The `$x` read: `$` at col 7, name `x` at col 8.
    let read = occurrence_at(&doc, 2, 8);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
    assert_eq!(read.range, vec![2, 8, 9]);
    // The bare `x` earlier in the expression is read too.
    let bare = occurrence_at(&doc, 2, 3);
    assert_eq!(bare.range, vec![2, 3, 4]);
}

#[test]
fn reference_after_a_same_line_multibyte_char_uses_byte_columns() {
    // The document declares UTF8CodeUnitOffsetFromLineStart, so a multibyte char
    // earlier on the line widens the columns of a following reference. In
    // `echo "é$NAME"`, `é` is 2 bytes, so $NAME's name lands at byte column 9
    // (not 8, which is where a character count would put it).
    let text = "NAME=x\necho \"\u{e9}$NAME\"\n";
    let doc = index(text);

    let read = occurrence_at(&doc, 1, 9);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
    assert_eq!(read.range, vec![1, 9, 13]);
}

#[test]
fn multibyte_character_inside_a_path_is_indexed() {
    // Columns are UTF-8 byte offsets (the encoding the document declares), so the
    // multibyte `é` widens the range. `cat "` is 5 bytes, then `/étc/passwd` is 12
    // bytes (`é` counts as 2), so the path covers byte columns 5..17.
    let text = "cat \"/\u{e9}tc/passwd\"\n";
    let doc = index_with_binaries(text, &["cat"]);

    let path = occurrence_at(&doc, 0, 5);
    assert_eq!(path.symbol, "scip-shell filesystem . . `/\u{e9}tc/passwd`.");
    assert_eq!(path.range, vec![0, 5, 17]);
}

#[test]
fn quoted_absolute_path_argument_is_indexed() {
    // A quoted path passed as a plain argument is indexed just like an unquoted
    // one, with the surrounding quotes excluded from the range.
    let doc = index_with_binaries("cat \"/etc/passwd\"\n", &["cat"]);

    let path = occurrence_at(&doc, 0, 5);
    assert_eq!(path.symbol, "scip-shell filesystem . . `/etc/passwd`.");
    assert_eq!(path.range, vec![0, 5, 16]);
}

#[test]
fn quoted_path_with_an_expansion_is_not_a_static_path() {
    // A quoted argument that embeds an expansion (`"/etc/$x"`) is a runtime path,
    // not a static one, so it must not be indexed as a literal filesystem symbol;
    // only the `$x` read inside it is recorded.
    let doc = index_with_binaries("x=1\ncat \"/etc/$x\"\n", &["cat"]);

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "a path containing an expansion must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
    // The `$x` read is still indexed: `$` at col 10, name at col 11.
    let read = occurrence_at(&doc, 1, 11);
    assert_eq!(
        read.symbol_roles & SymbolRole::ReadAccess as i32,
        SymbolRole::ReadAccess as i32
    );
}

#[test]
fn assignment_value_with_an_expansion_is_not_a_static_path() {
    // The same guard applies to an assignment value: `CONF="/etc/$x"` writes a
    // runtime path, so no filesystem symbol is emitted for it.
    let doc = index("x=1\nCONF=\"/etc/$x\"\n");

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "an assignment value with an expansion must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
}

#[test]
fn single_quoted_path_with_a_literal_dollar_is_a_static_path() {
    // A `$` inside single quotes is literal text, not an expansion, so the path
    // `/etc/$x` is a static path and is indexed (with the quotes excluded).
    let doc = index_with_binaries("cat '/etc/$x'\n", &["cat"]);

    let path = occurrence_at(&doc, 0, 5);
    assert_eq!(path.symbol, "scip-shell filesystem . . `/etc/$x`.");
    // `/etc/$x` is 7 characters; the range covers it without the quotes.
    assert_eq!(path.range, vec![0, 5, 12]);
}

#[test]
fn path_with_a_positional_parameter_is_not_a_static_path() {
    // `$1` is a positional parameter, expanded at runtime, so `/etc/$1` is not a
    // static path even though no user-defined variable symbol exists for `$1`.
    let doc = index_with_binaries("cat /etc/$1\n", &["cat"]);

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "a path with a positional parameter must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
}

#[test]
fn assignment_value_with_a_special_parameter_is_not_a_static_path() {
    // `$$` (the PID) is a special parameter expanded at runtime, so the assigned
    // value is not a static path.
    let doc = index("PIDFILE=/var/run/$$.pid\n");

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "a value with a special parameter must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
}

#[test]
fn glob_pattern_path_is_not_a_static_path() {
    // `/etc/*.conf` is a filename pattern expanded at runtime, not a fixed path,
    // so it is not indexed as a filesystem symbol.
    let doc = index_with_binaries("cat /etc/*.conf\n", &["cat"]);

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "a glob pattern must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
}

#[test]
fn single_quoted_glob_is_a_static_path() {
    // Inside single quotes a `*` is literal, so `'/etc/*.conf'` names a fixed
    // (if unusual) path and is indexed.
    let doc = index_with_binaries("cat '/etc/*.conf'\n", &["cat"]);

    let path = occurrence_at(&doc, 0, 5);
    assert_eq!(path.symbol, "scip-shell filesystem . . `/etc/*.conf`.");
}

#[test]
fn tilde_path_is_not_a_static_path() {
    // `~/foo` is home-directory expansion, not a fixed path. It is also not
    // absolute, so it yields no filesystem symbol either way.
    let doc = index_with_binaries("cat ~/foo\n", &["cat"]);

    assert!(
        !doc.occurrences
            .iter()
            .any(|o| o.symbol.contains("filesystem")),
        "a tilde path must not yield a filesystem symbol: {:#?}",
        doc.occurrences
    );
}

#[test]
fn double_quoted_path_with_an_apostrophe_is_a_static_path() {
    // A `'` inside double quotes is a literal apostrophe, not a quote delimiter,
    // so the path is static and is indexed.
    let doc = index_with_binaries("cat \"/etc/it's\"\n", &["cat"]);

    let path = occurrence_at(&doc, 0, 5);
    assert_eq!(path.symbol, "scip-shell filesystem . . `/etc/it's`.");
}

#[test]
fn function_in_command_substitution_has_no_misattributed_doc() {
    // A function defined inside a command substitution must not pick up the
    // comment above the enclosing statement: line numbers in the re-parsed
    // fragment are fragment-relative, so the document comment does not apply.
    let text = "# unrelated\nresult=$(\nfoo() { :; }\n)\n";
    let doc = index(text);

    let foo = doc
        .symbols
        .iter()
        .find(|s| s.display_name == "foo")
        .unwrap();
    assert_eq!(foo.documentation, Vec::<String>::new());
}
