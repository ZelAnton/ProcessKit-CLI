//! The human-readable rendering of one lifecycle event, and the notices this
//! command prints about a line it could not read as one.
//!
//! # One shape, no per-event table
//!
//! Every event is rendered the same way — `<time>  <event>  key=value …` — with the
//! fields taken from the object itself, in sorted key order, rather than from a
//! per-event-type list of "interesting fields". That is a deliberate choice, not
//! laziness: a hand-written table of which fields matter for which event type would
//! be one more parallel enumeration of a closed contract, and this codebase's
//! recurring experience is that those drift silently the moment the schema gains a
//! field or an event type (K-020). Deriving the fields from the line means a stream
//! written by a *newer* runner — a new event type, a new field on an old one —
//! still renders in full here instead of quietly losing whatever this build had not
//! heard of.
//!
//! Scalars render bare (`code=0`, `source=child_exit`); an object or array renders
//! as its compact JSON (`command={"argv":null,…}`), which keeps a nested shape
//! readable on one line without inventing a second notation for it. The rendering
//! is for a human to read, not for a machine to parse: a string value may itself
//! contain spaces or `=`, so anything parsing this output is doing it wrong and
//! wants `--json`, which hands over the runner's own bytes untouched.
//!
//! # Everything here crosses the terminal barrier
//!
//! The events file is *untrusted input* (`docs/threat-model.md`, "Untrusted
//! inputs": any local process can write one, and `events --file` reads an
//! arbitrary caller-specified path), so every fragment that reaches a terminal —
//! key, value, timestamp, event name, and the raw text of a line that failed to
//! parse — goes through [`text::terminal_safe_bounded`], the shared ingress/render
//! barrier `list`/`inspect` already use (K-091), rather than any narrower check of
//! this module's own. This module covers the *rendering* half of that inventory;
//! [`crate::events_cmd`]'s own docs list all of it, including the one operator
//! string outside this file — the stream's locator.

use serde_json::{Map, Value};

use crate::text;

/// The envelope fields rendered as the line's prefix (or, for `schema_version`,
/// deliberately not rendered at all: it is the same constant on every line of a
/// conforming stream, and `--json`/`--validate` are where it matters).
const ENVELOPE: [&str; 3] = ["schema_version", "time", "event"];

/// Stands in for an envelope field the line does not carry. A conforming stream
/// never shows it; a malformed line renders as much as it does have rather than
/// being dropped.
const MISSING: &str = "-";

/// Render one event object as a single human-readable line.
pub(crate) fn event_line(event: &Map<String, Value>) -> String {
    let mut line = format!("{}  {}", envelope(event, "time"), envelope(event, "event"));
    // Sorted explicitly rather than relying on the map's own iteration order, so the
    // rendering is stable no matter how `serde_json` is configured to store keys.
    let mut keys: Vec<&String> = event
        .keys()
        .filter(|key| !ENVELOPE.contains(&key.as_str()))
        .collect();
    keys.sort_unstable();
    for key in keys {
        line.push(' ');
        line.push_str(&field(key, &event[key]));
    }
    line
}

/// One envelope field as text, or [`MISSING`] when the line does not carry it as a
/// string.
fn envelope(event: &Map<String, Value>, name: &str) -> String {
    event
        .get(name)
        .and_then(Value::as_str)
        .map_or_else(|| MISSING.to_string(), text::terminal_safe_bounded)
}

/// One `key=value` pair. Scalars render bare; composites render as compact JSON;
/// an empty string renders as `""` so `key=` can never look like a rendering bug.
fn field(key: &str, value: &Value) -> String {
    let rendered = match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) if text.is_empty() => "\"\"".to_string(),
        Value::String(text) => text.clone(),
        composite => composite.to_string(),
    };
    format!(
        "{}={}",
        text::terminal_safe_bounded(key),
        text::terminal_safe_bounded(&rendered)
    )
}

