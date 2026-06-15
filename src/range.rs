//! Conversion from brush-parser source spans to SCIP ranges.

use brush_parser::SourceSpan;

/// A half-open range in SCIP's coordinate system: 0-based line and character
/// offsets, stored as `[startLine, startChar, endLine, endChar]`.
///
/// Field order matters: the derived `Ord` sorts by start line, then start
/// character, then end, which is the ascending order SCIP's canonical form wants
/// for occurrences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Range {
    pub start_line: i32,
    pub start_char: i32,
    pub end_line: i32,
    pub end_char: i32,
}

impl Range {
    /// Encode as the SCIP `Occurrence.range` vector. Uses the three-element
    /// `[line, startChar, endChar]` form when the range stays on a single line,
    /// otherwise the four-element form.
    pub fn to_scip(self) -> Vec<i32> {
        if self.start_line == self.end_line {
            vec![self.start_line, self.start_char, self.end_char]
        } else {
            vec![
                self.start_line,
                self.start_char,
                self.end_line,
                self.end_char,
            ]
        }
    }

    /// Translate a range produced by a sub-parse (numbered from line 0) into the
    /// containing document. Every line moves down by `line_shift`; a position on
    /// line 0 of the sub-parse sits on the substitution's first line, so its
    /// column also moves right by `col_shift`. Later lines keep their columns.
    pub fn shifted(self, line_shift: i32, col_shift: i32) -> Range {
        // Whether a position takes the column shift depends on its *original*
        // (sub-parse) line being 0, so the columns are computed from `self`
        // before the lines are shifted.
        let shift_col = |line: i32, char: i32| if line == 0 { char + col_shift } else { char };
        Range {
            start_char: shift_col(self.start_line, self.start_char),
            end_char: shift_col(self.end_line, self.end_char),
            start_line: self.start_line + line_shift,
            end_line: self.end_line + line_shift,
        }
    }
}

/// Convert a brush-parser [`SourceSpan`] into a SCIP [`Range`].
///
/// brush-parser lines are 1-based; SCIP wants 0-based. brush columns are 1-based
/// *character* counts, but the document declares `UTF8CodeUnitOffsetFromLineStart`,
/// so columns are recomputed as UTF-8 byte offsets from the line start using the
/// source. The span end is already exclusive, matching SCIP's half-open ranges.
pub fn span_to_range(span: &SourceSpan, source: &str) -> Range {
    Range {
        start_line: to_zero_based(span.start.line),
        start_char: byte_column(source, span.start.index),
        end_line: to_zero_based(span.end.line),
        end_char: byte_column(source, span.end.index),
    }
}

/// The UTF-8 byte offset of the character at `char_index`, measured from the
/// start of its line (the byte after the preceding `\n`).
fn byte_column(source: &str, char_index: usize) -> i32 {
    let byte = char_to_byte(source, char_index);
    let line_start = source[..byte].rfind('\n').map_or(0, |nl| nl + 1);
    (byte - line_start) as i32
}

/// Build a range for a sub-slice of a word, given the word's span, the full
/// source document, and a character offset and length within the word's *value*.
///
/// Shell words can embed several distinct things (e.g. a `$var` reference inside
/// a larger argument, or a here-document body spanning several lines), so
/// references are reported relative to the containing word. Offsets are counted
/// in characters of the word's parsed value, but the position is computed by
/// walking the raw `source` from the span's start: brush collapses `\<newline>`
/// line continuations in the value, so walking the value alone would drift.
/// Skipping continuations in the raw source keeps line and column aligned.
///
/// brush's `SourcePosition.index` is a character offset, not a byte offset, so
/// it is converted to a byte offset before slicing the raw source: indexing the
/// string directly with the character offset would split a multibyte character
/// and panic.
pub fn subrange(span: &SourceSpan, source: &str, char_offset: usize, len: usize) -> Range {
    subrange_inner(span, source, char_offset, len, false)
}

/// Like [`subrange`], but for a `<<-` here-document body, where brush has
/// stripped the leading tab(s) from each line of the word's value. The raw
/// source still contains those tabs, so they are skipped (consumed for position
/// without counting against the value offset) at the start of every line, the
/// same way line continuations are.
pub fn subrange_stripping_tabs(
    span: &SourceSpan,
    source: &str,
    char_offset: usize,
    len: usize,
) -> Range {
    subrange_inner(span, source, char_offset, len, true)
}

