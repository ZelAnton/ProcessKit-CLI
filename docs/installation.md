# Installation and distribution

`processkit-cli` is distributed as one self-contained executable. A machine
that runs the binary does not need Python, a virtual environment, or a Rust
toolchain. Choose a prebuilt archive directly or through `cargo-binstall` for
production and CI; use `cargo install` when building from source is already part
of your environment.

## Prebuilt archives

Every [GitHub Release](https://github.com/ZelAnton/ProcessKit-CLI/releases)
contains one archive per supported target:

| Platform | Target | Archive format |
| --- | --- | --- |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows Arm64 | `aarch64-pc-windows-msvc` | `.zip` |
| Linux x86_64, glibc | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux Arm64, glibc | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Linux x86_64, static musl | `x86_64-unknown-linux-musl` | `.tar.gz` |
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
