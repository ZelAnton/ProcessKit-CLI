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
}
