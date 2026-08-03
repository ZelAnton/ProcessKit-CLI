//! The checker behind `events --validate`: it interprets the JSON Schema document
//! this binary embeds ([`crate::probe::SCHEMA_JSON`]) directly, over the small
//! keyword subset that document actually uses.
//!
//! # Why not a JSON Schema engine
//!
//! Because the answer must be *right*, and it must not cost the shipped binary its
//! dependency posture. The obvious implementation — link the `jsonschema` crate the
//! test tier already uses — adds roughly **eighty-five crates** to the runtime
//! dependency tree of a process-containment tool (ICU, `wasm-bindgen`,
//! `num-bigint`, `fancy-regex`, …) and two license families the runtime allow-list
//! in `deny.toml` does not carry. That is not a proportionate price for one
//! diagnostic flag in a binary whose own threat model is partly about what it links
//! (`docs/threat-model.md`), so the crate stays a **dev**-dependency and this module
//! interprets the document instead. Hand-rolling a small primitive rather than
//! taking a large dependency for it is this project's standing convention — the
//! same one behind its own SHA-256, RFC 3339 validator, and duration grammar.
//!
//! # Why that is safe: fail closed on anything unimplemented
//!
//! A partial validator that silently ignores a keyword it does not know is *worse
//! than no validator*: it answers "valid" for a document a real engine rejects,
//! which is exactly the false "ok" a conformance gate exists to prevent (the same
//! failure `probe`'s fail-closed contract is built around, K-013/K-076). So this
//! module never ignores anything:
//!
//! - [`SchemaChecker::compile`] walks the **whole** embedded document up front and
//!   fails if it meets a keyword outside [`SUPPORTED_KEYWORDS`], a `type` name it
//!   does not implement, a `$ref` that is not a local `#/$defs/…`, or a `pattern`
//!   [`pattern::Anchored`] will not compile. There is no "skip what I do not
//!   understand" path anywhere in this file.
//! - The document is a compile-time constant this crate ships, so that walk either
//!   succeeds for every build or fails for every build — and
//!   `the_embedded_schema_compiles` pins which. A future schema edit that reaches
//!   for a new keyword breaks the build's tests rather than quietly weakening the
//!   checker.
//! - The verdict itself is held to a real engine's: `tests/events.rs` compares this
//!   checker against the `jsonschema` crate, line for line, over the golden fixture
//!   and a generated corpus of mutations of it. "Agrees with a real JSON Schema
//!   implementation" is therefore a tested fact about this code, not a claim in a
//!   comment.
//!
//! # Reporting: naming the branch, not just "no match"
//!
//! The document is a 14-way `oneOf` over the event types, so a bad line's honest
//! root-level verdict — *"matches none of the 14 shapes"* — says nothing useful
//! about *what* is wrong. When a failing line's own `event` tag names a branch, the
//! report is that branch's errors instead (`/code: expected type "integer", found
//! string`). The tag-to-branch map is derived from the document itself — every
//! `oneOf` entry's `$ref` resolved into `$defs`, each branch's own
//! `properties.event.const` read as its tag — so there is no hand-maintained event
//! list here to drift out of sync with the schema (K-020). Selecting a branch only
//! ever *explains* a failure the whole document already declared; it can never turn
//! an invalid line into a valid one.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::exit::{self, RunnerError};
use crate::probe::SCHEMA_JSON;
use crate::text;

use super::pattern::Anchored;

/// Every keyword this module implements. A schema object carrying anything else
/// fails [`SchemaChecker::compile`] — see the module docs on why silence would be
/// the one unacceptable behavior here.
const SUPPORTED_KEYWORDS: &[&str] = &[
    // Assertions.
    "$ref",
    "type",
    "const",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "propertyNames",
    "items",
    "minimum",
    "maximum",
    "maxLength",
    "pattern",
    "oneOf",
    "anyOf",
    // The definition container every `$ref` in this document points into. Not an
    // assertion itself: its members are schemas in their own right, walked (and so
    // keyword-checked) by `walk_supported`.
    "$defs",
    // Annotations: carried by the document for humans, with no effect on validity.
    // `format` is annotation-only in draft 2020-12, and every `format` in this
    // document is paired with a `pattern` that says the same thing normatively.
    "$schema",
    "$id",
    "title",
    "description",
    "format",
];

