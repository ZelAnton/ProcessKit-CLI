//! A deliberately tiny, **fail-closed** matcher for the anchored regular
//! expressions the embedded event schema's `pattern` keywords use.
//!
//! The four patterns in `fixtures/schema/v1/schema.json` are simple and fully
//! anchored — a timestamp shape, two hex digests, and a label-key shape:
//!
//! ```text
//! ^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$
//! ^[0-9a-f]{64}$
//! ^[A-Za-z_][A-Za-z0-9._-]{0,63}$
//! ```
//!
//! Supporting exactly that shape — literals, `\d`, escaped punctuation, character
//! classes with ranges, and bounded `{n}` / `{n,m}` counts, between a leading `^`
//! and a trailing `$` — is a bounded, testable job. Supporting *regular
//! expressions* is not, and this file does not pretend to: alternation, groups,
//! unbounded quantifiers, `.`, and every other construct are **rejected at compile
//! time** rather than silently mismatched. That refusal is what makes the omission
//! safe (see [`crate::events_cmd::schema`]): a pattern this matcher cannot
//! interpret stops the checker from running at all, so it can never quietly accept
//! a value a real engine would reject.
//!
//! `pattern` in JSON Schema is an unanchored *search*, and the difference matters —
//! so an un-anchored pattern is rejected here too rather than guessed at. All four
//! patterns above carry both anchors explicitly.

/// The most repetitions a `{n,m}` bound may ask for. Far above anything the event
/// schema needs (its largest is 64) and low enough that a hostile pattern cannot
/// turn matching into a denial of service. A larger bound is a compile-time
/// refusal, like every other unsupported construct.
const MAX_REPETITIONS: usize = 1024;

/// A compiled anchored pattern: a flat sequence of quantified single-character
/// atoms, matched left to right against the whole string.
#[derive(Debug)]
pub(crate) struct Anchored {
    elements: Vec<Element>,
}

#[derive(Debug)]
struct Element {
    atom: Atom,
    min: usize,
    max: usize,
}

#[derive(Debug)]
enum Atom {
    /// One exact character.
    Literal(char),
    /// `\d` — an ASCII digit, exactly as ECMA-262 defines it (`[0-9]`), not
    /// `char::is_numeric`, which would also accept other Unicode digits.
    Digit,
    /// `[...]`, optionally negated.
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

impl Atom {
    fn matches(&self, character: char) -> bool {
        match self {
            Self::Literal(expected) => character == *expected,
            Self::Digit => character.is_ascii_digit(),
            Self::Class { negated, items } => {
                let inside = items.iter().any(|item| match item {
                    ClassItem::Char(item) => character == *item,
                    ClassItem::Range(low, high) => (*low..=*high).contains(&character),
                });
                inside != *negated
            }
        }
    }
}

impl Anchored {
    /// Compile `pattern`, or explain why this matcher will not attempt it.
    pub(crate) fn compile(pattern: &str) -> Result<Self, String> {
        let body = pattern
            .strip_prefix('^')
            .and_then(|rest| rest.strip_suffix('$'))
            .ok_or_else(|| {
                format!(
                    "pattern `{pattern}` is not fully anchored (`^`…`$`), and an unanchored \
                     search is not supported"
                )
            })?;

        let characters: Vec<char> = body.chars().collect();
        let mut elements = Vec::new();
        let mut index = 0;
        while index < characters.len() {
            let element_start = index;
            let atom = parse_atom(&characters, &mut index, pattern)?;
            let (min, max) = parse_quantifier(&characters, &mut index, pattern)?;
            ensure_parser_progress(element_start, index, characters.len(), pattern)?;
            elements.push(Element { atom, min, max });
        }
        Ok(Self { elements })
    }

    /// Whether `text` matches the whole pattern.
    pub(crate) fn matches(&self, text: &str) -> bool {
        let characters: Vec<char> = text.chars().collect();
        self.match_from(&characters, 0, 0)
    }