fn subrange_inner(
    span: &SourceSpan,
    source: &str,
    char_offset: usize,
    len: usize,
    strip_tabs: bool,
) -> Range {
    let base = span_to_range(span, source);
    let mut line = base.start_line;
    let mut col = base.start_char;
    let byte_start = char_to_byte(source, span.start.index);
    let mut chars = source[byte_start..].chars().peekable();

    // A `<<-` body strips leading tabs from its first line too, so skip any that
    // sit at the span start before counting the value offset.
    if strip_tabs {
        skip_leading_tabs(&mut col, &mut chars);
    }
    advance(&mut line, &mut col, &mut chars, char_offset, strip_tabs);
    let (start_line, start_char) = (line, col);
    advance(&mut line, &mut col, &mut chars, len, strip_tabs);

    Range {
        start_line,
        start_char,
        end_line: line,
        end_char: col,
    }
}

/// Consume the run of tab characters at the current source position, advancing
/// the column past them without counting them against a value offset. A tab is
/// one UTF-8 byte, so the byte column advances by one per tab.
fn skip_leading_tabs(col: &mut i32, chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) {
    while chars.peek() == Some(&'\t') {
        chars.next();
        *col += 1;
    }
}

/// Convert a character offset into the byte offset of that character in `source`,
/// returning the end of the string if the offset is past the last character.
pub fn char_to_byte(source: &str, char_index: usize) -> usize {
    source
        .char_indices()
        .nth(char_index)
        .map_or(source.len(), |(byte, _)| byte)
}

/// Advance a `(line, col)` position by consuming `count` value-characters from
/// the raw source `chars`, treating each `\n` as a line break. `count` is a count
/// of value *characters*, but `col` is a UTF-8 byte offset from the line start
/// (the column units SCIP's `UTF8CodeUnitOffsetFromLineStart` declares), so each
/// character advances the column by its byte length. A `\<newline>` line
/// continuation does not appear in the word's value, so it is consumed for its
/// position effect without counting against `count`.
fn advance(
    line: &mut i32,
    col: &mut i32,
    chars: &mut std::iter::Peekable<impl Iterator<Item = char>>,
    count: usize,
    strip_tabs: bool,
) {
    let mut consumed = 0;
    while consumed < count {
        let Some(c) = chars.next() else { break };
        if c == '\\' && chars.peek() == Some(&'\n') {
            chars.next();
            *line += 1;
            *col = 0;
            if strip_tabs {
                skip_leading_tabs(col, chars);
            }
            continue;
        }
        if c == '\n' {
            *line += 1;
            *col = 0;
            // A `<<-` body strips the leading tabs from each line, so they are in
            // the raw source but not the value; skip them without counting.
            if strip_tabs {
                skip_leading_tabs(col, chars);
            }
        } else {
            *col += c.len_utf8() as i32;
        }
        consumed += 1;
    }
}

fn to_zero_based(one_based: usize) -> i32 {
    // Positions are 1-based; guard against a hypothetical 0 rather than
    // underflowing.
    one_based.saturating_sub(1) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start_line: i32, start_char: i32, end_line: i32, end_char: i32) -> Range {
        Range {
            start_line,
            start_char,
            end_line,
            end_char,
        }
    }

    #[test]
    fn ord_sorts_by_start_then_end() {
        // Occurrences must come out in ascending (start_line, start_char, end)
        // order. This pins the derived `Ord`, which depends on field order, and
        // includes a multi-line range so a field reorder cannot pass unnoticed.
        let mut ranges = vec![
            range(2, 0, 2, 4),
            range(0, 5, 0, 6),
            range(0, 3, 2, 1),
            range(0, 3, 0, 9),
        ];
        ranges.sort();
        assert_eq!(
            ranges,
            vec![
                range(0, 3, 0, 9),
                range(0, 3, 2, 1),
                range(0, 5, 0, 6),
                range(2, 0, 2, 4),
            ]
        );
    }

    #[test]
    fn to_scip_uses_three_elements_for_a_single_line() {
        assert_eq!(range(1, 2, 1, 5).to_scip(), vec![1, 2, 5]);
        assert_eq!(range(1, 2, 3, 5).to_scip(), vec![1, 2, 3, 5]);
    }

    #[test]
    fn shifted_moves_first_line_columns_only() {
        // A range wholly on sub-parse line 0 moves down and right.
        assert_eq!(range(0, 1, 0, 4).shifted(2, 8), range(2, 9, 2, 12));
        // A range starting on line 0 but ending on a later line: only the start
        // column shifts; the end column keeps its place on its own line.
        assert_eq!(range(0, 1, 1, 4).shifted(2, 8), range(2, 9, 3, 4));
        // A range entirely past line 0 only moves down.
        assert_eq!(range(1, 1, 1, 4).shifted(2, 8), range(3, 1, 3, 4));
    }
}
