//! Shared text normalization for human-readable terminal output.

/// Maximum number of untrusted characters retained in a single terminal field.
/// The explicit marker added after this prefix makes truncation visible.
pub(crate) const TERMINAL_FIELD_MAX_CHARS: usize = 256;
const TRUNCATION_MARKER: &str = "...";

/// Collapse terminal control and invisible formatting characters to ordinary spaces
/// before interpolating an untrusted string into a human-readable line. JSON
/// renderers deliberately do not use this helper: `serde_json` escapes controls
/// without changing the data contract.
pub(crate) fn terminal_safe(text: &str) -> String {
    text.chars().map(terminal_safe_char).collect()
}

/// Sanitize an untrusted terminal field and cap it to a visible, character-safe
/// prefix. Identity and address values remain untouched in registry/JSON data;
/// only human output uses this bounded representation.
pub(crate) fn terminal_safe_bounded(text: &str) -> String {
    let mut characters = text.chars();
    let prefix: String = characters
        .by_ref()
        .take(TERMINAL_FIELD_MAX_CHARS)
        .map(terminal_safe_char)
        .collect();
    if characters.next().is_some() {
        format!("{prefix}{TRUNCATION_MARKER}")
    } else {
        prefix
    }
}

/// Report whether a string contains a character that can control or invisibly
/// reshape terminal output. CLI identifiers reject these characters at ingress;
/// human renderers replace them defensively at the output boundary.
pub(crate) fn contains_terminal_unsafe(text: &str) -> bool {
    text.chars().any(is_terminal_unsafe)
}

fn is_terminal_unsafe(character: char) -> bool {
    character.is_control() || is_terminal_format(character)
}

fn terminal_safe_char(character: char) -> char {
    if is_terminal_unsafe(character) {
        ' '
    } else {
        character
    }
}

/// Unicode's formatting characters are not covered by [`char::is_control`], but
/// bidi overrides, isolates, zero-width marks, and tag characters can still change
/// or conceal what a human sees in one terminal line. Keep this dependency-free
/// table explicit so the terminal boundary does not silently trust them.
fn is_terminal_format(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{13430}'..='\u{1343f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// Render a column-aligned table with `separator` between cells and `prefix` before
/// every line. Column widths include the header; the final column is never padded,
/// so no line gains trailing whitespace. Const-generic row width makes a missing or
/// extra cell a compile error at each call site.
pub(crate) fn aligned_table<const N: usize>(
    headers: [&str; N],
    rows: &[[String; N]],
    prefix: &str,
    separator: &str,
) -> Vec<String> {
    assert!(N > 0, "an aligned table needs at least one column");

    let mut widths = headers.map(|header| header.chars().count());
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(aligned_row(
        &headers.map(str::to_string),
        &widths,
        prefix,
        separator,
    ));
    for row in rows {
        lines.push(aligned_row(row, &widths, prefix, separator));
    }
    lines
}

fn aligned_row<const N: usize>(
    cells: &[String; N],
    widths: &[usize; N],
    prefix: &str,
    separator: &str,
) -> String {
    let mut line = prefix.to_string();
    for (index, (cell, width)) in cells.iter().zip(widths.iter()).enumerate() {
        if index > 0 {
            line.push_str(separator);
        }
        if index + 1 == cells.len() {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{cell:width$}"));
        }
    }
    line.truncate(line.trim_end().len());
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_safe_collapses_line_and_escape_controls() {
        let safe = terminal_safe("run\nnext\t\u{1b}[31mred\u{7}");
        assert_eq!(safe, "run next  [31mred ");
        assert!(
            safe.chars().all(|character| !character.is_control()),
            "no terminal control character survives: {safe:?}"
        );
    }

    #[test]
    fn terminal_safe_collapses_bidi_and_zero_width_formatting() {
        let safe = terminal_safe(
            "left\u{202e}override\u{202c}\u{2066}isolate\u{2069}\u{200b}zero\u{200e}mark\u{feff}",
        );
        assert_eq!(safe, "left override  isolate  zero mark ");
        assert!(
            safe.chars().all(|character| !is_terminal_format(character)),
            "no invisible formatting character survives: {safe:?}"
        );
    }

    #[test]
    fn terminal_safe_bounded_marks_character_safe_truncation() {
        let exact = "α".repeat(TERMINAL_FIELD_MAX_CHARS);
        assert_eq!(terminal_safe_bounded(&exact), exact);

        let oversized = format!("{}\nTAIL", "β".repeat(TERMINAL_FIELD_MAX_CHARS));
        let bounded = terminal_safe_bounded(&oversized);
        assert_eq!(
            bounded,
            format!("{}...", "β".repeat(TERMINAL_FIELD_MAX_CHARS))
        );
        assert!(!bounded.chars().any(char::is_control));
    }

    #[test]
    fn aligned_table_uses_header_and_rows_for_widths_without_trailing_spaces() {
        let rows = [
            ["1".to_string(), "long".to_string(), "x".to_string()],
            ["222".to_string(), "y".to_string(), "last".to_string()],
        ];
        let lines = aligned_table(["A", "HEADER", "C"], &rows, "  ", "  ");

        assert_eq!(
            lines,
            vec![
                "  A    HEADER  C",
                "  1    long    x",
                "  222  y       last",
            ]
        );
        assert!(
            lines.iter().all(|line| line.trim_end() == line),
            "the final column is never padded: {lines:?}"
        );

        let empty_final = [["value".to_string(), "  ".to_string()]];
        let lines = aligned_table(["A", "B"], &empty_final, "", "  ");
        assert_eq!(lines[1], "value");
        assert_eq!(lines[1].trim_end(), lines[1]);
    }

    #[test]
    fn aligned_table_measures_multibyte_cells_in_characters_like_padding_does() {
        let rows = [
            ["ééé".to_string(), "first".to_string()],
            ["x".to_string(), "second".to_string()],
        ];
        let lines = aligned_table(["A", "B"], &rows, "", "|");

        assert_eq!(lines, vec!["A  |B", "ééé|first", "x  |second"]);
    }
}
