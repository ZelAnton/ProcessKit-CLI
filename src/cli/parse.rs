//! The hand-written value parsers the subcommand argument structs share:
//! durations, byte sizes, process counts, CPU quotas, exit-code bands,
//! `KEY=VALUE` environment entries, bare environment keys, and run ids.
//!
//! Each is deliberately strict and is the single source of truth for its flag's
//! *form*, so a malformed value fails loudly at parse time as the documented
//! `USAGE` (100) exit instead of reaching the runner as a mid-run failure. Every
//! parser here is `#[doc(hidden)] pub` — not exported clap surface, but reachable
//! as `processkit_cli::cli::parse_*` through [`super`]'s re-export so
//! `fuzz/fuzz_targets/cli_parsers.rs` can drive it with arbitrary text.

use std::time::Duration;

/// Parse a human duration for `--grace` (a zero-length duration is legal there —
/// "no pause" between soft stop and hard kill — so this parser accepts `0`; see
/// [`parse_positive_duration`] for the deadlines and `--snapshot-interval`, where
/// `0` is rejected as a degenerate, almost-certainly-a-typo value).
///
/// Grammar: a base-10, non-negative integer with an optional unit suffix — `ms`,
/// `s` (the default when the suffix is omitted), `m`, or `h`. Examples: `30`
/// (= 30 seconds), `0`, `500ms`, `5s`, `2m`, `1h`. Deliberately strict — a sign, a
/// fraction, surrounding whitespace, or an unknown unit is rejected rather than
/// silently reinterpreted, so a typo fails loudly at parse time instead of arming
/// a surprising deadline. The value is capped only by `u64` milliseconds; an
/// overflow is reported, not wrapped.
///
/// Returns the message that clap renders on failure (which the binary maps to the
/// `USAGE` exit code); on success it hands `run` a ready `Duration`.
///
/// `pub`/`#[doc(hidden)]` (not exported clap surface) so the CLI-parsers fuzz
/// target can drive it directly with arbitrary text (`fuzz/fuzz_targets/cli_parsers.rs`, T-186).
#[doc(hidden)]
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    if raw.is_empty() {
        return Err("empty duration; expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`".to_string());
    }

    // Split the leading digit run from the unit suffix. A value that does not
    // start with a digit (a sign, a bare unit, letters) leaves `number` empty.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "duration `{raw}` must start with a non-negative number; \
             expected e.g. `30`, `500ms`, `5s`, `2m`, or `1h`"
        ));
    }

    let value: u64 = number
        .parse()
        .map_err(|_| format!("duration `{raw}` is out of range for a 64-bit millisecond count"))?;

    let millis = match unit {
        "" | "s" => value.checked_mul(1_000),
        "ms" => Some(value),
        "m" => value.checked_mul(60_000),
        "h" => value.checked_mul(3_600_000),
        other => {
            return Err(format!(
                "duration `{raw}` has an unknown unit `{other}`; use ms, s, m, or h"
            ));
        }
    };

    let millis = millis
        .ok_or_else(|| format!("duration `{raw}` is too large to represent in milliseconds"))?;
    Ok(Duration::from_millis(millis))
}

