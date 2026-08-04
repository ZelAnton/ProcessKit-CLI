# Installation and distribution

`processkit-cli` is distributed as one self-contained executable. A machine
that runs the binary does not need Python, a virtual environment, or a Rust
toolchain. Choose a prebuilt archive directly or through `cargo-binstall` for
production and CI; use `cargo install` when building from source is already part
of your environment.

## Prebuilt archives

### One-command verified install

The repository installers select the host archive, download its published
`.sha256` sidecar, fail closed on any mismatch, and install only after successful
verification. They refuse to replace an existing destination unless running that
file with `--version` identifies it as `processkit-cli`.

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/ZelAnton/ProcessKit-CLI/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/ZelAnton/ProcessKit-CLI/main/install.ps1 | iex
```

The defaults install the latest release under `~/.local/bin`. For a pinned
version, custom destination, or explicit target, download the script and pass its
options directly:

```sh
sh install.sh --version X.Y.Z --install-dir /opt/processkit/bin
```

```powershell
.\install.ps1 -Version X.Y.Z -InstallDir C:\Tools\ProcessKit
```

The scripts print the installed binary's `--version` result and tell you when the
destination is not yet on `PATH`. The POSIX installer supports the published Linux
x86_64/Arm64 and macOS Arm64 builds; use `--target
x86_64-unknown-linux-musl` or `--target aarch64-unknown-linux-musl` when the
static musl archive is required for the matching architecture. The PowerShell
installer selects x86_64 or Arm64 from `PROCESSOR_ARCHITECTURE`.

Every [GitHub Release](https://github.com/ZelAnton/ProcessKit-CLI/releases)
contains one archive per supported target:

| Platform | Target | Archive format |
| --- | --- | --- |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows Arm64 | `aarch64-pc-windows-msvc` | `.zip` |
| Linux x86_64, glibc | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux Arm64, glibc | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Linux x86_64, static musl | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux Arm64, static musl | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |

The naming convention is
`processkit-cli-v<version>-<target>.<format>`. Each archive has a neighboring
`.sha256` file and a signed GitHub build-provenance attestation.

### Linux and macOS

```sh
version=X.Y.Z
target=x86_64-unknown-linux-gnu
archive="processkit-cli-v${version}-${target}.tar.gz"
base="https://github.com/ZelAnton/ProcessKit-CLI/releases/download/v${version}"

curl -sSLO "$base/$archive"
curl -sSLO "$base/$archive.sha256"
sha256sum -c "$archive.sha256" # macOS: shasum -a 256 -c
tar -xzf "$archive"
install -m 0755 processkit-cli "$HOME/.local/bin/processkit-cli"
```

### Windows PowerShell

```powershell
$version = 'X.Y.Z'
$target = 'x86_64-pc-windows-msvc'
$archive = "processkit-cli-v$version-$target.zip"
$base = "https://github.com/ZelAnton/ProcessKit-CLI/releases/download/v$version"

