//! Walks a parsed shell program and emits SCIP occurrences and symbols.

use std::collections::HashMap;
use std::io::BufReader;

use anyhow::{Context, Result};
use brush_parser::ast;
use brush_parser::{Parser, ParserOptions, SourceSpan};
use scip::types::symbol_information::Kind;
use scip::types::{Document, Occurrence, SymbolInformation, SymbolRole, SyntaxKind};

use crate::expansions::{
    find_arithmetic_variables, find_expansions, has_dynamic_value, is_variable_name, Expansion,
};
use crate::range::{char_to_byte, span_to_range, subrange, subrange_stripping_tabs, Range};
use crate::resolve::{has_path_component, is_absolute_path, Resolver};
use crate::symbols::{
    binary_symbol, function_symbol, local_symbol, path_symbol, source_symbol, PackageInfo,
};

/// Names that introduce function-local variables in their assignment arguments.
const LOCAL_DECLARATION_BUILTINS: &[&str] = &["local", "declare", "typeset"];

/// Names that pull in another file, making their first argument a source
/// reference.
const SOURCE_BUILTINS: &[&str] = &["source", "."];

/// A lexical scope: the local symbol assigned to each variable name declared in
/// it. The bottom scope is the document's global scope; each function body
/// pushes another.
type Scope = HashMap<String, String>;

/// Indexes a single shell document into a SCIP [`Document`].
pub struct DocumentIndexer<'a> {
    pkg: &'a PackageInfo,
    resolver: &'a dyn Resolver,
    relative_path: String,
    text: String,
    /// The source string the spans currently being visited refer to. This is the
    /// document text, except while re-parsing a command-substitution fragment,
    /// when it is the fragment so its spans resolve against the right bytes.
    current_source: String,
    /// Nesting depth of command-substitution fragments currently being re-parsed.
    /// Non-zero while inside a fragment, where source positions are relative to
    /// the fragment rather than the document.
    fragment_depth: usize,
    /// Occurrences with their ranges still in typed form, so they can be sorted
    /// and shifted before being encoded into SCIP's vector form at the end.
    occurrences: Vec<RawOccurrence>,
    /// Definitions keyed by symbol, deduplicated so each symbol is described once.
    symbols: HashMap<String, SymbolInformation>,
    /// Function symbols keyed by function name, so calls resolve to them.
    functions: HashMap<String, String>,
    /// Command-name calls awaiting resolution. A plain command word is resolved
    /// after the whole document is walked, so a call can link to a function
    /// defined later in the file (shell allows forward references at call time).
    pending_calls: Vec<PendingCall>,
    /// Stack of variable scopes, innermost last. Index 0 is the global scope.
    scopes: Vec<Scope>,
    next_local_id: usize,
}

/// A command-name word whose target is resolved once all definitions are known.
struct PendingCall {
    name: String,
    range: Range,
}

/// An occurrence whose range is still typed, awaiting sort, shift and encoding
/// into SCIP's vector form.
struct RawOccurrence {
    range: Range,
    symbol: String,
    roles: i32,
    syntax: SyntaxKind,
}

/// Parse and index a shell document. `relative_path` is recorded in the SCIP
/// document and should be relative to the project root. `resolver` classifies
/// command words as `$PATH` binaries.
pub fn index_document(
    pkg: &PackageInfo,
    resolver: &dyn Resolver,
    relative_path: &str,
    text: &str,
) -> Result<Document> {
    let program = parse(text).with_context(|| format!("parsing shell script {relative_path}"))?;

    let mut indexer = DocumentIndexer {
        pkg,
        resolver,
        relative_path: relative_path.to_string(),
        text: text.to_string(),
        current_source: text.to_string(),
        fragment_depth: 0,
        occurrences: Vec::new(),
        symbols: HashMap::new(),
        functions: HashMap::new(),
        pending_calls: Vec::new(),
        scopes: vec![Scope::new()],
        next_local_id: 0,
    };
    indexer.visit_shebang(text);
    indexer.visit_program(&program);
    indexer.resolve_pending_calls();
    Ok(indexer.into_document())
}

