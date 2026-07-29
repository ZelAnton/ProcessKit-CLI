#!/usr/bin/env python3
"""Generate package-manager manifests from published release checksums."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys


REPOSITORY = "https://github.com/ZelAnton/ProcessKit-CLI"
PACKAGE_IDENTIFIER = "ZelAnton.ProcessKitCLI"
MANIFEST_VERSION = "1.12.0"
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?\Z")
SHA256_PATTERN = re.compile(r"[0-9a-fA-F]{64}\Z")

TARGET_ARCHIVES = {
    "windows_x64": ("x86_64-pc-windows-msvc", ".zip"),
    "windows_arm64": ("aarch64-pc-windows-msvc", ".zip"),
    "linux_x64_musl": ("x86_64-unknown-linux-musl", ".tar.gz"),
    "macos_arm64": ("aarch64-apple-darwin", ".tar.gz"),
}


def archive_name(version: str, target: str, extension: str) -> str:
    return f"processkit-cli-v{version}-{target}{extension}"


def read_checksum(checksums_dir: Path, archive: str) -> str:
    sidecar = checksums_dir / f"{archive}.sha256"
    line = sidecar.read_text(encoding="utf-8").strip()
    parts = line.split()
    if len(parts) != 2 or parts[1].lstrip("*") != archive:
        raise ValueError(f"{sidecar.name} does not name the expected archive")
    digest = parts[0]
    if not SHA256_PATTERN.fullmatch(digest):
        raise ValueError(f"{sidecar.name} does not contain a SHA-256 digest")
    return digest.lower()


def release_data(version: str, checksums_dir: Path) -> dict[str, dict[str, str]]:
    data: dict[str, dict[str, str]] = {}
    base = f"{REPOSITORY}/releases/download/v{version}"
    for key, (target, extension) in TARGET_ARCHIVES.items():
        archive = archive_name(version, target, extension)
        data[key] = {
            "archive": archive,
            "url": f"{base}/{archive}",
            "sha256": read_checksum(checksums_dir, archive),
        }
    return data


def winget_manifests(version: str, data: dict[str, dict[str, str]]) -> dict[str, str]:
    version_manifest = f"""# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.{MANIFEST_VERSION}.schema.json

PackageIdentifier: {PACKAGE_IDENTIFIER}
PackageVersion: '{version}'
DefaultLocale: en-US
ManifestType: version
ManifestVersion: {MANIFEST_VERSION}
"""
    locale_manifest = f"""# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.{MANIFEST_VERSION}.schema.json

PackageIdentifier: {PACKAGE_IDENTIFIER}
PackageVersion: '{version}'
PackageLocale: en-US
Publisher: Zhelezniakou Anton
PublisherUrl: https://github.com/ZelAnton
PublisherSupportUrl: {REPOSITORY}/issues
Author: Zhelezniakou Anton
PackageName: ProcessKit CLI
PackageUrl: {REPOSITORY}
License: MIT
LicenseUrl: {REPOSITORY}/blob/v{version}/LICENSE
ShortDescription: Shell-free process-tree containment and lifecycle diagnostics
Description: >-
  Runs one command inside ProcessKit's kernel-backed containment boundary with
  exit-code fidelity, versioned JSONL lifecycle events, and a live control plane.
Moniker: processkit-cli
Tags:
  - cli
  - process
  - process-tree
  - runner
ReleaseNotesUrl: {REPOSITORY}/releases/tag/v{version}
ManifestType: defaultLocale
ManifestVersion: {MANIFEST_VERSION}
"""
    installer_manifest = f"""# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.{MANIFEST_VERSION}.schema.json

PackageIdentifier: {PACKAGE_IDENTIFIER}
PackageVersion: '{version}'
InstallerType: zip
NestedInstallerType: portable
Commands:
  - processkit-cli
Installers:
  - Architecture: x64
    InstallerUrl: {data['windows_x64']['url']}
    InstallerSha256: '{data['windows_x64']['sha256'].upper()}'
    NestedInstallerFiles:
      - RelativeFilePath: processkit-cli.exe
        PortableCommandAlias: processkit-cli
  - Architecture: arm64
    InstallerUrl: {data['windows_arm64']['url']}
    InstallerSha256: '{data['windows_arm64']['sha256'].upper()}'
    NestedInstallerFiles:
      - RelativeFilePath: processkit-cli.exe
        PortableCommandAlias: processkit-cli