/// The JSON type names `type` may use, and the one place they are interpreted.
const SUPPORTED_TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

/// Where a violation was found when it concerns the event object as a whole (an
/// empty JSON Pointer).
const WHOLE_EVENT: &str = "(whole event)";

/// The prefix every local reference in the embedded document uses.
const DEFS_PREFIX: &str = "#/$defs/";

/// One reason a line does not conform: where, and what.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Violation {
    /// JSON Pointer into the offending event (`/command/argv_sha256`), or empty for
    /// the event as a whole.
    path: String,
    message: String,
}

/// The compiled checker: the embedded document, plus the patterns it uses,
/// compiled once.
///
/// `#[doc(hidden)] pub` (like the fuzz/bench-facing internals elsewhere in this
/// crate, K-041/K-044) purely so `tests/events.rs` can hold its verdict against a
/// real JSON Schema engine's.
#[doc(hidden)]
pub struct SchemaChecker {
    document: Value,
    patterns: HashMap<String, Anchored>,
    /// `event` tag → the `$defs` name of the `oneOf` branch that pins it. Derived
    /// from the document; empty if the document's shape is not the one this
    /// derivation understands, in which case a failure reports the root verdict.
    branches: Vec<(String, String)>,
}

impl SchemaChecker {
    /// Compile the embedded schema, refusing anything this module cannot check.
    ///
    /// A failure is a genuine invariant violation — the document is a compile-time
    /// constant this crate ships and tests — so it takes [`exit::INTERNAL`] rather
    /// than a code that would suggest the caller could fix it.
    #[doc(hidden)]
    pub fn compile() -> Result<Self, RunnerError> {
        let document: Value = serde_json::from_str(SCHEMA_JSON).map_err(|err| {
            internal(format!(
                "the embedded event schema is not valid JSON: {err}"
            ))
        })?;
        let mut patterns = HashMap::new();
        walk_supported(&document, &document, &mut patterns).map_err(internal)?;
        let branches = branch_tags(&document);
        Ok(Self {
            document,
            patterns,
            branches,
        })
    }

    /// Every way `event` violates the schema, rendered for a report. Empty means it
    /// conforms.
    #[doc(hidden)]
    pub fn violations(&self, event: &Value) -> Vec<String> {
        let mut found = Vec::new();
        self.check(&self.document, event, "", &mut found);
        found.iter().map(Violation::describe).collect()
    }

    /// Whether `event` conforms — the whole-document verdict, with no rendering.
    #[doc(hidden)]
    pub fn conforms(&self, event: &Value) -> bool {
        let mut found = Vec::new();
        self.check(&self.document, event, "", &mut found);
        found.is_empty()
    }

    /// Validate `instance` at `path` against `schema`, appending every violation.
    fn check(&self, schema: &Value, instance: &Value, path: &str, out: &mut Vec<Violation>) {
        let Some(schema) = schema.as_object() else {
            // `compile` already rejected any non-object schema node, so this is
            // unreachable for the embedded document.
            return;
        };

        if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
            && let Some(target) = self.resolve(reference)
        {
            self.check(target, instance, path, out);
        }

        if let Some(expected) = schema.get("type")
            && !type_matches(expected, instance)
        {
            out.push(Violation::new(
                path,
                format!(
                    "expected type {}, found {}",
                    render_type(expected),
                    type_name(instance)
                ),
            ));
            // Every remaining assertion is about a value of the expected type;
            // reporting them too would bury the one that matters.
            return;
        }

        if let Some(expected) = schema.get("const")
            && instance != expected
        {
            out.push(Violation::new(
                path,
                format!("expected the constant {}", compact(expected)),
            ));
        }

        if let Some(Value::Array(allowed)) = schema.get("enum")
            && !allowed.contains(instance)
        {
            let rendered: Vec<String> = allowed.iter().map(compact).collect();
            out.push(Violation::new(
                path,
                format!(
                    "{} is not one of the allowed values: {}",
                    compact(instance),
                    rendered.join(", ")
                ),
            ));
        }

        self.check_number(schema, instance, path, out);
        self.check_string(schema, instance, path, out);
        self.check_object(schema, instance, path, out);
        self.check_array(schema, instance, path, out);
        self.check_combinators(schema, instance, path, out);
    }

