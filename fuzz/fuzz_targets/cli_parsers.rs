//! Fuzz the defensive CLI value parsers (`src/cli/parse.rs`), each fed the same raw
//! text independently — they never interact, and each is expected to reject
//! anything outside its own strict grammar rather than panic or silently
//! reinterpret it (see each function's own doc comment for its grammar:
//! `--timeout`/`--idle-timeout`/`--grace` duration, `--require-exit-code-band`, `--env`,
//! `--run-id`, `--max-memory` size, `--max-processes` count, `--cpu-quota` core
//! fraction, operator labels, and environment-file bytes). Environment parsing
//! also carries a secret-safety invariant: diagnostics must never echo values.
#![no_main]

use libfuzzer_sys::fuzz_target;
use processkit_cli::cli::{
    parse_cpu_quota, parse_duration, parse_env_kv, parse_exit_code_band, parse_max_processes,
    parse_positive_duration, parse_run_id, parse_size,
};
use processkit_cli::{labels, run::parse_env_file_contents};

fuzz_target!(|data: &[u8]| {
    let _ = parse_env_file_contents(data);

    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_duration(text);
        let _ = parse_positive_duration(text);
        let _ = parse_exit_code_band(text);
        let _ = parse_env_kv(text);
        let _ = parse_run_id(text);
        let _ = parse_size(text);
        let _ = parse_max_processes(text);
        let _ = parse_cpu_quota(text);
        let _ = labels::parse(text);

        let secret_input = format!("BAD KEY=FUZZ_SECRET_VALUE_{text}");
        if let Err(error) = parse_env_file_contents(secret_input.as_bytes()) {
            assert!(!error.contains("FUZZ_SECRET_VALUE_"));
        }
    }
});
