//! Scans raw shell word text for the expansions embedded in it.
//!
//! brush-parser keeps each word's raw text (quoting and expansions intact) but
//! does not break out the `$var` references or the `$(...)` / `$((...))` /
//! backtick substitutions inside it. We recover them with a small hand-written
//! scanner so each can be turned into an occurrence (and, for command
//! substitutions, re-parsed and indexed recursively).
//!
//! brush's `word::parse` does split a word into typed pieces, which is enough
//! for the static/dynamic question in [`has_dynamic_value`], but its pieces
//! carry no sub-positions: the name inside `${...}`, the operand of a `${x:-$y}`
//! default, the inner text of a `$(...)`. So we still scan by hand here to find
//! where each reference sits.
//!
//! TODO(jelmer): drop this scanner if brush ever exposes those sub-positions.

use brush_parser::word::{self, WordPiece};
use brush_parser::ParserOptions;

/// An expansion found within a word's raw text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expansion {
    /// A `$name` / `${name}` variable reference.
    Variable {
        /// The variable name, without the leading `$` or surrounding braces.
        name: String,
        /// Character offset of the name within the word text (past `$`/`${`).
        char_offset: usize,
    },
    /// A `$(...)` or backtick command substitution. The inner text is the shell
    /// code between the delimiters; `char_offset` is where that inner text
    /// begins within the word.
    CommandSubstitution { inner: String, char_offset: usize },
    /// A `$((...))` arithmetic expansion. The inner text is the arithmetic
    /// expression; `char_offset` is where it begins within the word.
    Arithmetic { inner: String, char_offset: usize },
}

/// Find every expansion in a word's raw text, in order of appearance.
///
/// Handles `$name`, `${name}`, the `${name...}` parameter-expansion forms (only
/// the leading name is reported), `$(...)`, `$((...))` and `` `...` ``.
/// Expansions inside single quotes are skipped because `$` and backticks are
/// literal there, and `\$` / `` \` `` are treated as escapes. Positional and
/// special parameters (`$1`, `$?`, `$@`, ...) are ignored since they have no
/// user-defined symbol.
pub fn find_expansions(text: &str) -> Vec<Expansion> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    scan_expansions(&chars, 0, &mut out);
    out
}

/// Whether a word's raw text is a runtime-computed value rather than a static
/// literal that names a fixed path.
///
/// A word is static only if every piece brush parses it into is literal text
/// (plain, single-quoted, ANSI-C-quoted, escaped, or such text nested in double
/// quotes) and the unquoted parts carry no glob metacharacters. Any parameter
/// expansion (`$x`, `$1`, `$$`, `${...}`), command substitution (`$(...)`,
/// `` `...` ``), arithmetic, tilde expansion (`~/x`) or unquoted glob
/// (`/etc/*.conf`) makes it dynamic. A word brush cannot parse is treated as
/// dynamic, since its value cannot be trusted as a literal.
pub fn has_dynamic_value(text: &str) -> bool {
    match word::parse(text, &ParserOptions::default()) {
        Ok(pieces) => pieces.iter().any(|p| !is_static_piece(&p.piece)),
        Err(_) => true,
    }
}

/// Whether a parsed word piece is static literal text. Plain `Text` is static
/// unless it carries an unquoted glob metacharacter; quoted and escaped text is
/// always literal (globs inside quotes lose their special meaning); a
/// double-quoted sequence is static when all its inner pieces are.
fn is_static_piece(piece: &WordPiece) -> bool {
    match piece {
        WordPiece::Text(text) => !has_glob_metacharacter(text),
        WordPiece::SingleQuotedText(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::EscapeSequence(_) => true,
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            inner.iter().all(|p| is_static_piece(&p.piece))
        }
        WordPiece::TildeExpansion(_)
        | WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => false,
    }
}