    fn check_number(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let Some(value) = instance.as_f64() else {
            return;
        };
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
            && value < minimum
        {
            out.push(Violation::new(
                path,
                format!("{} is below the minimum {minimum}", compact(instance)),
            ));
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
            && value > maximum
        {
            out.push(Violation::new(
                path,
                format!("{} is above the maximum {maximum}", compact(instance)),
            ));
        }
    }

    fn check_string(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let Some(text) = instance.as_str() else {
            return;
        };
        if let Some(limit) = schema.get("maxLength").and_then(Value::as_u64)
            && text.chars().count() as u64 > limit
        {
            out.push(Violation::new(
                path,
                format!("is longer than the {limit}-character maximum"),
            ));
        }
        if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
            && let Some(compiled) = self.patterns.get(pattern)
            && !compiled.matches(text)
        {
            out.push(Violation::new(
                path,
                format!(
                    "{} does not match the required `{pattern}`",
                    compact(instance)
                ),
            ));
        }
    }

    fn check_object(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let Some(object) = instance.as_object() else {
            return;
        };

        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    out.push(Violation::new(
                        path,
                        format!("is missing the required property `{name}`"),
                    ));
                }
            }
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (name, sub_schema) in properties {
                if let Some(value) = object.get(name) {
                    self.check(sub_schema, value, &child(path, name), out);
                }
            }
        }

        if let Some(names) = schema.get("propertyNames") {
            for name in object.keys() {
                self.check(names, &Value::String(name.clone()), &child(path, name), out);
            }
        }

        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                for name in object.keys() {
                    if properties.is_none_or(|known| !known.contains_key(name)) {
                        out.push(Violation::new(
                            path,
                            format!("has an unexpected property `{name}`"),
                        ));
                    }
                }
            }
            Some(sub_schema @ Value::Object(_)) => {
                for (name, value) in object {
                    if properties.is_none_or(|known| !known.contains_key(name)) {
                        self.check(sub_schema, value, &child(path, name), out);
                    }
                }
            }
            // `true` is the default and asserts nothing; `compile` rejected every
            // other shape.
            _ => {}
        }
    }

    fn check_array(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let (Some(items), Some(array)) = (schema.get("items"), instance.as_array()) else {
            return;
        };
        for (index, element) in array.iter().enumerate() {
            self.check(items, element, &child(path, &index.to_string()), out);
        }
    }

    fn check_combinators(
        &self,
        schema: &Map<String, Value>,
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        if let Some(Value::Array(branches)) = schema.get("oneOf") {
            let matched = branches
                .iter()
                .filter(|branch| self.matches(branch, instance))
                .count();
            if matched == 1 {
                return;
            }
            if matched > 1 {
                out.push(Violation::new(
                    path,
                    format!("matches {matched} of the schema's shapes; exactly one is required"),
                ));
                return;
            }
            self.explain_one_of(branches, instance, path, out);
        }

        if let Some(Value::Array(branches)) = schema.get("anyOf")
            && !branches.iter().any(|branch| self.matches(branch, instance))
        {
            out.push(Violation::new(
                path,
                format!("matches none of the {} allowed shapes", branches.len()),
            ));
            self.explain_any_of(branches, instance, path, out);
        }
    }

    /// Report a failed `oneOf` — the event-type dispatch — as helpfully as the
    /// instance allows, and **no more**:
    ///
    /// - the instance's `event` names a branch → report that branch's own errors,
    ///   which is what is actually wrong with the line;
    /// - it names something the schema has no branch for → say exactly that, naming
    ///   the tag. Reporting some *other* branch's errors here (the one that happened
    ///   to complain least, say) would tell a reader their `teleported` event is a
    ///   malformed `members_snapshot` — untrue, and it sends them somewhere useless;
    /// - it carries no usable `event` at all → the plain root verdict.
    fn explain_one_of(
        &self,
        branches: &[Value],
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let tag = instance.get("event").and_then(Value::as_str);

        if let Some(tag) = tag
            && let Some((_, name)) = self.branches.iter().find(|(known, _)| known == tag)
        {
            let reference = format!("{DEFS_PREFIX}{name}");
            if let Some(branch) = branches
                .iter()
                .find(|branch| branch.get("$ref").and_then(Value::as_str) == Some(&reference))
            {
                let before = out.len();
                self.check(branch, instance, path, out);
                if out.len() > before {
                    return;
                }
            }
        }

        if let Some(tag) = tag
            && !self.branches.is_empty()
        {
            out.push(Violation::new(
                path,
                format!(
                    "{} is not one of the {} event types this schema defines",
                    compact(&Value::String(tag.to_string())),
                    self.branches.len()
                ),
            ));
            return;
        }

        out.push(Violation::new(
            path,
            format!(
                "matches none of the {} shapes the schema defines",
                branches.len()
            ),
        ));
    }

    /// Detail for a failed `anyOf` (this document uses one: `shutdown` is `null` or
    /// an object). The summary above already states the verdict; this adds the
    /// errors of the alternative that actually *engaged* with the value — one whose
    /// complaints are all about the value's contents rather than its type — so a bad
    /// field inside a `shutdown` object is reported as that field rather than as
    /// "expected null". When no alternative engaged, the summary stands alone rather
    /// than being padded with a guess.
    fn explain_any_of(
        &self,
        branches: &[Value],
        instance: &Value,
        path: &str,
        out: &mut Vec<Violation>,
    ) {
        let mut best: Option<Vec<Violation>> = None;
        for branch in branches {
            let mut found = Vec::new();
            self.check(branch, instance, path, &mut found);
            if found.is_empty() || found.iter().any(|violation| violation.path == path) {
                continue;
            }
            if best.as_ref().is_none_or(|kept| found.len() < kept.len()) {
                best = Some(found);
            }
        }
        if let Some(mut best) = best {
            out.append(&mut best);
        }
    }

    /// Whether `instance` satisfies `schema` outright — the combinator primitive.
    fn matches(&self, schema: &Value, instance: &Value) -> bool {
        let mut found = Vec::new();
        self.check(schema, instance, "", &mut found);
        found.is_empty()
    }

    /// Resolve a local `#/$defs/<name>` reference against the document.
    fn resolve(&self, reference: &str) -> Option<&Value> {
        let name = reference.strip_prefix(DEFS_PREFIX)?;
        self.document.get("$defs")?.get(name)
    }
}