/// Parse a human duration for the flags where `0` is degenerate rather than
/// meaningful — the deadlines (`--timeout`, `--idle-timeout`, `wait --timeout`) and
/// the `run --snapshot-interval` cadence: same grammar as [`parse_duration`], but a
/// total of `0` (in any unit — `0`, `0ms`, `0s`, `0m`, `0h`) is additionally
/// rejected at parse time, mirroring the "degenerate cap" treatment
/// `parse_size`/`parse_max_processes`/`parse_cpu_quota` give `0` for
/// `--max-memory`/`--max-processes`/`--cpu-quota`.
///
/// The degeneracy differs by flag, which is why the rejection message names the
/// *shape* of the mistake rather than one flag's consequence:
///
/// - a `0` **deadline** (`--timeout`, `--idle-timeout`, `wait --timeout`) is already
///   elapsed on the very first poll — the child is torn down immediately after spawn
///   (for `--idle-timeout`, unconditionally, since `remaining` saturates to zero);
/// - a `0` **interval** (`--snapshot-interval`) asks for no spacing at all between
///   samples, i.e. a snapshot storm bounded only by how fast the container can be
///   read — not a fast cadence but the absence of one.
///
/// Neither is a useful setting in its own right, and both are indistinguishable in
/// practice from an operator typo (`--timeout 0` instead of, say, `--timeout 30`).
/// Per this module's own philosophy ("a typo fails loudly at parse time instead of
/// arming a surprising deadline", see [`parse_duration`]), that typo is rejected here
/// rather than silently armed.
///
/// `--grace 0` stays legal and keeps using [`parse_duration`] directly: there `0`
/// is meaningful ("no pause" between the soft stop and the hard kill), not
/// degenerate, so it is not routed through this stricter parser.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_positive_duration(raw: &str) -> Result<Duration, String> {
    let duration = parse_duration(raw)?;
    if duration.is_zero() {
        return Err(format!(
            "duration `{raw}` must be greater than 0; zero is almost certainly a \
             typo rather than a setting — as a deadline it expires on the first \
             check (tearing the child down immediately after spawn), and as an \
             interval it asks for no spacing between samples at all. Omit the flag \
             to leave this behavior unarmed"
        ));
    }
    Ok(duration)
}

/// Parse a `--max-memory` byte size.
///
/// Grammar: a base-10, positive integer with an optional **binary** unit suffix —
/// `b` (bytes, the default when omitted), `k` (KiB, ×1024), `m` (MiB, ×1024²), or
/// `g` (GiB, ×1024³); the suffix is case-insensitive (`512K` == `512k`). Examples:
/// `1048576`, `512k`, `256m`, `2g`. Deliberately strict — a sign, a fraction,
/// surrounding whitespace, an unknown unit, or a total of `0` bytes is rejected
/// rather than silently reinterpreted, so a typo fails loudly at parse time
/// (mapped to the `USAGE` exit) instead of arming a surprising or degenerate cap.
///
/// **This parser is the single source of truth for the flag's *form*** — it
/// rejects the same nonsense (`0`, non-numeric) that ProcessKit's own
/// `ResourceLimits` validation would (`LimitReason::Invalid`), so a malformed
/// `--max-memory` surfaces as the documented `USAGE` (100) like any other bad
/// flag, never reaching `ProcessGroup::with_options` (where it would otherwise be
/// a mid-run `limit_hit`). The value is capped only by `u64` bytes; an overflow is
/// reported, not wrapped.
///
/// `pub`/`#[doc(hidden)]` (not exported clap surface) so the CLI-parsers fuzz
/// target can drive it directly (`fuzz/fuzz_targets/cli_parsers.rs`, T-186).
#[doc(hidden)]
pub fn parse_size(raw: &str) -> Result<u64, String> {
    if raw.is_empty() {
        return Err("empty size; expected e.g. `1048576`, `512k`, `256m`, or `2g`".to_string());
    }

    // Split the leading digit run from the unit suffix, exactly like
    // `parse_duration`: a value that does not start with a digit (a sign, a bare
    // unit, letters) leaves `number` empty.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (number, unit) = raw.split_at(split);
    if number.is_empty() {
        return Err(format!(
            "size `{raw}` must start with a non-negative number; \
             expected e.g. `1048576`, `512k`, `256m`, or `2g`"
        ));
    }

    let value: u64 = number
        .parse()
        .map_err(|_| format!("size `{raw}` is out of range for a 64-bit byte count"))?;

    // Case-insensitive so `512K` and `512k` are the same; binary (1024-based)
    // units, documented above so the wire meaning is never ambiguous.
    let bytes = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => Some(value),
        "k" => value.checked_mul(1024),
        "m" => value.checked_mul(1024 * 1024),
        "g" => value.checked_mul(1024 * 1024 * 1024),
        other => {
            return Err(format!(
                "size `{raw}` has an unknown unit `{other}`; use b, k, m, or g"
            ));
        }
    };

    let bytes = bytes.ok_or_else(|| format!("size `{raw}` is too large to represent in bytes"))?;
    if bytes == 0 {
        // `0` bytes is a degenerate cap ProcessKit itself rejects
        // (`LimitReason::Invalid`); catch it here so it is a form error, not a
        // mid-run `limit_hit`.
        return Err(format!("size `{raw}` must be greater than 0 bytes"));
    }
    Ok(bytes)
}

