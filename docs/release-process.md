# Release process

This is the single maintainer-facing reference for how a release happens: what
you click, what `.github/workflows/release.yml` does step by step, what repo
configuration it needs, and how to recover from a failure partway through. It
supersedes the old root-level `release-token-bypass.md` (folded in below as
its own section).

## Triggering a release

The workflow is `workflow_dispatch`-only — it has no `push:`/tag trigger, so it
never runs on its own. In the Actions UI, run **Release** and pick a version
bump (`patch` / `minor` / `major`). It always runs from `main` (an explicit
"Require main" step rejects any other ref, even if you pick one from the
dropdown). The version number is never typed by hand: the bump you pick is
applied to whatever `Cargo.toml` currently says, and that derived version then
drives the commit, the tag, and the GitHub Release, so the three can never
drift apart. The very first release (no `v*` tag exists yet) ignores the
chosen bump and ships the current `Cargo.toml` version as-is.

## What the `release` job does, step by step

1. **Mint GitHub App token** (conditional). If repo variable `RELEASE_APP_ID`
   is set, mints a short-lived GitHub App installation token, used further down
   to push as the App instead of the default `GITHUB_TOKEN`. See
   "GitHub App bypass for a protected `main`" below for why and how to set this
   up. Skipped entirely when the variable is empty.
2. **Checkout** with full history (`fetch-depth: 0`, needed for tag-based
   version math and for git-cliff to walk commits) and the token from step 1
   (or `GITHUB_TOKEN` as a fallback) so the later push carries the right
   identity.
3. **Require main** — fails fast if the workflow was dispatched from anything
   other than `refs/heads/main`.
4. **Preflight — require `CRATES_IO_TOKEN`** — fails fast, before any of the
   slower work below, if the `CRATES_IO_TOKEN` repo secret isn't set.
5. **Determine version** — parses the current `version` from `Cargo.toml`. If a
   prior `v*` tag exists, applies the chosen bump (major/minor/patch) to it;
   otherwise (first release) keeps the current `Cargo.toml` version unchanged.
   Exposes `version`, `tag` (`v<version>`) and `prev_tag` as step outputs.
6. **Verify tag does not exist** — refuses to proceed if `v<version>` is
   already tagged.
7. **Bump version** — `cargo set-version` writes the computed version into
   `Cargo.toml`/`Cargo.lock`. A no-op on the first release.
8. **Auto-fill empty `[Unreleased]` from git log** — manual `CHANGELOG.md`
   entries always win. Only when the `## [Unreleased]` section in
   `CHANGELOG.md` has no real bullets does this step generate one via
   `git-cliff --config cliff.toml`, walking commits since `prev_tag` (or full
   history on the first release) and bucketing them by commit-message prefix
   per `cliff.toml`'s rules (`feat`/`add` → Added, `fix`/`bug` → Fixed,
   `remove`/`delete`/`drop` → Removed, `refactor`/`change`/`update`/... →
   Changed, `doc`/`chore`/`test`/`style` → skipped, everything else falls back
   to Changed). Fails the run if there is nothing release-worthy to put there.
9. **Extract release notes** — curates the (now non-empty) `[Unreleased]` body
   down to only the `### Header` sections that have at least one real bullet,
   dropping placeholder `-` lines, and writes the result to
   `$RUNNER_TEMP/release-notes.md` — deliberately outside the working tree so
   it neither dirties it (which would abort `cargo publish`) nor ends up
   packaged into the crate.
10. **Promote `[Unreleased]` in `CHANGELOG.md`** — renames the curated
    `## [Unreleased]` heading to `## [<version>] - <date>`, leaves a fresh
    empty `[Unreleased]` above it, and rewrites the Keep-a-Changelog reference
    links (compare link on subsequent releases, tag link on the first one).
11. **Commit version bump + changelog** — commits `Cargo.toml`, `Cargo.lock`
    and `CHANGELOG.md` locally (not pushed yet, so the next step can verify
    against a clean tree).
12. **Verify the crate publishes (dry run)** — `cargo publish --locked
    --dry-run`, catching build/packaging/metadata errors before the
    irreversible step below.
13. **Publish to crates.io** — `cargo publish --locked`, retried up to 3
    attempts on transient failures. An "already uploaded"/"already exists"
    response from cargo (a prior run that published but failed before
    tagging) is treated as success, so a re-run can still proceed to tag +
    Release.
14. **Tag and push** — only after the crate is live: tags `v<version>` and
    pushes the commit + tag to `main` atomically (`git push --atomic`), so a
    rejected push can never advance the branch while dropping the tag (or vice
    versa).
15. **Publish GitHub Release** — creates (or, on retry, edits) the GitHub
    Release for the tag, using the curated notes file from step 9. Retried up
    to 3 attempts; if it still fails, the job error tells you to finish it by
    hand with `gh release create <tag> --notes-file <notes>` and explicitly
    **not** to re-run the workflow, since a re-run would bump to the next
    version from the now-updated `main` and strand this release.

## What the `build-artifacts` job does

