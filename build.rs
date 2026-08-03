//! Build script: generates shell completions and man pages from the *live* CLI
//! definition (`src/cli/`), so operators get tab-completion and `man`
//! documentation for `run`/`inspect`/`cancel`/`kill`/`list`/`prune`/`probe`
//! without this project hand-maintaining a second, driftable copy of the
//! surface.
//!
//! ## Why build-time generation, not a `generate-completions` subcommand
//!
//! `probe`'s surface tokens (`src/probe.rs::surface_tokens`) are derived from
//! the **runtime** `Cli::command()` tree via `Command::get_subcommands()` — every
//! subcommand in that tree, visible or `hide = true`, is part of what a
//! consumer's fail-closed `--require-surface` preflight can observe growing or
//! shrinking release to release. A visible `generate-completions` subcommand
//! would silently widen that surface; a hidden one would still need an explicit
//! carve-out and a test proving it never leaks into `probe --json`. Generating
//! here, in `build.rs`, sidesteps the question entirely: this file is compiled
//! and run by Cargo at build time, is never linked into the shipped binary, and
//! therefore never touches — and can never leak into — the runtime CLI tree
//! `probe` introspects. See `README.md`, "Shell completions and man pages", for
//! how the generated files are packaged and installed, and
//! `.github/workflows/release.yml` for how a release attaches them.
//!
//! ## One CLI definition, not two
//!
//! Loading `src/cli/mod.rs` as a module here (`#[path = "src/cli/mod.rs"] mod
//! cli;` — the same files the binary itself compiles, just reached from a
//! second compilation root) means this generator always reflects the real
//! parser; there is no second, hand-written copy of the flags to fall out of
//! sync. Rust treats any `#[path]`-included file as a `mod.rs`, so `cli`'s own
//! `mod run;`/`mod parse;`/… declarations resolve against `src/cli/` here
//! exactly as they do for the library — one `#[path]` line reaches the whole
//! directory, and a future submodule needs no change to this script. The
//! `#[cfg(test)]` module inside each of those files is compiled out here
//! exactly as it is in a normal (non-test) build of the crate, so this build
//! script needs none of their test-only dependencies (`proptest`).
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, ValueEnum};
use clap_complete::aot::{Shell, generate_to};

#[path = "src/cli/mod.rs"]
// `src/cli/mod.rs` re-exports every argument struct and value parser its
// submodules define, so the library keeps one `crate::cli::<Item>` path per
// item. This generator only ever names `Cli`, and `mod cli;` is private here, so
// those re-exports lead nowhere in *this* compilation — which is precisely what
// `unused_imports` reports. Allowed for the same reason the two sibling modules
// below allow `dead_code`: the unused part belongs to the library, not to a
// mistake in this script.
#[allow(unused_imports)]
mod cli;
#[path = "src/labels.rs"]
#[allow(dead_code)]
mod labels;
#[path = "src/text.rs"]
#[allow(dead_code)]
mod text;

fn main() {
    // Re-run only when the CLI surface (or this script) actually changes —
    // otherwise Cargo would already skip re-running an unchanged build script.
    // A directory, watched recursively: every file under `src/cli/` is part of
    // the parser this script reads, including any submodule added later.
    println!("cargo:rerun-if-changed=src/cli");
    println!("cargo:rerun-if-changed=src/labels.rs");
    println!("cargo:rerun-if-changed=src/text.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set for build.rs"),
    );
    // Written to the workspace's `target/assets/`, not the private, hash-suffixed
    // `OUT_DIR` (`target/<profile>/build/<pkg>-<hash>/out/`): `OUT_DIR` is meant
    // for this crate's own compile-time `include!`s, not for a downstream release
    // step to locate. `target/assets/` stays at a fixed, predictable path across
    // profiles and `--target` triples — this script always runs on the host even
    // when cross-compiling, so the path never varies with the target triple —
    // which is what lets `.github/workflows/release.yml` find it deterministically.
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target"));
    let completions_dir = target_dir.join("assets").join("completions");
    // `man1`: the conventional MANPATH section-1 (user commands) subdirectory, so
    // `target/assets/man` can be copied wholesale onto a `MANPATH` entry.
    let man_dir = target_dir.join("assets").join("man").join("man1");

    create_dir(&completions_dir);
    create_dir(&man_dir);

    let bin_name = "processkit-cli";

    // Each generator gets its own fresh, not-yet-built `Command`: clap's builder
    // finalizes a command tree (including the auto-added `help` subcommand) the
    // first time it is built, and re-uses that finalized tree afterwards — so
    // reusing one already-built `Command` across both generators would make
    // `clap_mangen`'s own `disable_help_subcommand(true)` (below) a no-op and
    // spuriously emit `processkit-cli-help-*.1` pages for clap's synthetic `help`
    // subcommand, which is not part of the real CLI surface.
    let mut completion_cmd = cli::Cli::command();
    for &shell in Shell::value_variants() {
        generate_to(shell, &mut completion_cmd, bin_name, &completions_dir)
            .unwrap_or_else(|err| panic!("generate {shell} completions: {err}"));
    }

    // Recurses through every subcommand (skipping any `hide = true` one — none
    // exist today, but this keeps the generator honest if that ever changes) and
    // writes one man page per level: `processkit-cli.1`, `processkit-cli-run.1`,
    // `processkit-cli-probe.1`, etc. `clap_mangen::generate_to` disables the
    // synthetic `help` subcommand itself, so no `processkit-cli-help-*.1` pages
    // are produced.
    let mut man_cmd = cli::Cli::command();
    man_cmd.set_bin_name(bin_name);
    clap_mangen::generate_to(man_cmd, &man_dir)
        .unwrap_or_else(|err| panic!("generate man pages: {err}"));
}

fn create_dir(dir: &Path) {
    fs::create_dir_all(dir).unwrap_or_else(|err| panic!("create {}: {err}", dir.display()));
}