    /// Match `elements[element..]` against `characters[position..]`, greedily and
    /// with backtracking. Every atom consumes exactly one character and every
    /// bound is finite, so the search is bounded by (elements × repetitions) and
    /// cannot degenerate.
    fn match_from(&self, characters: &[char], element: usize, position: usize) -> bool {
        let Some(current) = self.elements.get(element) else {
            // Every element is consumed: the `$` anchor requires the string to be
            // as well.
            return position == characters.len();
        };

        // Iterator bounds make termination independent of an incrementing
        // counter, so a counter regression cannot turn matching into a denial
        // of service.
        let available = characters
            .iter()
            .skip(position)
            .take(current.max)
            .take_while(|character| current.atom.matches(**character))
            .count();
        if available < current.min {
            return false;
        }

        for count in (current.min..=available).rev() {
            if self.match_from(characters, element + 1, position + count) {
                return true;
            }
        }
        false
    }
}

fn ensure_parser_progress(
    element_start: usize,
    next_index: usize,
    input_len: usize,
    pattern: &str,
) -> Result<(), String> {
    // This is a denial-of-service boundary: every compiled element must consume
    // input. A helper regression must fail closed instead of letting the compile
    // loop grow `elements` without bound.
    if next_index <= element_start || next_index > input_len {
        return Err(format!(
            "pattern `{pattern}` parser made no bounded forward progress"
        ));
    }
    Ok(())
}

fn advance_parser(index: &mut usize, input_len: usize, pattern: &str) -> Result<(), String> {
    let current = *index;
    let next = current
        .checked_add(1)
        .ok_or_else(|| format!("pattern `{pattern}` parser cursor overflowed"))?;
    ensure_parser_progress(current, next, input_len, pattern)?;
    *index = next;
    Ok(())
}

fn is_escaped_punctuation(character: char) -> bool {
    matches!(
        character,
        '.' | '\\'
            | '^'
            | '$'
            | '['
            | ']'
            | '('
            | ')'
            | '{'
            | '}'
            | '|'
            | '*'
            | '+'
            | '?'
            | '-'
            | '/'
    )
}

fn parse_atom(characters: &[char], index: &mut usize, pattern: &str) -> Result<Atom, String> {
    let character = characters[*index];
    advance_parser(index, characters.len(), pattern)?;
    match character {
        '\\' => {
            let Some(escaped) = characters.get(*index).copied() else {
                return Err(format!("pattern `{pattern}` ends with a dangling escape"));
            };
            advance_parser(index, characters.len(), pattern)?;
            match escaped {
                'd' => Ok(Atom::Digit),
                // Escaped punctuation stands for itself. Restricted to the
                // characters a schema pattern plausibly escapes, so an unknown
                // class escape (`\w`, `\s`, `\b`, …) is refused rather than read as
                // a literal letter, which would silently change the meaning.
                escaped if is_escaped_punctuation(escaped) => Ok(Atom::Literal(escaped)),
                other => Err(format!(
                    "pattern `{pattern}` uses the unsupported escape `\\{other}`"
                )),
            }
        }
        '[' => parse_class(characters, index, pattern),
        // Every remaining metacharacter is a construct this matcher deliberately
        // does not implement; refusing is the whole point (see the module docs).
        '.' | '*' | '+' | '?' | '|' | '(' | ')' | '^' | '$' => Err(format!(
            "pattern `{pattern}` uses the unsupported construct `{character}`"
        )),
        literal => Ok(Atom::Literal(literal)),
    }
}

fn parse_class(characters: &[char], index: &mut usize, pattern: &str) -> Result<Atom, String> {
    let negated = characters.get(*index) == Some(&'^');
    if negated {
        advance_parser(index, characters.len(), pattern)?;
    }
    let mut items = Vec::new();
    // The independent iteration budget keeps this parser bounded even if a
    // cursor-advance regression slips past its local invariant.
    for _ in 0..=characters.len() {
        let Some(character) = characters.get(*index).copied() else {
            return Err(format!("pattern `{pattern}` has an unterminated `[` class"));
        };
        advance_parser(index, characters.len(), pattern)?;
        if character == ']' {
            if items.is_empty() {
                return Err(format!("pattern `{pattern}` has an empty `[]` class"));
            }
            return Ok(Atom::Class { negated, items });
        }
        let low = if character == '\\' {
            let Some(escaped) = characters.get(*index).copied() else {
                return Err(format!("pattern `{pattern}` ends with a dangling escape"));
            };
            advance_parser(index, characters.len(), pattern)?;
            if !is_escaped_punctuation(escaped) {
                return Err(format!(
                    "pattern `{pattern}` uses the unsupported escape `\\{escaped}`"
                ));
            }
            escaped
        } else {
            character
        };
        // A `-` immediately before the closing `]` is a literal `-`, the ordinary
        // regex convention the label-key pattern above relies on.
        if characters.get(*index) == Some(&'-') && characters.get(*index + 1) != Some(&']') {
            if character == '\\' || characters.get(*index + 1) == Some(&'\\') {
                return Err(format!(
                    "pattern `{pattern}` uses an escaped range endpoint"
                ));
            }
            advance_parser(index, characters.len(), pattern)?;
            let Some(high) = characters.get(*index).copied() else {
                return Err(format!("pattern `{pattern}` has an unterminated range"));
            };
            advance_parser(index, characters.len(), pattern)?;
            if high < low {
                return Err(format!(
                    "pattern `{pattern}` has an inverted range `{low}-{high}`"
                ));
            }
            items.push(ClassItem::Range(low, high));
        } else {
            items.push(ClassItem::Char(low));
        }
    }
    Err(format!(
        "pattern `{pattern}` class parser made no bounded forward progress"
    ))
}

/// The `{n}` / `{n,m}` bound following an atom, or `(1, 1)` when there is none.
fn parse_quantifier(
    characters: &[char],
    index: &mut usize,
    pattern: &str,
) -> Result<(usize, usize), String> {
    if characters.get(*index) != Some(&'{') {
        // `*`, `+` and `?` are unbounded or optional forms this matcher does not
        // implement; catching them here (rather than as a stray literal atom on the
        // next pass) keeps the refusal precise.
        if let Some(quantifier @ ('*' | '+' | '?')) = characters.get(*index).copied() {
            return Err(format!(
                "pattern `{pattern}` uses the unsupported quantifier `{quantifier}`"
            ));
        }
        return Ok((1, 1));
    }
    advance_parser(index, characters.len(), pattern)?;

    let mut bounds: Vec<String> = vec![String::new()];
    let mut closed = false;
    // Keep termination independent of cursor movement for the same reason as
    // the character-class parser above.
    for _ in 0..=characters.len() {
        let Some(character) = characters.get(*index).copied() else {
            return Err(format!(
                "pattern `{pattern}` has an unterminated `{{` bound"
            ));
        };
        advance_parser(index, characters.len(), pattern)?;
        match character {
            '}' => {
                closed = true;
                break;
            }
            ',' => {
                if bounds.len() != 1 {
                    return Err(format!(
                        "pattern `{pattern}` has more than one comma in a `{{` bound"
                    ));
                }
                bounds.push(String::new());
            }
            digit @ '0'..='9' => {
                bounds
                    .last_mut()
                    .expect("the bound list is never empty")
                    .push(digit);
            }
            other => {
                return Err(format!(
                    "pattern `{pattern}` has an unsupported `{{` bound containing `{other}`"
                ));
            }
        }
    }
    if !closed {
        return Err(format!(
            "pattern `{pattern}` quantifier parser made no bounded forward progress"
        ));
    }

    let parse = |raw: &String| -> Result<usize, String> {
        raw.parse::<usize>()
            .map_err(|_| format!("pattern `{pattern}` has an unreadable `{{` bound"))
    };
    let (min, max) = match bounds.as_slice() {
        [only] => {
            let exact = parse(only)?;
            (exact, exact)
        }
        [low, high] => match high.as_str() {
            // `{n,}` is unbounded, which this matcher does not implement.
            "" => {
                return Err(format!(
                    "pattern `{pattern}` uses an open-ended `{{n,}}` bound, which is not supported"
                ));
            }
            _ => (parse(low)?, parse(high)?),
        },
        _ => {
            return Err(format!(
                "pattern `{pattern}` has an unsupported number of `{{` bounds"
            ));
        }
    };
    if min > max {
        return Err(format!("pattern `{pattern}` has an inverted `{{` bound"));
    }
    if max > MAX_REPETITIONS {
        return Err(format!(
            "pattern `{pattern}` asks for more than {MAX_REPETITIONS} repetitions"
        ));
    }
    Ok((min, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(pattern: &str) -> Anchored {
        Anchored::compile(pattern).unwrap_or_else(|err| panic!("compile `{pattern}`: {err}"))
    }

    #[test]
    fn parser_progress_invariant_rejects_stalls_regressions_and_overshoot() {
        assert!(ensure_parser_progress(1, 2, 3, "^a$").is_ok());
        for (next_index, input_len) in [(1, 3), (0, 3), (4, 3)] {
            let error = ensure_parser_progress(1, next_index, input_len, "^a$")
                .expect_err("a non-forward cursor must fail closed");
            assert!(error.contains("no bounded forward progress"), "{error}");
        }
    }

    /// The event schema's own timestamp pattern, against the shape the emitter
    /// writes and against every near-miss that must not pass.
    #[test]
    fn the_timestamp_pattern_accepts_only_the_emitted_shape() {
        let time = compiled(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$");
        assert!(time.matches("2026-07-22T09:00:00.000Z"));
        assert!(time.matches("0001-01-01T00:00:00.999Z"));
        for bad in [
            "2026-07-22T09:00:00.000",   // no trailing Z
            "2026-07-22T09:00:00.0000Z", // four fractional digits
            "2026-07-22T09:00:00.00Z",   // two fractional digits
            "2026-07-22 09:00:00.000Z",  // space instead of T
            "2026-07-22T09:00:00x000Z",  // the escaped dot is a literal dot
            "x2026-07-22T09:00:00.000Z", // leading junk: the `^` anchor binds
            "2026-07-22T09:00:00.000Zx", // trailing junk: the `$` anchor binds
            "",
        ] {
            assert!(!time.matches(bad), "must reject {bad:?}");
        }
    }

    /// The digest pattern: exactly 64 lowercase hex characters, counted.
    #[test]
    fn the_digest_pattern_counts_exactly() {
        let digest = compiled("^[0-9a-f]{64}$");
        assert!(digest.matches(&"a".repeat(64)));
        assert!(digest.matches(&"0123456789abcdef".repeat(4)));
        assert!(!digest.matches(&"a".repeat(63)));
        assert!(!digest.matches(&"a".repeat(65)));
        assert!(
            !digest.matches(&format!("{}A", "a".repeat(63))),
            "uppercase is outside the class"
        );

        let maximum = compiled("^a{1024}$");
        assert!(maximum.matches(&"a".repeat(MAX_REPETITIONS)));
    }

    /// The label-key pattern exercises the two remaining features: a `{0,n}`
    /// range, and a literal `-` at the end of a class.
    #[test]
    fn the_label_key_pattern_handles_ranges_and_a_trailing_dash() {
        let key = compiled("^[A-Za-z_][A-Za-z0-9._-]{0,63}$");
        assert!(key.matches("batch"));
        assert!(key.matches("_x"));
        assert!(key.matches("a"), "the {{0,63}} tail may be empty");
        assert!(key.matches("a-b.c_d9"), "the trailing `-` is a literal");
        assert!(
            key.matches(&format!("a{}", "b".repeat(63))),
            "64 characters"
        );
        assert!(
            !key.matches(&format!("a{}", "b".repeat(64))),
            "65 characters is past the bound"
        );
        assert!(!key.matches("9lead"), "a digit cannot lead");
        assert!(!key.matches(""), "the first atom is mandatory");
        assert!(!key.matches("has space"));
    }

    /// A negated class and an exact count, neither of which the event schema uses
    /// today — implemented because they are part of the same small grammar, and
    /// tested so they are not merely assumed to work.
    #[test]
    fn negated_classes_and_exact_counts_work() {
        let negated = compiled("^[^abc]{2}$");
        assert!(negated.matches("xy"));
        assert!(negated.matches("^x"), "the negation marker is not an item");
        assert!(negated.matches("[x"), "the opening bracket is not an item");
        assert!(!negated.matches("ax"));
        assert!(!negated.matches("x"));

        let equal_range = compiled("^[a-a]$");
        assert!(
            equal_range.matches("a"),
            "an equal range contains its endpoint"
        );
        assert!(!equal_range.matches("b"));
    }

    /// Backtracking: a greedy element that swallowed too much gives characters
    /// back so a later element can match.
    #[test]
    fn a_greedy_element_backtracks_for_a_later_one() {
        let pattern = compiled("^[a-z]{1,5}z$");
        assert!(pattern.matches("abcz"), "the class must give the `z` back");
        assert!(pattern.matches("az"));
        assert!(
            !pattern.matches("z"),
            "the class needs at least one character"
        );
    }

    /// The fail-closed half — the property that makes this matcher's small grammar
    /// safe. Every construct it does not implement is a compile-time refusal, never
    /// a silent mismatch.
    #[test]
    fn unsupported_constructs_are_refused_at_compile_time() {
        for unsupported in [
            "[0-9]+",     // unanchored
            "^[0-9]+$",   // unbounded quantifier
            "^a*$",       // unbounded quantifier
            "^a?$",       // optional
            "^(ab)$",     // group
            "^a|b$",      // alternation
            "^.$",        // any-character
            r"^\w$",      // class escape
            r"^\s{2}$",   // class escape
            r"^[\d]$",    // class escape
            r"^[\w]$",    // class escape
            r"^[\b]$",    // unsupported non-punctuation class escape
            r"^[\x]$",    // unsupported non-punctuation class escape
            r"^[a-\d]$",  // escaped upper range endpoint
            r"^[\d-a]$",  // escaped lower range endpoint
            r"^[!-\]]$",  // escaped punctuation range endpoint
            "^a{2,}$",    // open-ended bound
            "^a{1,2,3}$", // more than one comma
            "^a{3,1}$",   // inverted bound
            "^a{2000}$",  // beyond the repetition cap
            "^[a-$",      // unterminated class
            "^[]$",       // empty class
            "^[z-a]$",    // inverted range
            r"^a\$",      // dangling escape (the `$` is consumed as anchor)
            "^a{x}$",     // non-numeric bound
        ] {
            assert!(
                Anchored::compile(unsupported).is_err(),
                "`{unsupported}` must be refused, not guessed at"
            );
        }

        let open_ended = Anchored::compile("^a{2,}$")
            .expect_err("an open-ended repetition must be refused explicitly");
        assert!(open_ended.contains("open-ended"), "{open_ended}");
    }
}