/// Whether unquoted `text` contains a shell glob metacharacter (`*`, `?`, `[`),
/// which makes it a filename pattern rather than a fixed path.
fn has_glob_metacharacter(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// Scan `chars` for expansions, recording each at its absolute position. `base`
/// is added to every offset so the same routine can scan a sub-slice (the inner
/// text of a `${...}` or `$((...))`) and still report positions relative to the
/// whole word.
fn scan_expansions(chars: &[char], base: usize, out: &mut Vec<Expansion>) {
    let mut i = 0;
    let mut in_single_quote = false;

    while i < chars.len() {
        let c = chars[i];

        if c == '\\' && !in_single_quote {
            i += 2;
            continue;
        }

        if c == '\'' {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }

        if in_single_quote {
            i += 1;
            continue;
        }

        if c == '`' {
            if let Some((inner, offset, consumed)) = parse_backticks(chars, i) {
                out.push(Expansion::CommandSubstitution {
                    inner,
                    char_offset: base + offset,
                });
                i += consumed;
                continue;
            }
        }

        if c == '$' {
            if let Some(consumed) = parse_dollar(chars, i, base, out) {
                i += consumed;
                continue;
            }
        }

        i += 1;
    }
}

/// Parse whatever follows a `$` at index `dollar`, pushing any expansions it
/// contains (with `base` added to their offsets) and returning how many
/// characters were consumed including the `$`. Returns `None` for `$` forms
/// without a user-defined symbol (`$1`, `$?`, `$$`, a bare `$`), so the caller
/// advances by one.
fn parse_dollar(
    chars: &[char],
    dollar: usize,
    base: usize,
    out: &mut Vec<Expansion>,
) -> Option<usize> {
    let after = dollar + 1;
    let next = *chars.get(after)?;

    // `$((` arithmetic must be checked before `$(` command substitution.
    if next == '(' && chars.get(after + 1) == Some(&'(') {
        let inner_start = after + 2;
        let (end, closed) = find_double_close_paren(chars, inner_start);
        let inner: String = chars[inner_start..end].iter().collect();
        out.push(Expansion::Arithmetic {
            inner,
            char_offset: base + inner_start,
        });
        // A `$var` used inside the arithmetic is a normal expansion; recurse so
        // it is reported too (bare names are recovered separately).
        scan_expansions(&chars[inner_start..end], base + inner_start, out);
        // Past `))` if closed, else to end of input.
        let consumed = if closed { end + 2 } else { end } - dollar;
        return Some(consumed);
    }

    if next == '(' {
        let inner_start = after + 1;
        let (end, closed) = find_matching_paren(chars, inner_start);
        let inner: String = chars[inner_start..end].iter().collect();
        out.push(Expansion::CommandSubstitution {
            inner,
            char_offset: base + inner_start,
        });
        let consumed = if closed { end + 1 } else { end } - dollar;
        return Some(consumed);
    }

    if next == '{' {
        return parse_braced(chars, dollar, base, out);
    }

    if is_name_start(next) {
        let name = take_name(chars, after);
        let consumed = 1 + name.chars().count();
        out.push(Expansion::Variable {
            name,
            char_offset: base + after,
        });
        return Some(consumed);
    }

    None
}

/// Parse a `${...}` parameter expansion. Reports the parameter being expanded
/// (skipping the `#` length and `!` indirection prefixes) and recurses into the
/// operand so references in `${x:-$y}` defaults or `${arr[$i]}` subscripts are
/// reported too.
fn parse_braced(
    chars: &[char],
    dollar: usize,
    base: usize,
    out: &mut Vec<Expansion>,
) -> Option<usize> {
    let open = dollar + 1; // index of `{`
    let (end, closed) = find_matching_brace(chars, open + 1);

    // `${#name}` is the length of `name`; `${!name}` is indirect expansion of
    // `name`. Both still read `name`, so step past the operator before the name.
    let mut name_start = open + 1;
    if matches!(chars.get(name_start), Some('#') | Some('!')) {
        name_start += 1;
    }

    let name = take_name(chars, name_start);
    if !name.is_empty() {
        out.push(Expansion::Variable {
            name: name.clone(),
            char_offset: base + name_start,
        });
    }

    // The remainder of the braces may hold further expansions (a `:-$y` default,
    // a `[$i]` subscript). Scan it, skipping the name we already reported.
    let operand_start = name_start + name.chars().count();
    if operand_start < end {
        scan_expansions(&chars[operand_start..end], base + operand_start, out);
    }

    // Past the `}` when closed, else to end of input. A `${...}` with no
    // recognisable name (e.g. `${1:-x}`) reports nothing itself, but its operand
    // may have, which the scan above handled.
    Some(end + usize::from(closed) - dollar)
}

/// Find the index of the `}` that closes a `${` whose body starts at `start`,
/// honouring nested `${...}`. Returns the closing-brace index (or end of input)
/// and whether it was found.
fn find_matching_brace(chars: &[char], start: usize) -> (usize, bool) {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 2;
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (i, true);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (chars.len(), false)
}

/// Parse a backtick command substitution starting at the opening backtick.
/// Returns the inner text, its offset, and characters consumed (including both
/// backticks).
fn parse_backticks(chars: &[char], open: usize) -> Option<(String, usize, usize)> {
    let inner_start = open + 1;
    let mut i = inner_start;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '`' {
            let inner: String = chars[inner_start..i].iter().collect();
            return Some((inner, inner_start, i + 1 - open));
        }
        i += 1;
    }
    None
}