/// The `event` tag each `oneOf` branch pins, paired with that branch's `$defs`
/// name — derived from the document, never from a hand-written table. Empty when
/// the document is not the shape this derivation understands (a `oneOf` entry that
/// is not a local `$ref`, a branch with no `event` const, or two branches claiming
/// one tag, which would make "the branch this line must match" ill-defined), in
/// which case a failing line simply reports the root verdict.
fn branch_tags(document: &Value) -> Vec<(String, String)> {
    let (Some(defs), Some(one_of)) = (
        document.get("$defs"),
        document.get("oneOf").and_then(Value::as_array),
    ) else {
        return Vec::new();
    };
    let mut tags: Vec<(String, String)> = Vec::with_capacity(one_of.len());
    for branch in one_of {
        let Some(name) = branch
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix(DEFS_PREFIX))
        else {
            return Vec::new();
        };
        let Some(tag) = defs
            .get(name)
            .and_then(|def| def.pointer("/properties/event/const"))
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        if tags.iter().any(|(known, _)| known == tag) {
            return Vec::new();
        }
        tags.push((tag.to_string(), name.to_string()));
    }
    tags
}

/// Walk every schema node reachable from `node`, refusing anything this module does
/// not implement and compiling every `pattern` it will need. This is the whole of
/// the fail-closed guarantee (see the module docs).
fn walk_supported(
    node: &Value,
    document: &Value,
    patterns: &mut HashMap<String, Anchored>,
) -> Result<(), String> {
    let Some(schema) = node.as_object() else {
        return Err(format!(
            "the embedded schema contains a non-object schema node: {}",
            compact(node)
        ));
    };

    for (keyword, value) in schema {
        if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
            return Err(format!(
                "the embedded schema uses `{keyword}`, which this build's checker does not \
                 implement"
            ));
        }
        match keyword.as_str() {
            "$ref" => {
                let reference = value.as_str().unwrap_or_default();
                let target = reference
                    .strip_prefix(DEFS_PREFIX)
                    .and_then(|name| document.get("$defs").and_then(|defs| defs.get(name)));
                if target.is_none() {
                    return Err(format!(
                        "the embedded schema references `{reference}`, which is not a local \
                         `{DEFS_PREFIX}…` definition this checker can resolve"
                    ));
                }
                // The target is walked in its own right through `$defs` below, so it
                // is not re-walked here — which also makes a recursive reference
                // harmless.
            }
            "type" => {
                let names: Vec<&str> = match value {
                    Value::String(name) => vec![name.as_str()],
                    Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
                    other => {
                        return Err(format!(
                            "the embedded schema has a malformed `type`: {}",
                            compact(other)
                        ));
                    }
                };
                if names.is_empty() {
                    return Err("the embedded schema has an empty `type`".to_string());
                }
                for name in names {
                    if !SUPPORTED_TYPES.contains(&name) {
                        return Err(format!(
                            "the embedded schema uses the type `{name}`, which this build's \
                             checker does not implement"
                        ));
                    }
                }
            }
            "pattern" => {
                let raw = value.as_str().unwrap_or_default();
                if !patterns.contains_key(raw) {
                    let compiled = Anchored::compile(raw)?;
                    patterns.insert(raw.to_string(), compiled);
                }
            }
            "properties" | "$defs" => {
                let Some(members) = value.as_object() else {
                    return Err(format!("the embedded schema has a malformed `{keyword}`"));
                };
                for member in members.values() {
                    walk_supported(member, document, patterns)?;
                }
            }
            "oneOf" | "anyOf" => {
                let Some(branches) = value.as_array() else {
                    return Err(format!("the embedded schema has a malformed `{keyword}`"));
                };
                for branch in branches {
                    walk_supported(branch, document, patterns)?;
                }
            }
            "items" | "propertyNames" => walk_supported(value, document, patterns)?,
            "additionalProperties" => match value {
                Value::Bool(_) => {}
                Value::Object(_) => walk_supported(value, document, patterns)?,
                other => {
                    return Err(format!(
                        "the embedded schema has an `additionalProperties` this checker does not \
                         implement: {}",
                        compact(other)
                    ));
                }
            },
            // `const`/`enum`/`required`/`minimum`/`maximum`/`maxLength` carry data,
            // not sub-schemas; the annotations carry prose.
            _ => {}
        }
    }
    Ok(())
}

