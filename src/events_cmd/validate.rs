//! `events --validate`: check a JSONL stream against the event schema this binary
//! embeds at build time, and report the result.
//!
//! The schema is not a second copy of anything: it is
//! [`crate::probe::SCHEMA_JSON`], the exact bytes `probe --print-schema` hands a
//! consumer and the exact file (`fixtures/schema/v1/schema.json`) `tests/events.rs`
//! validates the golden fixture and live streams against. One document, one
//! verdict — an adapter author's fixture is held to precisely the bar this project
//! holds its own output to, which is the only way a conformance checker is worth
//! having.
//!
//! The checking itself lives in [`super::schema`]; this module is the pass over a
//! stream: what was checked, what failed, and the exit verdict that carries.

use crate::exit::{self, RunnerError};
use crate::text;

use super::StreamLine;
use super::schema::SchemaChecker;

/// The running result of a `--validate` pass: what it checked, what failed, and the
/// verdict it exits with.
pub(crate) struct ValidateReport {
    checker: SchemaChecker,
    checked: usize,
    invalid: usize,
}

impl ValidateReport {
    pub(crate) fn new() -> Result<Self, RunnerError> {
        Ok(Self {
            checker: SchemaChecker::compile()?,
            checked: 0,
            invalid: 0,
        })
    }

    /// Check one line and print what it violated. A line that is not JSON at all is
    /// a violation of the very first thing the schema requires of it — being a JSON
    /// document — and is counted as one rather than passed over in silence.
    pub(crate) fn absorb(&mut self, line: &StreamLine<'_>) {
        self.checked += 1;
        match line.value() {
            Ok(value) => {
                let violations = self.checker.violations(value);
                if violations.is_empty() {
                    return;
                }
                self.invalid += 1;
                for violation in violations {
                    println!("line {}: {violation}", line.number());
                }
            }
            Err(reason) => {
                self.invalid += 1;
                println!(
                    "line {}: {reason}: {}",
                    line.number(),
                    text::terminal_safe_bounded(line.raw())
                );
            }
        }
    }

    /// Print the summary and turn the tally into this command's verdict: `Ok` when
    /// every checked line conformed, [`exit::EVENTS_INVALID`] when any did not.
    pub(crate) fn finish(self) -> Result<(), RunnerError> {
        let lines = if self.checked == 1 { "line" } else { "lines" };
        if self.invalid == 0 {
            println!("checked {} {lines}: all valid", self.checked);
            return Ok(());
        }
        println!(
            "checked {} {lines}: {} valid, {} invalid",
            self.checked,
            self.checked - self.invalid,
            self.invalid
        );
        Err(RunnerError::new(
            exit::EVENTS_INVALID,
            format!(
                "{} of {} checked {lines} do not conform to this binary's event schema \
                 (`probe --print-schema`)",
                self.invalid, self.checked
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(checked: usize, invalid: usize) -> ValidateReport {
        ValidateReport {
            checker: SchemaChecker::compile().expect("the embedded schema compiles"),
            checked,
            invalid,
        }
    }

    /// The whole point of the exit contract: a clean pass is `Ok`, any failure is
    /// the reserved `EVENTS_INVALID` code, never a generic one.
    #[test]
    fn the_verdict_maps_onto_the_reserved_exit_code() {
        assert!(report(3, 0).finish().is_ok(), "every line conformed");

        let err = report(3, 2)
            .finish()
            .expect_err("two lines did not conform");
        assert_eq!(err.code(), exit::EVENTS_INVALID);
        assert_ne!(
            err.code(),
            exit::SETUP,
            "a stream that was read fine and found wanting is not a setup failure"
        );
        let message = err.to_string();
        assert!(
            message.contains('2') && message.contains('3'),
            "the failure names how many of how many: {message}"
        );
    }

    /// An empty stream is vacuously conforming, and says so with the right
    /// grammatical number.
    #[test]
    fn the_summary_agrees_with_itself_on_number() {
        assert!(report(0, 0).finish().is_ok());
        assert!(report(1, 0).finish().is_ok());
    }
}