/// Walk `chars` from `start`, invoking `f` for each character that is not inside
/// single or double quotes and not backslash-escaped. `f` returns `true` to stop
/// the walk early. Returns the index `f` stopped at and whether it stopped.
///
/// This is the shared quote/escape skeleton for the substitution and arithmetic
/// close-paren finders, so a `)` in a quoted string never ends them early.
fn for_each_active_char(
    chars: &[char],
    start: usize,
    mut f: impl FnMut(usize, char) -> bool,
) -> (usize, bool) {
    let mut i = start;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some(_) => match c {
                '\\' => {
                    i += 2;
                    continue;
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\\' => {
                    i += 2;
                    continue;
                }
                '\'' | '"' => quote = Some(c),
                _ => {
                    if f(i, c) {
                        return (i, true);
                    }
                }
            },
        }
        i += 1;
    }
    (chars.len(), false)
}

/// Find the index of the `)` that closes a `$(` starting at `start`, tracking
/// nested parentheses. Returns the index of the closing paren (or end of input)
/// and whether it was actually found.
fn find_matching_paren(chars: &[char], start: usize) -> (usize, bool) {
    let mut depth = 1;
    for_each_active_char(chars, start, |_, c| match c {
        '(' => {
            depth += 1;
            false
        }
        ')' => {
            depth -= 1;
            depth == 0
        }
        _ => false,
    })
}

/// Find the index of the first `)` of the `))` that closes a `$((` starting at
/// `start`. Returns that index (or end of input) and whether it was found.
fn find_double_close_paren(chars: &[char], start: usize) -> (usize, bool) {
    let mut depth = 0;
    for_each_active_char(chars, start, |i, c| match c {
        '(' => {
            depth += 1;
            false
        }
        ')' => {
            if depth == 0 && chars.get(i + 1) == Some(&')') {
                return true;
            }
            if depth > 0 {
                depth -= 1;
            }
            false
        }
        _ => false,
    })
}

/// A bare variable name found in an arithmetic expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArithmeticVariable {
    pub name: String,
    pub char_offset: usize,
}

/// Find the bare variable names in an arithmetic expression (the inner text of a
/// `$((...))`). Inside arithmetic, identifiers are variable reads without a `$`.
/// `$name` forms and numeric literals are skipped; the former are handled by
/// [`find_expansions`] over the same text.
pub fn find_arithmetic_variables(text: &str) -> Vec<ArithmeticVariable> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        // A `$`-prefixed expansion is handled elsewhere; skip it whole so its
        // contents are not also reported as bare identifiers. `$(( ))` must be
        // checked before `$( )`, and `$name` is the plain case.
        if c == '$' {
            if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                let (end, closed) = find_double_close_paren(&chars, i + 3);
                i = if closed { end + 2 } else { end };
                continue;
            }
            if chars.get(i + 1) == Some(&'(') {
                let (end, closed) = find_matching_paren(&chars, i + 2);
                i = if closed { end + 1 } else { end };
                continue;
            }
            if chars.get(i + 1) == Some(&'{') {
                let (end, closed) = find_matching_brace(&chars, i + 2);
                i = if closed { end + 1 } else { end };
                continue;
            }
            i += 1;
            while i < chars.len() && is_name_continue(chars[i]) {
                i += 1;
            }
            continue;
        }
        // A backtick command substitution is likewise not a bare identifier.
        if c == '`' {
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            if i < chars.len() {
                i += 1; // past the closing backtick
            }
            continue;
        }
        // A run of digits is a numeric literal; skip it whole, including any
        // trailing name-like characters (`0x1f`, `1e3`) so they are not read as
        // identifiers.
        if c.is_ascii_digit() {
            while i < chars.len() && is_name_continue(chars[i]) {
                i += 1;
            }
            continue;
        }
        if is_name_start(c) {
            let name = take_name(&chars, i);
            let len = name.chars().count();
            out.push(ArithmeticVariable {
                name,
                char_offset: i,
            });
            i += len;
            continue;
        }
        i += 1;
    }

    out
}

/// Collect a shell variable name (`[A-Za-z_][A-Za-z0-9_]*`) starting at `start`.
fn take_name(chars: &[char], start: usize) -> String {
    let mut name = String::new();
    let mut i = start;
    if i >= chars.len() || !is_name_start(chars[i]) {
        return name;
    }
    name.push(chars[i]);
    i += 1;
    while i < chars.len() && is_name_continue(chars[i]) {
        name.push(chars[i]);
        i += 1;
    }
    name
}