fn type_matches(expected: &Value, instance: &Value) -> bool {
    match expected {
        Value::String(name) => one_type_matches(name, instance),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| one_type_matches(name, instance)),
        _ => false,
    }
}

fn one_type_matches(name: &str, instance: &Value) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        // JSON Schema's `integer` is about the *value*, not the notation: `1.0` is
        // an integer. `compile` rejects any other type name, so this match is total
        // for every document that gets this far.
        "integer" => match instance {
            Value::Number(number) => {
                number.is_i64()
                    || number.is_u64()
                    || number.as_f64().is_some_and(|value| value.fract() == 0.0)
            }
            _ => false,
        },
        _ => false,
    }
}

fn render_type(expected: &Value) -> String {
    match expected {
        Value::String(name) => format!("\"{name}\""),
        Value::Array(names) => {
            let rendered: Vec<String> = names
                .iter()
                .filter_map(Value::as_str)
                .map(|name| format!("\"{name}\""))
                .collect();
            rendered.join(" or ")
        }
        other => compact(other),
    }
}

fn type_name(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A value as compact JSON, for a message. The result is untrusted data, so every
/// message is bounded and sanitized on the way out ([`Violation::describe`]).
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unrenderable>".to_string())
}

fn child(path: &str, name: &str) -> String {
    // JSON Pointer escaping (RFC 6901): `~` then `/`, in that order.
    format!("{path}/{}", name.replace('~', "~0").replace('/', "~1"))
}