Strictly downstream (`needs: release`) of the job above — it never bumps,
publishes to crates.io, tags, or creates the Release; it only builds and
attaches assets to the Release the `release` job already created. It fans out
across a `fail-fast: false` matrix of seven targets:

- `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` (Windows)
- `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (Linux glibc; the
  aarch64 leg cross-compiles with `gcc-aarch64-linux-gnu`)
- `x86_64-unknown-linux-musl` (static Linux, dependency-free binary; built on
  `ubuntu-latest`)
- `aarch64-unknown-linux-musl` (static Linux, dependency-free binary; built
  *natively* on the `ubuntu-24.04-arm` hosted runner instead of
  cross-compiling, since apt has no `aarch64-linux-musl` cross-gcc package)
- `aarch64-apple-darwin` (macOS, Apple Silicon)

For each target, the job:

1. Checks out the exact tagged commit (`ref: needs.release.outputs.tag`), so
   the binary embeds the released version.
2. Builds `cargo build --release --locked --target <triple>` (installing a
   cross linker first for the aarch64-glibc leg; the two musl legs each
   install `musl-tools` for their own native architecture instead).
3. Packages the binary, `build.rs`'s generated shell completions and man
   pages, and a `schema/` directory (`schema.json` + `events.jsonl`, copied
   verbatim from the tracked `fixtures/schema/v1/` — not `build.rs` output, so
   the fixture stays the single source of truth) into a per-target archive
   named `processkit-cli-v<version>-<triple>` (`.zip` on Windows via `7z`,
   `.tar.gz` elsewhere via `tar`).
4. Computes a `<archive>.sha256` checksum right next to the archive
   (`sha256sum` on Linux/Windows-Git-Bash; `shasum -a 256` fallback on macOS,
   which ships BSD tools without `sha256sum`).
5. Uploads the archive + checksum to the Release with `gh release upload
   --clobber` — idempotent, so a re-run of this job replaces rather than
   duplicates the assets.
6. Records a signed SLSA build-provenance attestation for the archive via
   `actions/attest-build-provenance`, kept last in the leg since the archive
   and checksum are already on the Release before it runs; a downloader
   verifies it with `gh attestation verify <archive> --repo
   ZelAnton/ProcessKit-CLI`.

Because `contents: write`/`id-token: write`/`attestations: write` are
re-declared at the job level here (a job-level `permissions:` block *replaces*,
rather than merges with, the top-level `contents: write`), the `release` job
above is unaffected and keeps only the top-level permission it already had.

## What the `package-manifests` job does

This final job waits for the release and every archive-matrix leg to settle. It
still runs when a leg failed only in its final provenance-attestation step,
because the archive and checksum were already uploaded; a missing required
checksum instead fails generation honestly. The job never publishes to an
external package repository. It:

1. Checks out the exact release tag and downloads the Release's archive
   `.sha256` sidecars.
2. Runs `scripts/generate_package_manifests.py`, which accepts only a SemVer
   release and verifies that every sidecar names the exact archive whose URL is
   being embedded.
3. Produces the three-file `ZelAnton.ProcessKitCLI` winget manifest, an
   architecture-aware Scoop `processkit-cli.json`, and a Homebrew
   `processkit-cli.rb` formula for macOS Arm64 plus Linux x86_64/Arm64. The
   Linux formula deliberately uses the static musl archives rather than
   inheriting the release runner's glibc floor, for both architectures.
4. Syntax-checks the JSON and Ruby output, packages the complete directory as
   `processkit-cli-v<version>-package-manifests.tar.gz`, and checksums that
   bundle.
5. Attaches the individual manifests, bundle, and bundle checksum to the
   existing GitHub Release with `--clobber` idempotence.

Winget retains its external `microsoft/winget-pkgs` review. Scoop and Homebrew
receive ready-to-copy files for an account-owned bucket/tap, but those separate
repositories are not mutated and need no credentials in this project. This
keeps all external channel availability out of the crate/tag/Release critical
path.

## Required repository configuration

- **`CRATES_IO_TOKEN`** (secret) — required. Publishing to crates.io fails the
  preflight check immediately if it's missing.
- **`RELEASE_APP_ID`** (variable) + **`RELEASE_APP_PRIVATE_KEY`** (secret) —
  optional. Only needed once `main` is protected by a ruleset that would
  otherwise reject the release commit/tag push (see the next section). Until
  `RELEASE_APP_ID` is set, the "Mint GitHub App token" step is skipped and the
  push falls back to the default `GITHUB_TOKEN` — fine while `main` is
  unprotected.
- The `GITHUB_TOKEN` used to create/edit the GitHub Release and to upload build
  artifacts and package manifests is the default one GitHub Actions provides;
  no setup needed beyond the job-level `permissions:` blocks already in the
  workflow.

## Recovering from a failure partway through a release

The steps are ordered specifically so that the one truly irreversible action —
publishing to crates.io — happens *before* anything is pushed or tagged, and
so `build-artifacts` only ever adds optional, re-buildable assets on top of an
already-complete release:

- **Failure before "Publish to crates.io"** (version bump, changelog
  generation/promotion, dry-run publish, etc.): nothing has left the runner.
  Just re-run the workflow from the same `bump` input; the earlier local commit
  is discarded with the job.
- **Failure during/after "Publish to crates.io" but before "Tag and push"**:
  the crate version is live on crates.io but `main` hasn't moved and no tag
  exists yet. Re-run the workflow — the "Publish to crates.io" step treats an
  "already uploaded"/"already exists" response as success and proceeds to tag
  and push, so this is safe and does not attempt a duplicate publish.
- **Failure during "Tag and push"**: the atomic push means either both the
  commit and the tag landed on `main`, or neither did — never a half state. If
  neither landed, re-run as above. If it actually did land but the step still
  reported failure (e.g. a flaky follow-up check), inspect `main` and the `v*`
  tags before re-running to avoid a wasted crates.io no-op.
- **Failure during "Publish GitHub Release"**: crates.io is published and the
  tag is pushed — `cargo install processkit-cli` and `cargo add
  processkit-cli` already work. Per the job's own error message: finish the
  Release by hand with `gh release create <tag> --notes-file <notes>` and do
  **not** re-run the workflow, since a re-run would bump from the
  already-advanced `main` and ship the *next* version, stranding this one.
- **Failure in `build-artifacts` (one or more matrix legs)**: the release
  itself (crate + tag + GitHub Release) is unaffected — this job never
  bumps/publishes/tags/creates the Release. `cargo install processkit-cli` and
  the GitHub Release page both already work; only the prebuilt archive for the
  failed target(s) is missing. Either re-run just that job (the upload is
  `--clobber`-idempotent and the attestation is regenerated), or build it by
  hand (`cargo build --release --target <triple>`, package it with the
  completions/man/`schema/` trees + checksum it the same way the job does —
  see "What the `build-artifacts` job does" above for the exact contents —
  then `gh release upload <tag> <archive> <archive>.sha256 --clobber`).
- **Failure in `package-manifests`**: the crate, tag, Release, and prebuilt
  archives are already published. Repair or upload any missing checksum
  sidecar, then re-run this job; generation is deterministic from the tagged
  script plus those sidecars, and every upload uses `--clobber`. Do not trigger
  a new release merely to repair these distributor inputs.

## GitHub App bypass for a protected `main`

`.github/workflows/release.yml` pushes the release commit (the version bump +
promoted `CHANGELOG.md`) and the `v<version>` tag straight to `main`. Once you
protect `main` with a rule that **requires pull requests**, that direct push is
rejected — for every actor except those on the rule's **bypass list**.

You cannot put the built-in `github-actions[bot]` on a bypass list (it is a
system actor, not an addressable App), and a personal access token expires and
ties the push to a human. The supported path is a **GitHub App**: the workflow
mints a short-lived installation token (auto-revoked, no rotation), pushes as
the App, and the App sits in the ruleset's bypass list.

When `main` is **not** protected, none of this is needed — the workflow falls
back to the default `GITHUB_TOKEN` and the push just works. Set this up only
once you turn on PR-required branch protection.

### One-time setup

1. **Create a GitHub App** (Settings → Developer settings → GitHub Apps → *New
   GitHub App*). Minimal config:
   - **Repository permissions → Contents: Read and write** (to push the commit
     + tag). Nothing else is required.
   - No webhook needed (uncheck *Active*).
   - It can be private to your account/org; it does not need to be public.

2. **Generate a private key** for the App (App settings → *Private keys* →
   *Generate a private key*) and download the `.pem`.

3. **Install the App** on the target repository (App settings → *Install App*
   → pick the repo).

4. **Add the credentials to the repo** (repo Settings → *Secrets and
   variables* → *Actions*):
   - **Variable** `RELEASE_APP_ID` = the App's numeric *App ID*.
   - **Secret** `RELEASE_APP_PRIVATE_KEY` = the full contents of the `.pem`
     (including the `-----BEGIN/END-----` lines).

   The workflow's "Mint GitHub App token" step is guarded by
   `if: ${{ vars.RELEASE_APP_ID != '' }}`, so until the variable exists the
   step is skipped and the push uses the default token.

5. **Add the App to the branch-protection bypass list.** Use a **repository
   ruleset** (repo Settings → *Rules* → *Rulesets*), which — unlike the older
   "branch protection rules" screen — supports a bypass list:
   - Target branch `main`, enable *Require a pull request before merging*.
   - Under **Bypass list**, add your App (it appears once installed).

   The App can now push directly to `main`; everyone else still goes through a
   PR.

### Verifying

Dispatch the release workflow (Actions → *Release* → *Run workflow* → pick a
bump). The **Mint GitHub App token** step should run (not skip), and the **Tag
and push** step should push the `Release v<version>` commit and tag to `main`
without a protection error. If the push is rejected, re-check that the App is
installed on the repo and is actually listed in the ruleset's bypass list, and
that `RELEASE_APP_ID` / `RELEASE_APP_PRIVATE_KEY` are set on the repo (not the
org, unless the App is org-owned).

> This is ordinary setup documentation: it applies only if `main` is protected
> by a ruleset that would otherwise reject the release workflow's tag push.