Invoke-WebRequest "$base/$archive" -OutFile $archive
Invoke-WebRequest "$base/$archive.sha256" -OutFile "$archive.sha256"
Get-FileHash -Algorithm SHA256 $archive
# Compare the printed hash with the value in $archive.sha256 before extracting.
Expand-Archive $archive -DestinationPath .\processkit-cli
```

Put `processkit-cli.exe` in a directory already present in `PATH`, or add its
installation directory to `PATH` for the account that launches the runner.

## Verify provenance

The checksum detects damaged or substituted bytes. The attestation additionally
proves that GitHub Actions built the archive from this repository:

```sh
gh attestation verify "$archive" --repo ZelAnton/ProcessKit-CLI
```

Both checks are useful in automation: the checksum is portable and offline once
downloaded, while attestation verification ties the bytes to the release
workflow and repository identity.

## Package-manager manifests

After the release-archive matrix settles, the release workflow reads the
published `.sha256` sidecars for each archive the channels use and attaches
distributor-ready manifests for
[winget](https://learn.microsoft.com/en-us/windows/package-manager/package/manifest),
[Scoop](https://github.com/ScoopInstaller/Scoop/wiki/App-Manifests), and a
[Homebrew tap](https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap). The same
files are collected in
`processkit-cli-v<version>-package-manifests.tar.gz`, with a neighboring bundle
checksum. The generator rejects a sidecar unless it names the exact archive the
manifest will download, so URLs and digests cannot be paired accidentally.

Package managers need a repository of their own; a manifest attached to this
project's GitHub Release is distributor input, not a public package source. The
release workflow can publish the Homebrew and Scoop files into repositories this
project owns, but that publication stays off until an operator provisions those
repositories (see "Publishing to a tap or bucket" below), and winget's is
external by nature. The current publication boundary is explicit:

| Channel | Generated release asset | Availability |
| --- | --- | --- |
| winget | Three-file `ZelAnton.ProcessKitCLI` manifest for x86_64 and Arm64 | Submit all three files to `microsoft/winget-pkgs`; installation is available only after Microsoft's external review accepts the version. Deliberately not automated — see "Why winget is submitted by hand" below. |
| Scoop | `processkit-cli.json` for x86_64 and Arm64 | Ready for `bucket/processkit-cli.json` in an account-owned bucket, and published there by every release once the operator sets `SCOOP_BUCKET_TOKEN`; no canonical public bucket is advertised yet. |
| Homebrew | `processkit-cli.rb` for macOS Arm64 and Linux x86_64/Arm64 | Ready for `Formula/processkit-cli.rb` in an account-owned tap, and published there by every release once the operator sets `HOMEBREW_TAP_TOKEN`; both Linux architectures use their static musl archive so the formula does not inherit the release runner's glibc floor. No canonical public tap is advertised yet. |

Once a source is actually published, that package manager provides its normal
install and upgrade lifecycle. Until its availability row changes, use the
verified installer, `cargo-binstall`, or `cargo install`; do not paste a future
bucket/tap name into automation and assume it exists.

### Publishing to a tap or bucket

The last job of `.github/workflows/release.yml`, `publish-package-repos`, takes
the formula and Scoop manifest the job before it attached to the Release and
pushes each into the package repository it belongs in. It is inert until an
operator sets that up, and it cannot fail a release:

| Channel | Target repository (default) | Enabling secret | Published path |
| --- | --- | --- | --- |
| Homebrew | `<owner>/homebrew-tap` | `HOMEBREW_TAP_TOKEN` | `Formula/processkit-cli.rb` |
| Scoop | `<owner>/scoop-bucket` | `SCOOP_BUCKET_TOKEN` | `bucket/processkit-cli.json` |

To turn a channel on:

1. Create the target repository under the same owner. For Homebrew the name is
   not free-form: `brew tap <owner>/tap` resolves to `<owner>/homebrew-tap`.
2. Add the repository secret (Settings → Secrets and variables → Actions) with a
   token that can push to *that* repository and nothing else — a fine-grained
   personal access token with `Contents: read and write` on the tap/bucket
   alone, or a GitHub App installation token. The workflow's built-in
   `GITHUB_TOKEN` cannot be used: it is scoped to this repository.
3. Optionally set the repository variable `HOMEBREW_TAP_REPOSITORY` or
   `SCOOP_BUCKET_REPOSITORY` when the target is named differently. The token
   secret alone decides whether a channel publishes; the variable only renames
   the target.
4. After a release has actually published, update the availability table above.
   That table, not this section, is what a reader is told to trust.

The two channels are independent — configure either, both, or neither. The job
publishes exactly the bytes attached to the Release rather than regenerating
them, pushes nothing when the target already holds identical bytes, and can be
re-run on its own. Without the secret it skips and logs a notice naming the
repository and secret to create. A configured channel that fails (repository
missing, token expired, protected branch) is reported as an annotation but
leaves the release green, because publication runs after crates.io, the tag, the
GitHub Release, and every asset upload.

For this repository today, neither `ZelAnton/homebrew-tap` nor
`ZelAnton/scoop-bucket` exists, so no release has published to either and the
availability rows above still say so. As soon as the operator creates
`ZelAnton/homebrew-tap` and sets `HOMEBREW_TAP_TOKEN`, the next release publishes
the formula there automatically and `brew install ZelAnton/tap/processkit-cli`
becomes a working command — the point at which that row has to change.

### Why winget is submitted by hand

winget has no repository under this project's control. A version becomes
installable only after a pull request to `microsoft/winget-pkgs` passes
Microsoft's automated validation and human review. Automating that submission
with `wingetcreate submit` — the standard tool — would require a maintained fork
of that repository plus a token scoped broadly enough to push to it, and a green
release step still would not mean the version is installable, because the
decision belongs to reviewers outside this project and a rejected or stalled
pull request is not something a release workflow can act on. The three-file
manifest is therefore attached to the Release and submitted deliberately, by
hand or with `wingetcreate submit`, when the maintainer chooses to.

## Install with cargo-binstall

If [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) is already
available, install the matching prebuilt binary without a local source build:

```sh
cargo binstall processkit-cli
```

The crate metadata resolves to the same GitHub Release archive documented above:
`.tar.gz` with a root-level `processkit-cli` on Unix, and `.zip` with a root-level
`processkit-cli.exe` on Windows. The adjacent `.sha256` and signed provenance
attestation belong to that same release, but cargo-binstall does not verify those
sidecars automatically. Use the manual archive path when policy requires those
checks or when you also need `completions/`, `man/`, or `schema/`.

## Install from crates.io

```sh
cargo install processkit-cli
```

This builds for the current host and installs into Cargo's binary directory.
Use `cargo install processkit-cli --locked` when you want the dependency graph
from the published lockfile rather than a newly resolved compatible graph.

Building from source also generates shell completions and man pages under
`target/assets/` in the build tree. Release archives already contain those
assets.

## Shell completions

The `completions/` directory contains bash, zsh, fish, PowerShell, and Elvish
scripts generated from the same clap definition as the binary.

```sh
# bash
install -Dm644 completions/processkit-cli.bash \
  "$HOME/.local/share/bash-completion/completions/processkit-cli"