fn internal(message: String) -> RunnerError {
    RunnerError::new(exit::INTERNAL, message)
}

impl Violation {
    fn new(path: &str, message: String) -> Self {
        Self {
            path: path.to_string(),
            message,
        }
    }

    /// `<where>: <what>` — the JSON Pointer into the offending event, or
    /// [`WHOLE_EVENT`]. The message embeds values from an untrusted file, so the
    /// whole rendered string crosses the shared terminal barrier
    /// ([`text::terminal_safe_bounded`]) like every other operator line this binary
    /// prints (K-091).
    fn describe(&self) -> String {
        let where_ = if self.path.is_empty() {
            WHOLE_EVENT
        } else {
            &self.path
        };
        text::terminal_safe_bounded(&format!("{where_}: {}", self.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker() -> SchemaChecker {
        SchemaChecker::compile().expect("the embedded schema compiles")
    }

    fn value(raw: &str) -> Value {
        serde_json::from_str(raw).expect("the fixture line is valid JSON")
    }

    const VALID_RUNNER_EXIT: &str = r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":0}"#;

    /// The fail-closed guarantee, stated as a test: the document this binary ships
    /// is fully covered by the keyword subset implemented here. If a future schema
    /// edit reaches for a keyword this module does not implement, this fails —
    /// which is the point, since the alternative is a checker that silently stops
    /// checking part of the contract.
    #[test]
    fn the_embedded_schema_compiles() {
        let checker = checker();
        assert!(
            !checker.patterns.is_empty(),
            "the document's `pattern` keywords are compiled up front"
        );
        assert_eq!(
            checker.branches.len(),
            checker
                .document
                .get("oneOf")
                .and_then(Value::as_array)
                .expect("the document is a oneOf over the event types")
                .len(),
            "every oneOf branch contributes exactly one event tag"
        );
    }

    /// An unimplemented keyword is refused rather than ignored — proven against a
    /// synthetic document, since the shipped one (deliberately) has none.
    #[test]
    fn an_unimplemented_keyword_is_refused_not_ignored() {
        let mut patterns = HashMap::new();
        let exotic = value(r#"{"type":"object","dependentRequired":{"a":["b"]}}"#);
        let err = walk_supported(&exotic, &exotic, &mut patterns)
            .expect_err("an unimplemented keyword must not be silently skipped");
        assert!(
            err.contains("dependentRequired"),
            "the refusal names the keyword: {err}"
        );

        let exotic = value(r#"{"type":"object","properties":{"a":{"type":"symbol"}}}"#);
        let err = walk_supported(&exotic, &exotic, &mut patterns)
            .expect_err("an unimplemented type must not be silently skipped");
        assert!(err.contains("symbol"), "the refusal names the type: {err}");

        let exotic = value(r#"{"type":"string","pattern":"[0-9]+"}"#);
        let err = walk_supported(&exotic, &exotic, &mut patterns)
            .expect_err("an uncompilable pattern must not be silently skipped");
        assert!(err.contains("anchored"), "the refusal explains why: {err}");

        let exotic = value(r#"{"$ref":"https://example.com/other.json"}"#);
        let err = walk_supported(&exotic, &exotic, &mut patterns)
            .expect_err("a remote reference must not be silently skipped");
        assert!(err.contains("local"), "the refusal explains why: {err}");
    }

    /// A conforming event has nothing said about it.
    #[test]
    fn a_conforming_event_reports_no_violations() {
        assert!(checker().violations(&value(VALID_RUNNER_EXIT)).is_empty());
        assert!(checker().conforms(&value(VALID_RUNNER_EXIT)));
    }

    /// A line whose `event` names a known type reports the *field* that is wrong,
    /// not the document-level "matches none of the shapes" — the whole reason the
    /// branch step exists.
    #[test]
    fn a_wrong_field_is_reported_against_the_named_branch() {
        let violations = checker().violations(&value(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":"seven","source":"child_exit","child_code":null}"#,
        ));
        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with("/code:") && violation.contains("integer")),
            "the report points at the offending field and says what was wrong: {violations:?}"
        );
    }

    /// Each assertion this module implements is exercised against the real document
    /// through the event shape that uses it.
    #[test]
    fn every_implemented_assertion_catches_its_own_violation() {
        let checker = checker();
        let cases: [(&str, &str); 7] = [
            // `required`
            (
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","source":"child_exit","child_code":null}"#,
                "required property `code`",
            ),
            // `additionalProperties: false`
            (
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":null,"extra":1}"#,
                "unexpected property `extra`",
            ),
            // `enum`
            (
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"teleported","child_code":null}"#,
                "not one of the allowed values",
            ),
            // `const` (`schema_version`)
            (
                r#"{"schema_version":2,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"child_exit","child_code":null}"#,
                "expected the constant 1",
            ),
            // `pattern` (the timestamp shape)
            (
                r#"{"schema_version":1,"time":"yesterday","event":"runner_exit","code":0,"source":"child_exit","child_code":null}"#,
                "does not match the required",
            ),
            // `minimum` (`spawn_failed.code` is a u8)
            (
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"spawn_failed","code":-1,"message":"x"}"#,
                "below the minimum",
            ),
            // `maximum` (same u8 bound)
            (
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"spawn_failed","code":9000,"message":"x"}"#,
                "above the maximum",
            ),
        ];
        for (line, expected) in cases {
            let violations = checker.violations(&value(line));
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.contains(expected)),
                "expected a violation mentioning {expected:?} for {line}: {violations:?}"
            );
        }
    }