/// Parse a `--max-processes` count: a base-10 integer strictly greater than `0`
/// and within `u32`. Deliberately strict — a sign, a fraction, whitespace, `0`, or
/// an out-of-range value fails loudly at parse time (mapped to the `USAGE` exit)
/// rather than reaching `ProcessGroup::with_options` as a mid-run `limit_hit`.
/// Like [`parse_size`], this parser is the single source of truth for the flag's
/// form, mirroring ProcessKit's `max_processes(0)` → `LimitReason::Invalid`
/// rejection at the CLI boundary instead of duplicating it downstream.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_max_processes(raw: &str) -> Result<u32, String> {
    let n: u32 = raw.parse().map_err(|_| {
        format!("`--max-processes` value `{raw}` must be an integer in 1..=4294967295")
    })?;
    if n == 0 {
        return Err("`--max-processes` must be greater than 0".to_string());
    }
    Ok(n)
}

/// Parse a `--cpu-quota` value: a finite `f64` strictly greater than `0` (a
/// fraction of a single core — `0.5` is half a core, `2` is two cores). `0`,
/// negatives, `NaN`, and the infinities are rejected at parse time (mapped to the
/// `USAGE` exit), mirroring ProcessKit's own `cpu_quota` validity rule
/// (`LimitReason::Invalid`) at the CLI boundary so a nonsense quota is a loud form
/// error, never a mid-run `limit_hit`. Like [`parse_size`]/[`parse_max_processes`]
/// this parser is the single source of truth for the flag's form.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_cpu_quota(raw: &str) -> Result<f64, String> {
    let cores: f64 = raw.parse().map_err(|_| {
        format!("`--cpu-quota` value `{raw}` must be a number, e.g. `0.5`, `1`, `2`")
    })?;
    if !(cores.is_finite() && cores > 0.0) {
        return Err(format!(
            "`--cpu-quota` value `{raw}` must be a finite value greater than 0"
        ));
    }
    Ok(cores)
}

/// Parse a `--require-exit-code-band` value: two `u8`s as `start-end` (e.g.
/// `100-119`). Deliberately strict — exactly one `-` separating two base-10
/// integers, with `start <= end` — so a typo fails loudly at parse time (mapped to
/// the `USAGE` exit) rather than being reinterpreted into a band the consumer did
/// not mean. Returns the message clap renders on failure; on success it hands the
/// probe a ready `(start, end)` pair to compare against the reserved band.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_exit_code_band(raw: &str) -> Result<(u8, u8), String> {
    let (start, end) = raw.split_once('-').ok_or_else(|| {
        format!("exit-code band `{raw}` must be two numbers as `start-end`, e.g. `100-119`")
    })?;
    let start: u8 = start
        .parse()
        .map_err(|_| format!("exit-code band `{raw}` has a non-`u8` start `{start}`"))?;
    let end: u8 = end
        .parse()
        .map_err(|_| format!("exit-code band `{raw}` has a non-`u8` end `{end}`"))?;
    if start > end {
        return Err(format!(
            "exit-code band `{raw}` is inverted: start {start} is above end {end}"
        ));
    }
    Ok((start, end))
}