# fish
install -Dm644 completions/processkit-cli.fish \
  "$HOME/.config/fish/completions/processkit-cli.fish"
```

```powershell
$destination = Join-Path (Split-Path $PROFILE) '_processkit-cli.ps1'
Copy-Item completions\_processkit-cli.ps1 $destination
Add-Content $PROFILE ". '$destination'"
```

The exact filenames are visible in the extracted `completions/` directory;
shell conventions differ slightly between package managers.

## Man pages

Each subcommand has a page under `man/man1/`, plus the top-level
`processkit-cli.1` page:

```sh
install -Dm644 man/man1/*.1 "$HOME/.local/share/man/man1/"
man processkit-cli-run
```

## Schema files in an archive

Release archives carry `schema/schema.json` and `schema/events.jsonl`. They are
the machine-readable JSON Schema and golden event stream for the binary's
current `schema_version`. An installed binary can print the same schema without
an archive:

```sh
processkit-cli probe --json --print-schema > processkit-cli.schema.json
```

See [Compatibility and upgrades](compatibility.md) before replacing a runner
that is consumed by an adapter.

## Post-install verification

```sh
processkit-cli --version
processkit-cli probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run \
  --require-surface run:--jsonl
```

The first command confirms which executable `PATH` resolves. The second is a
side-effect-free compatibility check: it launches no child, creates no registry
entry, and exits with `110` when a requirement is not met.

## Next steps

- [Cookbook](cookbook.md) — copyable task-oriented examples.
- [Running commands](running-commands.md) — argv, working directory,
  environment, run identity, and foreground behavior.
- [Platform support](platform-support.md) — containment strength and the
  meaning of `mechanism` / `abrupt_cleanup`.