    /// `maxLength` and `propertyNames` are only reachable through `run_started`'s
    /// label map, so they get their own case.
    #[test]
    fn label_keys_and_values_are_checked() {
        let checker = checker();
        let with_labels = |labels: &str| {
            format!(
                r#"{{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"run_started","run_id":"r1","labels":{labels},"root_pid":1,"mechanism":"job_object","abrupt_cleanup":"whole_tree","cwd":null,"command":{{"redacted":true,"argv":null,"argv_sha256":null,"hint":null}}}}"#
            )
        };
        assert!(
            checker
                .violations(&value(&with_labels(r#"{"batch":"42"}"#)))
                .is_empty(),
            "an ordinary label pair conforms"
        );
        assert!(
            !checker
                .violations(&value(&with_labels(r#"{"9bad":"42"}"#)))
                .is_empty(),
            "a label key that breaks `propertyNames` is a violation"
        );
        let oversized = "v".repeat(257);
        assert!(
            !checker
                .violations(&value(&with_labels(&format!(
                    r#"{{"batch":"{oversized}"}}"#
                ))))
                .is_empty(),
            "a label value past `maxLength` is a violation"
        );
    }

    /// An unknown `event` type is named as exactly that — and, crucially, is *not*
    /// explained as a malformed instance of whichever known event type happens to
    /// complain least, which would send a reader after the wrong problem.
    #[test]
    fn an_unknown_event_type_is_named_and_never_blamed_on_another_shape() {
        let violations = checker().violations(&value(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"teleported"}"#,
        ));
        assert_eq!(violations.len(), 1, "one honest verdict: {violations:?}");
        assert!(
            violations[0].starts_with(WHOLE_EVENT)
                && violations[0].contains("teleported")
                && violations[0].contains("not one of the"),
            "the verdict names the unknown tag: {violations:?}"
        );
        for known in ["members_snapshot", "runner_exit", "run_started"] {
            assert!(
                !violations[0].contains(known),
                "an unknown tag is never reported as a malformed `{known}`: {violations:?}"
            );
        }
    }

    /// A line with no `event` tag at all cannot be attributed to any branch, so the
    /// plain root verdict is what it gets.
    #[test]
    fn an_event_without_a_tag_reports_the_root_verdict() {
        let violations = checker().violations(&value(r#"{"schema_version":1}"#));
        assert_eq!(violations.len(), 1, "one honest verdict: {violations:?}");
        assert!(
            violations[0].starts_with(WHOLE_EVENT) && violations[0].contains("none of the"),
            "the root verdict is reported: {violations:?}"
        );
    }

    /// The `anyOf` case (`cleanup_finished.shutdown`, `null` or an object): a bad
    /// field inside the object is reported as that field, not as "expected null".
    #[test]
    fn an_any_of_failure_is_explained_by_the_alternative_that_engaged() {
        let violations = checker().violations(&value(
            r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"cleanup_finished","remaining":0,"remaining_pids":[],"soft_terminate":null,"read_error":false,"shutdown":{"soft_stop_scope":"whole_tree","soft_signal":"teleported","members_before":null,"members_after":null,"drained_within_grace":null,"escalated":null,"elapsed_ms":null}}"#,
        ));
        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with("/shutdown/soft_signal")),
            "the offending field inside the object is named: {violations:?}"
        );
        assert!(
            !violations
                .iter()
                .any(|violation| violation.contains("expected type \"null\"")),
            "the null alternative is not what a bad object field is blamed on: {violations:?}"
        );
    }

    /// JSON Schema's `integer` is about the value, not the notation — `1.0` is an
    /// integer, and a real engine agrees (the differential test in
    /// `tests/events.rs` is what keeps this honest).
    #[test]
    fn an_integral_float_counts_as_an_integer() {
        let checker = checker();
        assert!(
            checker.conforms(&value(
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":1.0,"source":"child_exit","child_code":null}"#
            )),
            "1.0 is an integer value"
        );
        assert!(
            !checker.conforms(&value(
                r#"{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":1.5,"source":"child_exit","child_code":null}"#
            )),
            "1.5 is not"
        );
    }

    /// Violations embed values from an untrusted file, so they are sanitized and
    /// bounded before they can reach a terminal.
    #[test]
    fn violations_are_terminal_safe_and_bounded() {
        let forged = value(&format!(
            r#"{{"schema_version":1,"time":"2026-07-22T09:00:00.000Z","event":"runner_exit","code":0,"source":"forged\u001b[31m{}","child_code":null}}"#,
            "x".repeat(600)
        ));
        let violations = checker().violations(&forged);
        assert!(!violations.is_empty(), "an unknown source cannot be valid");
        for violation in &violations {
            assert!(
                violation.chars().all(|character| !character.is_control()),
                "no terminal control survives into a violation: {violation:?}"
            );
            assert!(
                violation.chars().count() <= text::TERMINAL_FIELD_MAX_CHARS + 3,
                "a violation is bounded like every other rendered field: {} chars",
                violation.chars().count()
            );
        }
    }

    /// A property name with a JSON Pointer metacharacter is escaped per RFC 6901,
    /// so a report's path stays a path.
    #[test]
    fn pointer_paths_escape_their_metacharacters() {
        assert_eq!(child("", "code"), "/code");
        assert_eq!(child("/command", "argv"), "/command/argv");
        assert_eq!(child("", "a/b"), "/a~1b");
        assert_eq!(child("", "a~b"), "/a~0b");
    }
}