/// The **one** rule for the form of a child environment variable *name*, shared by
/// every flag that names one: non-empty, and free of whitespace, control
/// characters, and `=`.
///
/// The `=` clause states a rule `--env` has always had implicitly — [`parse_env_kv`]
/// splits on the first `=`, so a KEY it produces can never contain one — and which
/// a flag taking a *bare* name must therefore check explicitly to stay held to the
/// same grammar rather than a laxer one.
///
/// Single source of truth in this module's own sense: `--env`'s `KEY=VALUE`
/// grammar ([`parse_env_kv`] — which every `--env-file` line also goes through,
/// see `run::parse_env_file_contents`) and `run --run-id-env <KEY>`
/// ([`parse_env_key`]) both call this instead of each re-deriving what a key may
/// look like, so the two can never drift into accepting different names.
///
/// `flag` is only ever the caller's own flag name, so the message points at what
/// the operator actually typed; the rule itself is identical for every caller. The
/// key is escaped rather than echoed raw — an invisible or terminal-reshaping
/// character in the *name* must not reshape the diagnostic — and no environment
/// **value** is ever part of a message here (see
/// `parse_env_kv_errors_never_repeat_the_value`).
fn validate_env_key(flag: &str, key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err(format!("`{flag}` KEY is empty"));
    }
    if key
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(format!(
            "`{flag}` KEY `{}` contains whitespace or a control character",
            key.escape_debug()
        ));
    }
    if key.contains('=') {
        // Unreachable from `parse_env_kv` (it split the key off at the first `=`),
        // but load-bearing for every caller that takes a bare name: `FOO=bar` is a
        // caller reaching for the `--env` spelling, not a variable named `FOO=bar`.
        return Err(format!(
            "`{flag}` takes a bare KEY, not `KEY=VALUE`: `{}` contains `=`",
            key.escape_debug()
        ));
    }
    Ok(())
}

/// Parse a `--env` value: `KEY=VALUE`, split on the **first** `=` (so a value
/// containing `=` is preserved verbatim rather than truncated). A missing `=` or
/// an empty `KEY`, or one containing whitespace/control characters, is rejected
/// at parse time — mapped to the `USAGE` exit — rather than silently accepted as
/// a malformed environment variable name. The `KEY` half is held to the shared
/// [`validate_env_key`] rule.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_env_kv(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw.split_once('=').ok_or_else(|| {
        "`--env` value must be `KEY=VALUE` (a literal `=` separating name and value)".to_string()
    })?;
    validate_env_key("--env", key)?;
    Ok((key.to_string(), value.to_string()))
}

/// Parse a `run --run-id-env` destination: a bare environment variable **name**,
/// with no `=` and no value — the run id is the value, and it comes from the
/// runner, not the command line.
///
/// The name is held to exactly the same form as an `--env` KEY, through the shared
/// [`validate_env_key`] rather than a second, similar-looking check: a caller that
/// can write `--env FOO=1` can write `--run-id-env FOO`, and neither flag accepts
/// a name the other would reject.
///
/// `pub`/`#[doc(hidden)]` — see [`parse_duration`]'s note on why (fuzz target).
#[doc(hidden)]
pub fn parse_env_key(raw: &str) -> Result<String, String> {
    validate_env_key("--run-id-env", raw)?;
    Ok(raw.to_string())
}

