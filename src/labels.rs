//! Operator labels shared by `run`, registry discovery, and aggregate commands.

use std::collections::BTreeMap;

/// Maximum key bytes and value characters. Labels are operator metadata rather
/// than arbitrary payloads, so bounding both keeps registry records and table
/// output predictable even when they come from an untrusted on-disk record.
pub const MAX_KEY_BYTES: usize = 64;
pub const MAX_VALUE_CHARS: usize = 256;

/// One parsed `KEY=VALUE` label from the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorLabel {
    pub key: String,
    pub value: String,
}

/// Parse the common label grammar used by `run --label` and aggregate filters.
/// Public so the CLI fuzz target can exercise the real parser with arbitrary
/// operator text; this is not a separate compatibility surface.
pub fn parse(raw: &str) -> Result<OperatorLabel, String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| "label must be `KEY=VALUE`".to_string())?;
    if !valid_key(key) {
        return Err(format!(
            "label key `{key}` must be 1-{MAX_KEY_BYTES} ASCII bytes, start with a letter or `_`, and contain only letters, digits, `.`, `-`, or `_`"
        ));
    }
    if !valid_value(value) {
        return Err(format!(
            "label value for `{key}` must be at most {MAX_VALUE_CHARS} characters and contain no terminal control or formatting characters"
        ));
    }
    Ok(OperatorLabel {
        key: key.to_string(),
        value: value.to_string(),
    })
}

/// Convert run labels to their registry/event map. Later occurrences win for a
/// duplicate key, matching the established `--env` override convention.
pub fn to_map(labels: &[OperatorLabel]) -> BTreeMap<String, String> {
    labels
        .iter()
        .map(|label| (label.key.clone(), label.value.clone()))
        .collect()
}

/// Whether every requested filter is present with the exact value. Multiple
/// filters therefore combine with logical AND; conflicting values for one key
/// intentionally match no run.
pub fn matches(labels: &BTreeMap<String, String>, filters: &[OperatorLabel]) -> bool {
    filters
        .iter()
        .all(|filter| labels.get(&filter.key) == Some(&filter.value))
}

/// Validate a key read from an untrusted registry record.
pub fn valid_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return false;
    }
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Validate a value read from an untrusted registry record. Mirrors
/// `cli::parse_run_id`'s terminal-safety bar: labels are display/discovery
/// strings persisted into registry records and echoed verbatim in the
/// `run_started` JSONL event, so they reject the same terminal control and
/// invisible Unicode formatting characters at ingress (see
/// `text::contains_terminal_unsafe`'s doc comment for that shared ingress
/// contract) rather than relying solely on the output-boundary sanitization
/// human renderers already apply.
pub fn valid_value(value: &str) -> bool {
    value.chars().count() <= MAX_VALUE_CHARS && !crate::text::contains_terminal_unsafe(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_pins_the_label_grammar() {
        assert_eq!(
            parse("batch.id=42").unwrap(),
            OperatorLabel {
                key: "batch.id".to_string(),
                value: "42".to_string(),
            }
        );
        assert!(parse("empty=").is_ok());
        for bad in [
            "missing",
            "=value",
            "9bad=value",
            "bad key=value",
            "key=a\nb",
            "key=bidi\u{202e}value",
        ] {
            assert!(parse(bad).is_err(), "expected `{bad}` to be rejected");
        }
    }

    #[test]
    fn maps_use_last_value_and_filters_are_conjunctive() {
        let labels = [
            parse("batch=old").unwrap(),
            parse("lane=test").unwrap(),
            parse("batch=new").unwrap(),
        ];
        let map = to_map(&labels);
        assert_eq!(map.get("batch").map(String::as_str), Some("new"));
        assert!(matches(
            &map,
            &[parse("batch=new").unwrap(), parse("lane=test").unwrap()]
        ));
        assert!(!matches(&map, &[parse("batch=old").unwrap()]));
    }
}