fn parse(text: &str) -> Result<ast::Program> {
    let reader = BufReader::new(text.as_bytes());
    let mut parser = Parser::new(reader, &ParserOptions::default());
    parser.parse_program().map_err(|e| anyhow::anyhow!("{e}"))
}

impl DocumentIndexer<'_> {
    fn into_document(self) -> Document {
        let mut symbols: Vec<SymbolInformation> = self.symbols.into_values().collect();
        symbols.sort_by(|a, b| a.symbol.cmp(&b.symbol));

        // SCIP's canonical form expects occurrences in ascending range order.
        // Deferred call resolution appends out of order, so sort before encoding.
        let mut occurrences = self.occurrences;
        occurrences.sort_by_key(|o| o.range);
        let occurrences = occurrences
            .into_iter()
            .map(|o| Occurrence {
                range: o.range.to_scip(),
                symbol: o.symbol,
                symbol_roles: o.roles,
                syntax_kind: o.syntax.into(),
                ..Default::default()
            })
            .collect();

        Document {
            // SCIP's `Document.language` is a free-form string; the convention
            // is to use the `Language` enum variant name.
            language: "ShellScript".to_string(),
            relative_path: self.relative_path,
            occurrences,
            symbols,
            text: self.text,
            position_encoding: scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart
                .into(),
            ..Default::default()
        }
    }

    fn emit(&mut self, symbol: &str, range: Range, roles: i32, syntax: SyntaxKind) {
        self.occurrences.push(RawOccurrence {
            range,
            symbol: symbol.to_string(),
            roles,
            syntax,
        });
    }

    fn describe(&mut self, symbol: String, display_name: String, kind: Kind) {
        self.symbols
            .entry(symbol.clone())
            .or_insert_with(|| SymbolInformation {
                symbol,
                display_name,
                kind: kind.into(),
                ..Default::default()
            });
    }

    /// Resolve a variable name to a symbol for a *reference*: the innermost scope
    /// that declares it, falling back to the global scope (creating the binding
    /// there if the name has not been seen). Shell references that hit no `local`
    /// declaration resolve to the global variable.
    fn reference_symbol(&mut self, name: &str) -> String {
        for scope in self.scopes.iter().rev() {
            if let Some(symbol) = scope.get(name) {
                return symbol.clone();
            }
        }
        self.declare_in_global(name)
    }

    /// Declare a function-local variable in the innermost scope. Outside any
    /// function the innermost scope is the global one, where `declare` /
    /// `typeset` act on the global variable rather than introducing a new local,
    /// so this defers to [`Self::declare_in_global`] to reuse any existing
    /// binding. Re-declaring a name already local to the innermost scope (a
    /// second `local x` in the same function) refers to the same variable, so the
    /// existing binding is reused rather than splitting it across two symbols.
    fn declare_local(&mut self, name: &str) -> String {
        if self.scopes.len() == 1 {
            return self.declare_in_global(name);
        }
        let scope = self
            .scopes
            .last_mut()
            .expect("at least the global scope is present");
        if let Some(existing) = scope.get(name) {
            return existing.clone();
        }
        let symbol = local_symbol(self.next_local_id);
        self.next_local_id += 1;
        scope.insert(name.to_string(), symbol.clone());
        symbol
    }

    fn declare_in_global(&mut self, name: &str) -> String {
        if let Some(existing) = self.scopes[0].get(name) {
            return existing.clone();
        }
        let symbol = local_symbol(self.next_local_id);
        self.next_local_id += 1;
        self.scopes[0].insert(name.to_string(), symbol.clone());
        symbol
    }

    /// Index a `#!` shebang on the first line as a filesystem path occurrence
    /// for its interpreter.
    fn visit_shebang(&mut self, text: &str) {
        let Some(rest) = text.strip_prefix("#!") else {
            return;
        };
        let first_line = rest.split('\n').next().unwrap_or(rest);
        // The interpreter is the first whitespace-delimited token after `#!`.
        let leading_ws: String = first_line
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let interp: String = first_line
            .chars()
            .skip(leading_ws.chars().count())
            .take_while(|c| !c.is_whitespace())
            .collect();
        let interp = interp.as_str();
        if !is_absolute_path(interp) {
            return;
        }
        // Columns are UTF-8 byte offsets from the line start: 2 for `#!`, plus the
        // leading whitespace, then the interpreter token.
        let start = 2 + leading_ws.len() as i32;
        let range = Range {
            start_line: 0,
            start_char: start,
            end_line: 0,
            end_char: start + interp.len() as i32,
        };
        let symbol = path_symbol(interp);
        self.emit(&symbol, range, 0, SyntaxKind::StringLiteral);
        self.describe(symbol, interp.to_string(), Kind::File);
    }

    fn visit_program(&mut self, program: &ast::Program) {
        for command in &program.complete_commands {
            self.visit_compound_list(command);
        }
    }

    fn visit_compound_list(&mut self, list: &ast::CompoundList) {
        for item in &list.0 {
            self.visit_and_or_list(&item.0);
        }
    }

    /// Visit a subshell body (a process substitution `<(...)` / `>(...)`),
    /// isolating it like a command substitution: its `local` declarations and
    /// function definitions do not escape, and its calls resolve against the
    /// functions visible inside it. Unlike `visit_substitution` there is no
    /// re-parse, so the spans already refer to the document and need no shifting.
    fn visit_subshell(&mut self, list: &ast::CompoundList) {
        self.in_subshell_scope(|me| me.visit_compound_list(list));
    }

    /// Run `body` in an isolated subshell scope: a fresh variable scope and a
    /// snapshot of the visible functions, both restored afterwards, so `local`
    /// declarations and function definitions inside do not escape. The body's own
    /// pending calls are resolved while its functions are still in scope.
    fn in_subshell_scope(&mut self, body: impl FnOnce(&mut Self)) {
        let pending_before = self.pending_calls.len();
        let functions_before = self.functions.clone();
        self.scopes.push(Scope::new());
        body(self);
        self.resolve_pending_calls_from(pending_before);
        self.functions = functions_before;
        self.scopes.pop();
    }

    fn visit_and_or_list(&mut self, list: &ast::AndOrList) {
        self.visit_pipeline(&list.first);
        for and_or in &list.additional {
            match and_or {
                ast::AndOr::And(p) | ast::AndOr::Or(p) => self.visit_pipeline(p),
            }
        }
    }

    fn visit_pipeline(&mut self, pipeline: &ast::Pipeline) {
        for command in &pipeline.seq {
            self.visit_command(command);
        }
    }

    fn visit_command(&mut self, command: &ast::Command) {
        match command {
            ast::Command::Simple(simple) => self.visit_simple_command(simple),
            ast::Command::Compound(compound, redirects) => {
                self.visit_compound_command(compound);
                self.visit_redirects(redirects);
            }
            ast::Command::Function(func) => self.visit_function_definition(func),
            ast::Command::ExtendedTest(_, _) => {}
        }
    }

    fn visit_compound_command(&mut self, compound: &ast::CompoundCommand) {
        match compound {
            ast::CompoundCommand::BraceGroup(b) => self.visit_compound_list(&b.list),
            ast::CompoundCommand::Subshell(s) => self.visit_compound_list(&s.list),
            ast::CompoundCommand::ForClause(f) => {
                if let Some(values) = &f.values {
                    for value in values {
                        self.visit_argument_word(value);
                    }
                }
                self.visit_compound_list(&f.body.list);
            }
            ast::CompoundCommand::CaseClause(c) => {
                self.visit_argument_word(&c.value);
                for case in &c.cases {
                    if let Some(cmd) = &case.cmd {
                        self.visit_compound_list(cmd);
                    }
                }
            }
            ast::CompoundCommand::IfClause(i) => {
                self.visit_compound_list(&i.condition);
                self.visit_compound_list(&i.then);
                if let Some(elses) = &i.elses {
                    for else_clause in elses {
                        if let Some(condition) = &else_clause.condition {
                            self.visit_compound_list(condition);
                        }
                        self.visit_compound_list(&else_clause.body);
                    }
                }
            }
            ast::CompoundCommand::WhileClause(w) | ast::CompoundCommand::UntilClause(w) => {
                self.visit_compound_list(&w.0);
                self.visit_compound_list(&w.1.list);
            }
            ast::CompoundCommand::Coprocess(c) => self.visit_command(&c.body),
            ast::CompoundCommand::Arithmetic(a) => self.visit_arithmetic_command(a),
            ast::CompoundCommand::ArithmeticForClause(_) => {}
        }
    }

    fn visit_function_definition(&mut self, func: &ast::FunctionDefinition) {
        let name = func.fname.value.clone();
        let symbol = function_symbol(self.pkg, &name);
        self.functions.insert(name.clone(), symbol.clone());

        if let Some(loc) = &func.fname.loc {
            let range = span_to_range(loc, &self.current_source);
            self.emit(
                &symbol,
                range,
                SymbolRole::Definition as i32,
                SyntaxKind::IdentifierFunctionDefinition,
            );
        }

        let documentation = func
            .fname
            .loc
            .as_ref()
            .map(|loc| self.doc_comment_above(loc.start.line))
            .unwrap_or_default();

        self.symbols
            .entry(symbol.clone())
            .or_insert_with(|| SymbolInformation {
                symbol,
                display_name: name,
                kind: Kind::Function.into(),
                documentation,
                ..Default::default()
            });

        // Function bodies have their own variable scope for `local`s.
        self.scopes.push(Scope::new());
        self.visit_compound_command(&func.body.0);
        self.scopes.pop();
    }

    /// Collect the contiguous run of `#` comment lines immediately above the
    /// given 1-based source line, returned as the symbol's documentation.
    fn doc_comment_above(&self, def_line: usize) -> Vec<String> {
        if def_line < 2 {
            return Vec::new();
        }
        // Inside a command-substitution fragment the line numbers are relative to
        // the fragment, not `self.text`, so a lookup against the document text
        // would attach unrelated comments. A fragment has no meaningful leading
        // documentation of its own, so skip it.
        if self.in_fragment() {
            return Vec::new();
        }
        // Walk the lines above the definition (1-based `def_line`) from the
        // bottom up, collecting the contiguous run of `#` comments.
        let above: Vec<&str> = self.text.lines().take(def_line - 1).collect();
        let mut collected = Vec::new();
        for line in above.into_iter().rev() {
            let Some(comment) = line.trim_start().strip_prefix('#') else {
                break;
            };
            // A `#!` shebang is not documentation.
            if comment.starts_with('!') {
                break;
            }
            collected.push(comment.trim().to_string());
        }
        if collected.is_empty() {
            return Vec::new();
        }
        collected.reverse();
        vec![collected.join("\n")]
    }

    fn visit_simple_command(&mut self, simple: &ast::SimpleCommand) {
        let command_name = simple.word_or_name.as_ref().map(|w| w.value.as_str());

        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                self.visit_prefix_or_suffix_item(item, command_name);
            }
        }

        if let Some(word) = &simple.word_or_name {
            self.visit_command_word(word);
        }

        if let Some(suffix) = &simple.suffix {
            // The first plain-word argument is the source target of a `source` /
            // `.` command. Redirects and assignment words can precede it, so it
            // is the first Word item, not the first suffix item.
            let mut seen_word = false;
            for item in &suffix.0 {
                let is_first_word =
                    !seen_word && matches!(item, ast::CommandPrefixOrSuffixItem::Word(_));
                if is_first_word {
                    seen_word = true;
                }
                self.visit_suffix_item(item, command_name, is_first_word);
            }
        }
    }

    /// The command name position. A command word can resolve, in priority order,
    /// to a user-defined function, an explicit path to a program, or a `$PATH`
    /// binary. Whatever it is, embedded expansions are also scanned.
    fn visit_command_word(&mut self, word: &ast::Word) {
        if let Some(loc) = word.loc.clone() {
            let range = span_to_range(&loc, &self.current_source);
            if has_path_component(&word.value) {
                // Invoked by path, e.g. `./build.sh` or `/usr/bin/env`.
                self.visit_path_word(&word.value, &loc);
            } else {
                // A plain name may be a function defined later in the file, which
                // takes precedence over a same-named binary, so defer it.
                self.pending_calls.push(PendingCall {
                    name: word.value.clone(),
                    range,
                });
            }
        }
        self.visit_word_expansions(word);
    }

    /// Resolve every deferred command-name call now that all function
    /// definitions are known.
    fn resolve_pending_calls(&mut self) {
        self.resolve_pending_calls_from(0);
    }

    /// Resolve the deferred calls recorded from `from` onward: a call links to a
    /// function of that name if one exists, otherwise to a `$PATH` binary,
    /// otherwise nothing. Used both at the end of the document and at the end of
    /// a command substitution, so a subshell's calls are resolved (and their
    /// ranges shifted) against the functions visible inside it.
    fn resolve_pending_calls_from(&mut self, from: usize) {
        for call in self.pending_calls.split_off(from) {
            if let Some(symbol) = self.functions.get(&call.name).cloned() {
                self.emit(&symbol, call.range, 0, SyntaxKind::IdentifierFunction);
            } else if self.resolver.is_binary_on_path(&call.name) {
                let symbol = binary_symbol(&call.name);
                self.emit(&symbol, call.range, 0, SyntaxKind::IdentifierBuiltin);
                self.describe(symbol, call.name, Kind::Function);
            }
        }
    }

    /// Emit a filesystem-path occurrence for a word, when it denotes an absolute
    /// path. Words with a relative path component (`./x`) are left unindexed.
    fn visit_path_word(&mut self, value: &str, loc: &SourceSpan) {
        if !is_absolute_path(value) {
            return;
        }
        let symbol = path_symbol(value);
        let range = span_to_range(loc, &self.current_source);
        self.emit(&symbol, range, 0, SyntaxKind::StringLiteral);
        self.describe(symbol, value.to_string(), Kind::File);
    }

    fn visit_prefix_or_suffix_item(
        &mut self,
        item: &ast::CommandPrefixOrSuffixItem,
        command_name: Option<&str>,
    ) {
        // A prefix item is never the source target.
        self.visit_suffix_item(item, command_name, false);
    }

    /// Index a prefix/suffix item. `is_first_word` marks the first plain-word
    /// argument, which is the file argument of a `source` / `.` command.
    fn visit_suffix_item(
        &mut self,
        item: &ast::CommandPrefixOrSuffixItem,
        command_name: Option<&str>,
        is_first_word: bool,
    ) {
        match item {
            ast::CommandPrefixOrSuffixItem::Word(word) => {
                if is_first_word && command_name.is_some_and(|n| SOURCE_BUILTINS.contains(&n)) {
                    self.visit_source_target(word);
                } else if command_name.is_some_and(|n| LOCAL_DECLARATION_BUILTINS.contains(&n)) {
                    // A bare `local x` declaration (no `=`) is still a local.
                    self.visit_local_declaration_word(word);
                } else {
                    self.visit_argument_word(word);
                }
            }
            ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                let local = command_name.is_some_and(|n| LOCAL_DECLARATION_BUILTINS.contains(&n));
                self.visit_assignment(assignment, word, local);
            }
            ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                self.visit_subshell(&subshell.list);
            }
            ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                self.visit_redirect(redirect);
            }
        }
    }

    /// Index the file argument of a `source` / `.` command as a source
    /// reference, linking this script to the file it pulls in. Any literal path
    /// links to the file, not just an absolute one.
    fn visit_source_target(&mut self, word: &ast::Word) {
        if let Some(loc) = &word.loc {
            self.emit_file_literal(loc, &word.value, 0, |path| Some(source_symbol(path)));
        }
        self.visit_word_expansions(word);
    }

    /// Emit a `File`-kind occurrence for a literal path embedded in a word, when
    /// the literal is a static path with no expansions. `literal` is the raw text
    /// (possibly quoted) lying `base_offset` characters into `loc`'s value (0 for
    /// a plain argument, the `name=` width for an assignment value). Surrounding
    /// quotes are stripped and the range is shifted past the opening quote so it
    /// covers the path text only.
    ///
    /// `select` turns the unquoted path into the symbol to emit, or `None` to skip
    /// it (filesystem references skip a non-absolute path; a source target accepts
    /// any). A literal that is a runtime-computed value (`"$lib"`, `"/etc/$x"`,
    /// `/etc/$1`) cannot be resolved statically, so nothing is emitted; a `$`
    /// inside single quotes or escaped is literal text and still counts as a path.
    fn emit_file_literal(
        &mut self,
        loc: &SourceSpan,
        literal: &str,
        base_offset: usize,
        select: impl FnOnce(&str) -> Option<String>,
    ) {
        if has_dynamic_value(literal) {
            return;
        }
        let (path, quote_offset) = strip_surrounding_quotes(literal);
        let Some(symbol) = select(path) else {
            return;
        };
        let range = subrange(
            loc,
            &self.current_source,
            base_offset + quote_offset,
            path.chars().count(),
        );
        self.emit(&symbol, range, 0, SyntaxKind::StringLiteral);
        self.describe(symbol, path.to_string(), Kind::File);
    }

    /// Handle a bare word in a `local` / `declare` command without an `=`, e.g.
    /// `local x`. It declares a function-local variable.
    fn visit_local_declaration_word(&mut self, word: &ast::Word) {
        // Options like `-i` are not variable names.
        if word.value.starts_with('-') {
            self.visit_argument_word(word);
            return;
        }
        if let Some(loc) = &word.loc {
            let name = &word.value;
            if is_variable_name(name) {
                let symbol = self.declare_local(name);
                let roles = SymbolRole::Definition as i32 | SymbolRole::WriteAccess as i32;
                let range = span_to_range(loc, &self.current_source);
                self.emit(&symbol, range, roles, SyntaxKind::IdentifierLocal);
                self.describe(symbol, name.clone(), Kind::Variable);
                return;
            }
        }
        self.visit_argument_word(word);
    }

    /// Index an assignment (`name=value`). `word` is the full located word
    /// (`name=value`); the [`Assignment`] itself carries the parsed name but its
    /// value word has no location, so expansions in the value are recovered by
    /// scanning the full word's text. `local` declares a function-scoped symbol.
    ///
    /// [`Assignment`]: ast::Assignment
    fn visit_assignment(&mut self, assignment: &ast::Assignment, word: &ast::Word, local: bool) {
        let name = match &assignment.name {
            ast::AssignmentName::VariableName(n) => n.clone(),
            ast::AssignmentName::ArrayElementName(n, _) => n.clone(),
        };
        // A plain assignment writes the variable visible at this point: a
        // function-local one if already declared, otherwise the global.
        let symbol = if local {
            self.declare_local(&name)
        } else {
            self.reference_symbol(&name)
        };

        // The assignment's span covers `name=value`; the definition occurrence
        // points at the name, which sits at the start of that span.
        let range = subrange(
            &assignment.loc,
            &self.current_source,
            0,
            name.chars().count(),
        );
        let roles = SymbolRole::Definition as i32 | SymbolRole::WriteAccess as i32;
        self.emit(&symbol, range, roles, SyntaxKind::IdentifierLocal);
        self.describe(symbol, name, Kind::Variable);

        // The parsed value word carries no location, so recover the value from
        // the full `name=value` word: a literal absolute path on the right-hand
        // side (`X=/etc/foo`, `X="/etc/foo"`) is indexed like any other path.
        if let Some(eq) = word.value.find('=') {
            let raw_value = &word.value[eq + 1..];
            // The value sits past the `name=` prefix within the assignment's
            // `name=value` span.
            let base_offset = word.value[..eq].chars().count() + 1;
            self.emit_file_literal(&assignment.loc, raw_value, base_offset, filesystem_symbol);
        }

        // The value may contain further expansions. The scanner only matches
        // `$`-prefixed names, so the `name=` prefix is not mistaken for one.
        self.visit_word_expansions(word);
    }

    /// Index a word in argument position: report it as a filesystem path when it
    /// is an absolute path, and scan it for expansions either way.
    fn visit_argument_word(&mut self, word: &ast::Word) {
        if let Some(loc) = &word.loc {
            self.emit_file_literal(loc, &word.value, 0, filesystem_symbol);
        }
        self.visit_word_expansions(word);
    }

    fn visit_redirects(&mut self, redirects: &Option<ast::RedirectList>) {
        if let Some(list) = redirects {
            for redirect in &list.0 {
                self.visit_redirect(redirect);
            }
        }
    }

    /// Index the target of an I/O redirection. The target word carries its own
    /// location, so paths and expansions in it are handled like any argument.
    fn visit_redirect(&mut self, redirect: &ast::IoRedirect) {
        match redirect {
            ast::IoRedirect::File(_, _, target) => match target {
                ast::IoFileRedirectTarget::Filename(word) => self.visit_argument_word(word),
                // A `>&word` duplicate names a file descriptor, not a file, so
                // scan it for expansions but do not index it as a path.
                ast::IoFileRedirectTarget::Duplicate(word) => self.visit_word_expansions(word),
                ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                    self.visit_subshell(&subshell.list);
                }
                ast::IoFileRedirectTarget::Fd(_) => {}
            },
            ast::IoRedirect::OutputAndError(word, _) => self.visit_argument_word(word),
            ast::IoRedirect::HereString(_, word) => self.visit_word_expansions(word),
            ast::IoRedirect::HereDocument(_, here_doc) => {
                // The here-end delimiter is literal; only the body expands (when
                // unquoted), so scan the body for references. A `<<-` body has had
                // its leading tabs stripped from the value, so position against
                // the raw source accordingly.
                if here_doc.requires_expansion {
                    if here_doc.remove_tabs {
                        self.visit_heredoc_body(&here_doc.doc);
                    } else {
                        self.visit_word_expansions(&here_doc.doc);
                    }
                }
            }
        }
    }

    /// Scan a word's raw text for every expansion and emit the appropriate
    /// occurrences: variable reads, and recursion into command and arithmetic
    /// substitutions.
    fn visit_word_expansions(&mut self, word: &ast::Word) {
        let Some(loc) = word.loc.clone() else { return };
        self.visit_expansions_in(&word.value, &loc, 0, false);
    }

    /// Like [`Self::visit_word_expansions`], but for a `<<-` here-document body,
    /// where brush has stripped each line's leading tabs from the value while the
    /// raw source still has them. Positions are computed skipping those tabs.
    fn visit_heredoc_body(&mut self, word: &ast::Word) {
        let Some(loc) = word.loc.clone() else { return };
        self.visit_expansions_in(&word.value, &loc, 0, true);
    }

    /// Scan `text` for every expansion and emit occurrences for it, where `text`
    /// begins `base_offset` characters into `span`. Shared by word scanning and
    /// arithmetic; the latter additionally reads bare identifiers (see
    /// [`Self::visit_arithmetic_command`]). `strip_tabs` positions against a
    /// `<<-` body whose leading tabs brush removed from the value.
    fn visit_expansions_in(
        &mut self,
        text: &str,
        span: &SourceSpan,
        base_offset: usize,
        strip_tabs: bool,
    ) {
        let locate = |me: &Self, offset, len| {
            if strip_tabs {
                subrange_stripping_tabs(span, &me.current_source, offset, len)
            } else {
                subrange(span, &me.current_source, offset, len)
            }
        };
        for expansion in find_expansions(text) {
            match expansion {
                Expansion::Variable { name, char_offset } => {
                    let symbol = self.reference_symbol(&name);
                    let range = locate(self, base_offset + char_offset, name.chars().count());
                    self.emit(
                        &symbol,
                        range,
                        SymbolRole::ReadAccess as i32,
                        SyntaxKind::IdentifierLocal,
                    );
                }
                Expansion::CommandSubstitution { inner, char_offset } => {
                    let base = locate(self, base_offset + char_offset, 0);
                    self.visit_substitution_at(&inner, base);
                }
                Expansion::Arithmetic { inner, char_offset } => {
                    self.visit_arithmetic(&inner, span, base_offset + char_offset);
                }
            }
        }
    }

    /// Re-parse the body of a command substitution and index it, translating the
    /// inner occurrences back to absolute positions in the containing word. `base`
    /// is the document position of the first character of `inner` (the caller
    /// computes it, since only the caller knows how the body is positioned).
    fn visit_substitution_at(&mut self, inner: &str, base: Range) {
        let program = match parse(inner) {
            Ok(program) => program,
            Err(e) => {
                // The outer document parsed, so this is a fragment the scanner
                // extracted that brush will not parse on its own. Index what we
                // can but make the gap observable rather than dropping it.
                eprintln!(
                    "{}: skipping unparsable substitution `{inner}`: {e:#}",
                    self.relative_path
                );
                return;
            }
        };
        // The sub-parse numbers from line 1 / column 1, so occurrences it produces
        // are shifted by the body's origin afterwards.
        let line_shift = base.start_line;
        let col_shift = base.start_char;
        let before = self.occurrences.len();
        // A command substitution runs in a subshell: it reads the enclosing
        // variables but its own `local` declarations and function definitions do
        // not escape. While visiting the fragment its spans refer to the fragment,
        // not the document, so the current source is swapped too.
        self.with_source(inner, |me| {
            me.in_subshell_scope(|me| me.visit_program(&program));
        });
        for occ in &mut self.occurrences[before..] {
            occ.range = occ.range.shifted(line_shift, col_shift);
        }
    }

    /// Run `body` with `current_source` set to `source`, restoring the previous
    /// source afterwards. References resolved inside `body` are positioned against
    /// `source`; this is used while indexing a re-parsed command-substitution
    /// fragment, whose spans are relative to the fragment rather than the
    /// document. `fragment_depth` tracks the nesting so document-only logic (such
    /// as doc-comment lookup) can tell it is inside a fragment. The swap is always
    /// undone once `body` returns.
    fn with_source(&mut self, source: &str, body: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.current_source, source.to_string());
        self.fragment_depth += 1;
        body(self);
        self.fragment_depth -= 1;
        self.current_source = outer;
    }

    /// Whether the walk is currently inside a re-parsed command-substitution
    /// fragment, where positions are relative to the fragment rather than the
    /// document. Logic that reads `self.text` (the whole document) directly must
    /// consult this, since the fragment's line numbers do not index into it.
    fn in_fragment(&self) -> bool {
        self.fragment_depth > 0
    }

    /// Index an arithmetic expression's bare variable reads. `$name` forms are
    /// already covered by [`Self::visit_word_expansions`].
    fn visit_arithmetic(&mut self, inner: &str, word_loc: &SourceSpan, char_offset: usize) {
        for var in find_arithmetic_variables(inner) {
            let symbol = self.reference_symbol(&var.name);
            let range = subrange(
                word_loc,
                &self.current_source,
                char_offset + var.char_offset,
                var.name.chars().count(),
            );
            self.emit(
                &symbol,
                range,
                SymbolRole::ReadAccess as i32,
                SyntaxKind::IdentifierLocal,
            );
        }
    }

    /// Index a standalone arithmetic command (`(( ... ))`). The expression text
    /// carries no location of its own, but the command's span does. The
    /// expression begins after the `((` and any whitespace, so its offset within
    /// the command can be measured from the raw source without a fragile search,
    /// and the reads inside it are indexed exactly like a `$((...))` body.
    fn visit_arithmetic_command(&mut self, cmd: &ast::ArithmeticCommand) {
        let span = &cmd.loc;
        // brush's index is a character offset; convert it to a byte offset before
        // slicing so a multibyte character is not split.
        let byte_start = char_to_byte(&self.current_source, span.start.index);
        let Some(after_open) = self.current_source[byte_start..].strip_prefix("((") else {
            return;
        };
        // The expression starts past the `((` and the whitespace following it.
        // Count characters (not bytes) so the offset matches `subrange`'s walk.
        let leading_ws: usize = after_open.chars().take_while(|c| c.is_whitespace()).count();
        let char_offset = 2 + leading_ws;
        // Index the reads inside the expression exactly like a `$((...))` body:
        // `$`-prefixed reads and substitutions as in any word, plus the bare
        // identifiers that only arithmetic context reads.
        let expr = &cmd.expr.value;
        self.visit_expansions_in(expr, span, char_offset, false);
        self.visit_arithmetic(expr, span, char_offset);
    }
}

/// Symbol selector for a filesystem path reference: links only an absolute path,
/// so relative paths and bare arguments are left unindexed.
fn filesystem_symbol(path: &str) -> Option<String> {
    is_absolute_path(path).then(|| path_symbol(path))
}

/// Strip a single matching pair of surrounding quotes from a literal value,
/// returning the inner text and how many characters were stripped from the
/// front (0 or 1). A value with no surrounding quotes is returned unchanged.
fn strip_surrounding_quotes(value: &str) -> (&str, usize) {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return (&value[1..value.len() - 1], 1);
        }
    }
    (value, 0)
}
