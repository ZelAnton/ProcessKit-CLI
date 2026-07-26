//! Shared text normalization for human-readable terminal output.

/// Collapse terminal control characters to ordinary spaces before interpolating an
/// untrusted string into a human-readable line. JSON renderers deliberately do not
/// use this helper: `serde_json` escapes controls without changing the data contract.
pub(crate) fn terminal_safe(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
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

    let mut widths = headers.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.len());
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
}