ManifestType: installer
ManifestVersion: {MANIFEST_VERSION}
"""
    return {
        f"{PACKAGE_IDENTIFIER}.yaml": version_manifest,
        f"{PACKAGE_IDENTIFIER}.locale.en-US.yaml": locale_manifest,
        f"{PACKAGE_IDENTIFIER}.installer.yaml": installer_manifest,
    }


def scoop_manifest(version: str, data: dict[str, dict[str, str]]) -> str:
    manifest = {
        "version": version,
        "description": (
            "Shell-free process-tree containment and lifecycle diagnostics"
        ),
        "homepage": REPOSITORY,
        "license": "MIT",
        "architecture": {
            "64bit": {
                "url": data["windows_x64"]["url"],
                "hash": data["windows_x64"]["sha256"],
            },
            "arm64": {
                "url": data["windows_arm64"]["url"],
                "hash": data["windows_arm64"]["sha256"],
            },
        },
        "bin": "processkit-cli.exe",
        "checkver": "github",
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": (
                        f"{REPOSITORY}/releases/download/v$version/"
                        "processkit-cli-v$version-x86_64-pc-windows-msvc.zip"
                    ),
                },
                "arm64": {
                    "url": (
                        f"{REPOSITORY}/releases/download/v$version/"
                        "processkit-cli-v$version-aarch64-pc-windows-msvc.zip"
                    ),
                },
            }
        },
    }
    return json.dumps(manifest, indent=2) + "\n"


def homebrew_formula(data: dict[str, dict[str, str]]) -> str:
    return f'''class ProcesskitCli < Formula
  desc "Shell-free process-tree containment and lifecycle diagnostics"
  homepage "{REPOSITORY}"
  license "MIT"

  on_macos do
    depends_on arch: :arm64
    on_arm do
      url "{data['macos_arm64']['url']}"
      sha256 "{data['macos_arm64']['sha256']}"
    end
  end

  on_linux do
    depends_on arch: :x86_64
    on_intel do
      url "{data['linux_x64_musl']['url']}"
      sha256 "{data['linux_x64_musl']['sha256']}"
    end
  end

  def install
    bin.install "processkit-cli"
    bash_completion.install "completions/processkit-cli.bash" => "processkit-cli"
    zsh_completion.install "completions/_processkit-cli"
    fish_completion.install "completions/processkit-cli.fish"
    man1.install Dir["man/man1/*.1"]
    (pkgshare/"schema").install Dir["schema/*"]
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/processkit-cli --version")
  end
end
'''


def bundle_readme(version: str, data: dict[str, dict[str, str]]) -> str:
    sources = "\n".join(
        f"- `{entry['archive']}` — `{entry['sha256']}`"
        for entry in data.values()
    )
    return f"""# ProcessKit CLI {version} package manifests

These files were generated from the SHA-256 sidecars attached to the `v{version}`
GitHub Release. They are distributor inputs, not proof that a public package source
has accepted or published this version.

- `winget/`: submit all three YAML files together to `microsoft/winget-pkgs`.
- `scoop/processkit-cli.json`: publish as `bucket/processkit-cli.json` in a Scoop bucket.
- `homebrew/processkit-cli.rb`: publish as `Formula/processkit-cli.rb` in a Homebrew tap.

Source archives:

{sources}
"""


def write_manifests(version: str, checksums_dir: Path, output_dir: Path) -> None:
    if not VERSION_PATTERN.fullmatch(version):
        raise ValueError("version must be a SemVer release without build metadata")
    data = release_data(version, checksums_dir)

    winget_dir = output_dir / "winget"
    scoop_dir = output_dir / "scoop"
    homebrew_dir = output_dir / "homebrew"
    for directory in (winget_dir, scoop_dir, homebrew_dir):
        directory.mkdir(parents=True, exist_ok=True)

    for name, contents in winget_manifests(version, data).items():
        (winget_dir / name).write_text(contents, encoding="utf-8", newline="\n")
    (scoop_dir / "processkit-cli.json").write_text(
        scoop_manifest(version, data), encoding="utf-8", newline="\n"
    )
    (homebrew_dir / "processkit-cli.rb").write_text(
        homebrew_formula(data), encoding="utf-8", newline="\n"
    )
    (output_dir / "README.md").write_text(
        bundle_readme(version, data), encoding="utf-8", newline="\n"
    )


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--version", required=True)
    result.add_argument("--checksums-dir", required=True, type=Path)
    result.add_argument("--output-dir", required=True, type=Path)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        write_manifests(args.version, args.checksums_dir, args.output_dir)
    except (OSError, ValueError) as error:
        print(f"package manifest generation failed: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