pub(crate) fn is_name_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

pub(crate) fn is_name_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Whether a whole word is a valid shell variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`).
pub(crate) fn is_variable_name(word: &str) -> bool {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) if is_name_start(c) => {}
        _ => return false,
    }
    chars.all(is_name_continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var_names(text: &str) -> Vec<String> {
        find_expansions(text)
            .into_iter()
            .filter_map(|e| match e {
                Expansion::Variable { name, .. } => Some(name),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_reference() {
        assert_eq!(var_names("$FOO"), vec!["FOO".to_string()]);
    }

    #[test]
    fn braced_reference() {
        assert_eq!(var_names("${BAR}"), vec!["BAR".to_string()]);
    }

    #[test]
    fn parameter_expansion_reports_name_only() {
        assert_eq!(var_names("${BAR:-default}"), vec!["BAR".to_string()]);
    }

    #[test]
    fn embedded_in_string() {
        assert_eq!(var_names("prefix-$HOME/bin"), vec!["HOME".to_string()]);
    }

    #[test]
    fn multiple_references() {
        assert_eq!(
            var_names("$A${B}c$D"),
            vec!["A".to_string(), "B".to_string(), "D".to_string()]
        );
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(var_names("'$FOO'"), Vec::<String>::new());
    }

    #[test]
    fn has_dynamic_value_distinguishes_literal_from_runtime() {
        // Expansions of every kind are runtime values.
        assert!(has_dynamic_value("$FOO"));
        assert!(has_dynamic_value("/etc/$x"));
        assert!(has_dynamic_value("`date`"));
        assert!(has_dynamic_value("$(date)"));
        assert!(has_dynamic_value("${x:-y}"));
        // Positional and special parameters too.
        assert!(has_dynamic_value("/etc/$1"));
        assert!(has_dynamic_value("/etc/$$"));
        assert!(has_dynamic_value("/var/run/$$.pid"));
        assert!(has_dynamic_value("/etc/$@"));
        // Tilde expansion and unquoted globs are not fixed paths.
        assert!(has_dynamic_value("~/foo"));
        assert!(has_dynamic_value("/etc/*.conf"));
        assert!(has_dynamic_value("/etc/foo?"));
        assert!(has_dynamic_value("/etc/[abc]"));

        // Static literals: plain, single-quoted, ANSI-C-quoted, escaped, and a
        // bare trailing `$`. A `$`, `*` or `'` inside quotes is literal text.
        assert!(!has_dynamic_value("/etc/passwd"));
        assert!(!has_dynamic_value("'/etc/$x'"));
        assert!(!has_dynamic_value("'/etc/*.conf'"));
        assert!(!has_dynamic_value("/etc/x\\$y"));
        assert!(!has_dynamic_value("price\\$5"));
        assert!(!has_dynamic_value("/var/run/$"));
        assert!(!has_dynamic_value("\"/etc/it's\""));
    }

    #[test]
    fn escaped_dollar_is_ignored() {
        assert_eq!(var_names("\\$FOO"), Vec::<String>::new());
    }

    #[test]
    fn positional_and_special_ignored() {
        assert_eq!(var_names("$1 $? $@ $$"), Vec::<String>::new());
    }

    #[test]
    fn offset_points_at_name() {
        let refs = find_expansions("ab$CD");
        assert_eq!(
            refs,
            vec![Expansion::Variable {
                name: "CD".to_string(),
                char_offset: 3,
            }]
        );

        let braced = find_expansions("ab${CD}");
        assert_eq!(
            braced,
            vec![Expansion::Variable {
                name: "CD".to_string(),
                char_offset: 4,
            }]
        );
    }

    #[test]
    fn command_substitution() {
        assert_eq!(
            find_expansions("$(grep foo)"),
            vec![Expansion::CommandSubstitution {
                inner: "grep foo".to_string(),
                char_offset: 2,
            }]
        );
    }

    #[test]
    fn nested_command_substitution() {
        assert_eq!(
            find_expansions("$(echo $(date))"),
            vec![Expansion::CommandSubstitution {
                inner: "echo $(date)".to_string(),
                char_offset: 2,
            }]
        );
    }

    #[test]
    fn command_substitution_with_quoted_paren() {
        // A `)` inside quotes must not end the substitution early.
        assert_eq!(
            find_expansions("$(echo ')')"),
            vec![Expansion::CommandSubstitution {
                inner: "echo ')'".to_string(),
                char_offset: 2,
            }]
        );
        assert_eq!(
            find_expansions("$(echo \")\")"),
            vec![Expansion::CommandSubstitution {
                inner: "echo \")\"".to_string(),
                char_offset: 2,
            }]
        );
    }

    #[test]
    fn backtick_substitution() {
        assert_eq!(
            find_expansions("`date`"),
            vec![Expansion::CommandSubstitution {
                inner: "date".to_string(),
                char_offset: 1,
            }]
        );
    }

    #[test]
    fn arithmetic_expansion() {
        assert_eq!(
            find_expansions("$((a + b))"),
            vec![Expansion::Arithmetic {
                inner: "a + b".to_string(),
                char_offset: 3,
            }]
        );
    }

    #[test]
    fn arithmetic_with_quoted_paren() {
        // A balanced quoted `)` inside arithmetic must not end it early.
        let exps = find_expansions("$(( x + ${y:-\")\"} ))");
        assert!(
            exps.iter()
                .any(|e| matches!(e, Expansion::Arithmetic { inner, .. } if inner.contains("${y"))),
            "arithmetic should extend past the quoted paren: {exps:?}"
        );
    }

    #[test]
    fn arithmetic_is_not_command_substitution() {
        // `$((` must be recognised as arithmetic, not as `$(` with a `(` inside.
        let exps = find_expansions("$((x))");
        assert_eq!(exps.len(), 1);
        assert!(matches!(exps[0], Expansion::Arithmetic { .. }));
    }

    #[test]
    fn arithmetic_reports_dollar_names() {
        // A `$var` inside arithmetic is a normal expansion and must be reported.
        assert_eq!(
            find_expansions("$(( total + $delta ))"),
            vec![
                Expansion::Arithmetic {
                    inner: " total + $delta ".to_string(),
                    char_offset: 3,
                },
                Expansion::Variable {
                    name: "delta".to_string(),
                    char_offset: 13,
                },
            ]
        );
    }

    #[test]
    fn length_and_indirect_report_the_name() {
        assert_eq!(var_names("${#FOO}"), vec!["FOO".to_string()]);
        assert_eq!(var_names("${!FOO}"), vec!["FOO".to_string()]);
    }

    #[test]
    fn parameter_default_operand_is_scanned() {
        assert_eq!(
            var_names("${x:-$y}"),
            vec!["x".to_string(), "y".to_string()]
        );
    }

    #[test]
    fn array_subscript_is_scanned() {
        assert_eq!(
            var_names("${arr[$i]}"),
            vec!["arr".to_string(), "i".to_string()]
        );
    }

    #[test]
    fn arithmetic_bare_variables() {
        assert_eq!(
            find_arithmetic_variables("a + b * 2"),
            vec![
                ArithmeticVariable {
                    name: "a".to_string(),
                    char_offset: 0,
                },
                ArithmeticVariable {
                    name: "b".to_string(),
                    char_offset: 4,
                },
            ]
        );
    }

    #[test]
    fn arithmetic_skips_dollar_names_and_numbers() {
        // `$n` is an expansion (handled elsewhere); `0x1f` is a literal.
        assert_eq!(
            find_arithmetic_variables("$n + 0x1f + count"),
            vec![ArithmeticVariable {
                name: "count".to_string(),
                char_offset: 12,
            }]
        );
    }

    #[test]
    fn arithmetic_skips_command_substitutions() {
        // A `$(...)` / backtick inside arithmetic is not a bare identifier; its
        // contents must not be reported as variable reads.
        assert_eq!(
            find_arithmetic_variables("x = $(id -u)"),
            vec![ArithmeticVariable {
                name: "x".to_string(),
                char_offset: 0,
            }]
        );
        assert_eq!(
            find_arithmetic_variables("a + $((b + c))"),
            vec![ArithmeticVariable {
                name: "a".to_string(),
                char_offset: 0,
            }]
        );
        assert_eq!(
            find_arithmetic_variables("y + `date +%s`"),
            vec![ArithmeticVariable {
                name: "y".to_string(),
                char_offset: 0,
            }]
        );
    }

    #[test]
    fn mixed_expansions() {
        assert_eq!(
            find_expansions("${x}-$(id -u)"),
            vec![
                Expansion::Variable {
                    name: "x".to_string(),
                    char_offset: 2,
                },
                Expansion::CommandSubstitution {
                    inner: "id -u".to_string(),
                    char_offset: 7,
                },
            ]
        );
    }
}