/// Parse an explicit `--run-id`. Run ids become registry keys and appear in human
/// diagnostics, so keep them non-empty, bounded, and free of characters that can
/// split or invisibly reshape terminal output. Ordinary Unicode remains valid.
#[doc(hidden)]
pub fn parse_run_id(raw: &str) -> Result<String, String> {
    const MAX_CHARS: usize = 256;

    if raw.is_empty() {
        return Err("`--run-id` cannot be empty".to_string());
    }
    if raw.chars().count() > MAX_CHARS {
        return Err(format!(
            "`--run-id` cannot exceed {MAX_CHARS} Unicode characters"
        ));
    }
    if crate::text::contains_terminal_unsafe(raw) {
        return Err(
            "`--run-id` cannot contain terminal control or formatting characters".to_string(),
        );
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_accepts_the_documented_grammar() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        // `0` is legal for `parse_duration` (used directly by `--grace`, where a
        // zero-length pause is a meaningful "no pause" setting, not degenerate);
        // see `parse_positive_duration_rejects_zero_in_every_unit` for the
        // stricter parser used by the deadlines and `--snapshot-interval`.
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_duration_rejects_malformed_values() {
        // Empty, non-numeric, signed, fractional, unknown unit, and whitespace all
        // fail loudly rather than being silently reinterpreted.
        for bad in ["", "abc", "-5", "5x", "1.5s", "s", "5 s", " 5s", "ms"] {
            assert!(
                parse_duration(bad).is_err(),
                "expected `{bad}` to be rejected as a duration"
            );
        }
    }

    #[test]
    fn parse_duration_reports_overflow_instead_of_wrapping() {
        // A value that would overflow the millisecond count is an error, never a
        // wrapped-around tiny duration.
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(parse_duration(&format!("{}h", u64::MAX)).is_err());
    }

    #[test]
    fn parse_positive_duration_accepts_the_same_grammar_minus_zero() {
        // Everything `parse_duration` accepts, `parse_positive_duration` accepts
        // too, as long as it is not a total of zero.
        assert_eq!(
            parse_positive_duration("30").unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_positive_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_positive_duration("5s").unwrap(),
            Duration::from_secs(5)
        );
        assert_eq!(
            parse_positive_duration("2m").unwrap(),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_positive_duration("1h").unwrap(),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn parse_positive_duration_rejects_zero_in_every_unit() {
        // Used by `--timeout`/`--idle-timeout`/`wait --timeout` (a zero deadline
        // arms instantly) and by `--snapshot-interval` (a zero interval is a
        // snapshot storm, not a cadence). Both are almost certainly a typo and are
        // rejected as a form error (`USAGE`, 100) rather than silently armed —
        // unlike `--grace`, which keeps accepting `0` through `parse_duration`.
        for zero in ["0", "0ms", "0s", "0m", "0h"] {
            assert!(
                parse_positive_duration(zero).is_err(),
                "expected `{zero}` to be rejected as a degenerate zero duration"
            );
        }
    }

    #[test]
    fn parse_positive_duration_rejects_malformed_values() {
        // Same malformed-input rejections as `parse_duration`, since it is
        // delegated to first.
        for bad in ["", "abc", "-5", "5x", "1.5s", "s", "5 s", " 5s", "ms"] {
            assert!(
                parse_positive_duration(bad).is_err(),
                "expected `{bad}` to be rejected as a duration"
            );
        }
    }

    // Property-based tier (T-167). Placed in this same `#[cfg(test)]` module
    // rather than a new `tests/properties.rs`: `parse_duration` is `pub` +
    // `#[doc(hidden)]` (T-186, for the `cli_parsers` fuzz target), but that is
    // not a stable, exported clap surface — the module is still not public
    // API — so proptests stay in-crate rather than moving to an integration
    // test under `tests/`. These run under the library unit-test tier (a
    // plain `cargo test`, i.e. `--lib`).
    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            /// Unit equivalence across the documented grammar: `s` is 1000x `ms`,
            /// `m` is 60x `s`, `h` is 60x `m`, and a bare number defaults to `s` —
            /// for any value small enough that none of the multiplications
            /// overflow `u64` milliseconds.
            #[test]
            fn unit_equivalence_holds(value in 0u64..1_000_000) {
                let bare = parse_duration(&value.to_string()).unwrap();
                let secs = parse_duration(&format!("{value}s")).unwrap();
                let millis = parse_duration(&format!("{}ms", value * 1_000)).unwrap();
                let mins = parse_duration(&format!("{value}m")).unwrap();
                let mins_as_secs = parse_duration(&format!("{}s", value * 60)).unwrap();
                let hours = parse_duration(&format!("{value}h")).unwrap();
                let hours_as_mins = parse_duration(&format!("{}m", value * 60)).unwrap();

                prop_assert_eq!(bare, secs, "a bare number must default to seconds");
                prop_assert_eq!(secs, millis, "`Ns` must equal `(N*1000)ms`");
                prop_assert_eq!(mins, mins_as_secs, "`Nm` must equal `(N*60)s`");
                prop_assert_eq!(hours, hours_as_mins, "`Nh` must equal `(N*60)m`");
            }

            /// Any string that does not start with an ASCII digit is rejected: the
            /// grammar requires a leading digit run, so `raw.find` locating the
            /// first non-digit at index 0 always leaves `number` empty.
            #[test]
            fn non_digit_leading_input_is_rejected(raw in "[^0-9]{0,32}") {
                prop_assert!(parse_duration(&raw).is_err());
            }

            /// A digit run followed by any suffix outside the four documented
            /// units is rejected rather than silently reinterpreted.
            #[test]
            fn digits_with_unknown_unit_are_rejected(
                value in 0u64..1_000_000,
                unit in "[a-zA-Z]{1,8}",
            ) {
                prop_assume!(!matches!(unit.as_str(), "ms" | "s" | "m" | "h"));
                let raw = format!("{value}{unit}");
                prop_assert!(parse_duration(&raw).is_err());
            }

            /// No input — arbitrary, not just grammar-shaped — ever makes the
            /// parser panic; it always returns `Ok` or `Err`.
            #[test]
            fn never_panics_on_arbitrary_input(raw in ".{0,64}") {
                let _ = parse_duration(&raw);
            }
        }
    }

    #[test]
    fn parse_exit_code_band_accepts_and_rejects() {
        assert_eq!(parse_exit_code_band("100-119").unwrap(), (100, 119));
        assert_eq!(parse_exit_code_band("0-255").unwrap(), (0, 255));
        assert_eq!(parse_exit_code_band("110-110").unwrap(), (110, 110));
        // Missing separator, non-numeric, out-of-u8-range, and an inverted band all
        // fail loudly rather than being reinterpreted.
        for bad in ["100", "100+119", "a-119", "100-b", "100-999", "119-100"] {
            assert!(
                parse_exit_code_band(bad).is_err(),
                "expected `{bad}` to be rejected as an exit-code band"
            );
        }
    }

    #[test]
    fn parse_env_kv_splits_on_the_first_equals() {
        assert_eq!(
            parse_env_kv("FOO=bar").unwrap(),
            ("FOO".to_string(), "bar".to_string())
        );
        assert_eq!(
            parse_env_kv("FOO=").unwrap(),
            ("FOO".to_string(), String::new())
        );
        assert_eq!(
            parse_env_kv("FOO=a=b=c").unwrap(),
            ("FOO".to_string(), "a=b=c".to_string())
        );
    }

    #[test]
    fn parse_env_kv_rejects_a_missing_separator_or_invalid_key() {
        for bad in [
            "FOO",
            "",
            "=novalue",
            " SPACE=value",
            "TAB\t=value",
            "LINE\n=value",
            "NO BREAK\u{00a0}=value",
        ] {
            assert!(
                parse_env_kv(bad).is_err(),
                "expected `{bad}` to be rejected as a KEY=VALUE pair"
            );
        }
    }

    #[test]
    fn parse_env_key_accepts_exactly_the_names_parse_env_kv_does() {
        // Differential, not two independent lists: whatever `--env` accepts as the
        // KEY half, `--run-id-env` accepts as a whole value, and whatever `--env`
        // rejects there, `--run-id-env` rejects too. Both go through
        // `validate_env_key`, and this is what would fail if one of them ever grew
        // its own second rule.
        for key in [
            "FOO",
            "PROCESSKIT_RUN_ID",
            "lower_case",
            "x",
            "WITH.DOT",
            "é",
        ] {
            let from_pair = parse_env_kv(&format!("{key}=value"))
                .unwrap_or_else(|err| panic!("`--env {key}=value` must parse: {err}"))
                .0;
            let bare = parse_env_key(key)
                .unwrap_or_else(|err| panic!("`--run-id-env {key}` must parse: {err}"));
            assert_eq!(from_pair, bare);
        }

        for bad in ["", "BAD KEY", "TAB\tKEY", "LINE\nKEY", "NO\u{00a0}BREAK"] {
            assert!(
                parse_env_kv(&format!("{bad}=value")).is_err(),
                "expected `--env {bad}=value` to be rejected"
            );
            assert!(
                parse_env_key(bad).is_err(),
                "expected `--run-id-env {bad}` to be rejected"
            );
        }
    }

    #[test]
    fn parse_env_key_takes_a_bare_name_not_a_pair() {
        // The value is the run id the runner resolves, so a `KEY=VALUE` spelling is
        // a caller misunderstanding rather than a redundant-but-harmless form: `=`
        // is not a legal name character here, and it fails loudly at parse time.
        // This is the same grammar `--env` has always enforced implicitly by
        // splitting on the first `=` — never a name that reached the child.
        for pair in ["FOO=bar", "FOO=", "=bar", "FOO=a=b"] {
            let error = parse_env_key(pair).expect_err("a pair is not a bare name");
            assert!(
                error.contains("bare KEY") || error.contains("is empty"),
                "the message explains the shape mistake: {error:?}"
            );
        }
    }

    #[test]
    fn parse_env_key_errors_name_the_flag_and_escape_the_key() {
        // The message points at the flag the operator actually typed (the shared
        // validator is parameterized precisely so it can), and a key carrying a
        // control character is escaped rather than replayed into the terminal.
        let error = parse_env_key("BAD KEY").expect_err("whitespace is rejected");
        assert!(error.contains("--run-id-env"), "{error:?}");

        let error = parse_env_kv("BAD KEY=value").expect_err("whitespace is rejected");
        assert!(error.contains("--env"), "{error:?}");

        let error = parse_env_key("BEL\u{7}KEY").expect_err("a control character is rejected");
        assert!(
            !error.chars().any(char::is_control),
            "diagnostics stay on one terminal line: {error:?}"
        );
    }

    #[test]
    fn parse_env_kv_errors_never_repeat_the_value() {
        let secret = "do-not-print-this-secret";
        for bad in [
            format!("BAD KEY={secret}"),
            format!("={secret}"),
            secret.to_string(),
        ] {
            let error = parse_env_kv(&bad).expect_err("the malformed entry must fail");
            assert!(
                !error.contains(secret),
                "environment parse errors must not disclose values: {error:?}"
            );
            assert!(
                !error.chars().any(char::is_control),
                "environment parse errors stay on one terminal line: {error:?}"
            );
        }
    }

    #[test]
    fn run_id_parser_accepts_ordinary_unicode_at_the_boundary() {
        let boundary = "é".repeat(256);
        assert_eq!(parse_run_id("build-α").unwrap(), "build-α");
        assert_eq!(parse_run_id(&boundary).unwrap(), boundary);
    }

    #[test]
    fn parse_size_accepts_bytes_and_binary_units() {
        assert_eq!(parse_size("1").unwrap(), 1);
        assert_eq!(parse_size("1048576").unwrap(), 1_048_576);
        assert_eq!(parse_size("512b").unwrap(), 512);
        assert_eq!(parse_size("1k").unwrap(), 1024);
        assert_eq!(parse_size("2m").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("3g").unwrap(), 3 * 1024 * 1024 * 1024);
        // The unit is case-insensitive.
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
    }

    #[test]
    fn parse_size_rejects_malformed_or_degenerate_values() {
        // Empty, non-numeric, signed, fractional, whitespace, an unknown unit, and a
        // bare unit all fail loudly. `0` (in any unit) is a degenerate cap ProcessKit
        // itself rejects, so the parser catches it as a form error before with_options.
        for bad in [
            "", "abc", "-5", "1.5m", "5 m", " 5m", "5x", "m", "0", "0k", "0m", "0g",
        ] {
            assert!(
                parse_size(bad).is_err(),
                "expected `{bad}` to be rejected as a size"
            );
        }
    }

    #[test]
    fn parse_size_reports_overflow_instead_of_wrapping() {
        assert!(parse_size(&format!("{}g", u64::MAX)).is_err());
        assert!(parse_size("99999999999999999999").is_err());
    }

    #[test]
    fn parse_max_processes_requires_a_positive_integer() {
        assert_eq!(parse_max_processes("1").unwrap(), 1);
        assert_eq!(parse_max_processes("64").unwrap(), 64);
        // `0`, negatives, fractions, and non-numbers are rejected at parse time.
        for bad in ["0", "-1", "1.5", "abc", "", "99999999999"] {
            assert!(
                parse_max_processes(bad).is_err(),
                "expected `{bad}` to be rejected as a process count"
            );
        }
    }

    #[test]
    fn parse_cpu_quota_requires_a_finite_positive() {
        assert_eq!(parse_cpu_quota("0.5").unwrap(), 0.5);
        assert_eq!(parse_cpu_quota("1").unwrap(), 1.0);
        assert_eq!(parse_cpu_quota("2").unwrap(), 2.0);
        // `0`, negatives, NaN, the infinities, and non-numbers are rejected — the
        // same values ProcessKit's `cpu_quota` validity rule treats as `Invalid`,
        // caught here at the CLI boundary instead.
        for bad in [
            "0", "0.0", "-1", "-0.5", "nan", "NaN", "inf", "infinity", "abc", "",
        ] {
            assert!(
                parse_cpu_quota(bad).is_err(),
                "expected `{bad}` to be rejected as a CPU quota"
            );
        }
    }
}