/// The notice printed for a line this command could not read as an event —
/// naming the line by number, saying why, and echoing what was actually there
/// (sanitized and bounded like everything else).
pub(crate) fn unreadable_line(number: usize, reason: &str, raw: &str) -> String {
    format!(
        "line {number}: {reason}: {}",
        text::terminal_safe_bounded(raw)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(raw: &str) -> Map<String, Value> {
        match serde_json::from_str(raw).expect("the fixture is valid JSON") {
            Value::Object(map) => map,
            other => panic!("expected a JSON object, got {other}"),
        }
    }

    /// The shape an operator actually reads: the timestamp, the event kind, then
    /// every remaining field of the event in sorted order.
    #[test]
    fn an_event_renders_as_time_kind_then_its_own_fields() {
        let line = event_line(&object(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":null}"#,
        ));
        assert_eq!(
            line,
            "2026-07-22T09:00:00.000Z  runner_exit child_code=null code=0 source=child_exit"
        );
    }

    /// A nested object or array stays on the one line as compact JSON — readable,
    /// and never silently dropped just because it is not a scalar. Its own keys come
    /// out in `serde_json`'s storage order, which is sorted here (the crate's default
    /// map is a `BTreeMap`), so the rendering is deterministic without this module
    /// re-sorting anything nested itself — one more reason a caller that needs the
    /// runner's exact bytes wants `--json`.
    #[test]
    fn composite_fields_render_as_compact_json() {
        let line = event_line(&object(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"members_snapshot","members":[{"pid":7,"ppid":1,"name":"child","start_time":null}]}"#,
        ));
        assert_eq!(
            line,
            "2026-07-22T09:00:00.000Z  members_snapshot \
             members=[{\"name\":\"child\",\"pid\":7,\"ppid\":1,\"start_time\":null}]"
        );
    }

    /// A field this build has never heard of still renders: the fields come from the
    /// line, not from a table of what this version happens to know (the drift this
    /// module's docs explain).
    #[test]
    fn an_unknown_event_type_and_unknown_fields_still_render() {
        let line = event_line(&object(
            r#"{"schema_version":2,"time":"2027-01-01T00:00:00.000Z","event":"teleported","destination":"mars","crew":3}"#,
        ));
        assert_eq!(
            line,
            "2027-01-01T00:00:00.000Z  teleported crew=3 destination=mars"
        );
    }

    /// A line missing its envelope renders what it does have rather than being
    /// dropped or panicking.
    #[test]
    fn a_line_without_an_envelope_renders_the_absent_marker() {
        let line = event_line(&object(r#"{"code":3}"#));
        assert_eq!(line, "-  - code=3");
    }

    /// The untrusted-input property, at the boundary an operator's terminal sees: a
    /// forged newline cannot add a line, an escape sequence cannot move the cursor,
    /// a bidi override cannot reverse what is displayed, and an oversized value is
    /// truncated with a visible marker.
    ///
    /// The fixture spells the dangerous characters as JSON's own `\uXXXX` escapes,
    /// so the *decoded* values carry the real newline / ESC / bidi override while
    /// this file's source stays free of invisible codepoints (which `rustc` rejects
    /// in a literal anyway).
    #[test]
    fn every_rendered_fragment_crosses_the_terminal_barrier() {
        let forged = event_line(&object(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z\nFORGED","event":"runner_exit","source":"child\u001b[31m","message":"bidi\u202eoverride"}"#,
        ));
        assert_eq!(
            forged.lines().count(),
            1,
            "a forged newline cannot add a line"
        );
        assert!(
            forged.chars().all(|character| !character.is_control()),
            "no terminal control survives: {forged:?}"
        );
        assert!(
            !forged.contains('\u{202e}'),
            "no invisible formatting character survives: {forged:?}"
        );

        let oversized = event_line(&object(&format!(
            r#"{{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","message":"{}"}}"#,
            "m".repeat(text::TERMINAL_FIELD_MAX_CHARS + 40)
        )));
        assert!(
            oversized.contains(&format!(
                "message={}...",
                "m".repeat(text::TERMINAL_FIELD_MAX_CHARS)
            )),
            "an oversized value is bounded with a visible marker: {oversized}"
        );
    }

    /// An empty string is rendered as `""` so it reads as a value, not as a missing
    /// one.
    #[test]
    fn an_empty_string_value_is_visible() {
        let line = event_line(&object(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"spawn_failed","message":""}"#,
        ));
        assert_eq!(line, "2026-07-22T09:00:00.000Z  spawn_failed message=\"\"");
    }

    /// The unreadable-line notice names the line, the reason, and what was there —
    /// with the untrusted echo held to the same barrier as a rendered field.
    #[test]
    fn the_unreadable_line_notice_is_specific_and_safe() {
        let notice = unreadable_line(7, "not valid JSON", "{\"broken\"\u{1b}[31m");
        assert!(notice.starts_with("line 7: not valid JSON: "));
        assert!(
            notice.chars().all(|character| !character.is_control()),
            "the echoed text is sanitized: {notice:?}"
        );
    }
}
