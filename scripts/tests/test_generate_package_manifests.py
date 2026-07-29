import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parents[1] / "generate_package_manifests.py"
VERSION = "1.2.3"
TARGETS = {
    "x86_64-pc-windows-msvc": ".zip",
    "aarch64-pc-windows-msvc": ".zip",
    "x86_64-unknown-linux-musl": ".tar.gz",
    "aarch64-apple-darwin": ".tar.gz",
}


def write_sidecars(directory: Path) -> dict[str, str]:
    hashes: dict[str, str] = {}
    for index, (target, extension) in enumerate(TARGETS.items(), start=1):
        archive = f"processkit-cli-v{VERSION}-{target}{extension}"
        digest = f"{index:064x}"
        (directory / f"{archive}.sha256").write_text(
            f"{digest}  {archive}\n", encoding="utf-8"
        )
        hashes[target] = digest
    return hashes


class PackageManifestTests(unittest.TestCase):
    def test_generates_all_channels_from_the_expected_sidecars(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            checksums = root / "checksums"
            checksums.mkdir()
            hashes = write_sidecars(checksums)
            output = root / "output"

            subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--version",
                    VERSION,
                    "--checksums-dir",
                    str(checksums),
                    "--output-dir",
                    str(output),
                ],
                check=True,
            )

            winget = output / "winget"
            self.assertEqual(
                {path.name for path in winget.iterdir()},
                {
                    "ZelAnton.ProcessKitCLI.yaml",
                    "ZelAnton.ProcessKitCLI.locale.en-US.yaml",
                    "ZelAnton.ProcessKitCLI.installer.yaml",
                },
            )
            installer = (winget / "ZelAnton.ProcessKitCLI.installer.yaml").read_text(
                encoding="utf-8"
            )
            self.assertIn("ManifestVersion: 1.12.0", installer)
            self.assertIn("Architecture: x64", installer)
            self.assertIn("Architecture: arm64", installer)
            self.assertIn(
                f"InstallerSha256: '{hashes['x86_64-pc-windows-msvc'].upper()}'",
                installer,
            )
            self.assertIn("RelativeFilePath: processkit-cli.exe", installer)

            scoop = json.loads(
                (output / "scoop" / "processkit-cli.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(scoop["version"], VERSION)
            self.assertEqual(
                scoop["architecture"]["arm64"]["hash"],
                hashes["aarch64-pc-windows-msvc"],
            )
            self.assertEqual(scoop["bin"], "processkit-cli.exe")
            self.assertEqual(scoop["checkver"], "github")
            self.assertEqual(
                scoop["autoupdate"]["architecture"]["64bit"]["url"],
                "https://github.com/ZelAnton/ProcessKit-CLI/releases/download/"
                "v$version/processkit-cli-v$version-x86_64-pc-windows-msvc.zip",
            )

            formula = (output / "homebrew" / "processkit-cli.rb").read_text(
                encoding="utf-8"
            )
            self.assertIn("on_macos do", formula)
            self.assertIn("on_linux do", formula)
            self.assertEqual(formula.count("on_arm do"), 1)
            self.assertIn(hashes["aarch64-apple-darwin"], formula)
            self.assertIn(hashes["x86_64-unknown-linux-musl"], formula)
            self.assertIn("depends_on arch: :x86_64", formula)
            self.assertIn('bin.install "processkit-cli"', formula)

            readme = (output / "README.md").read_text(encoding="utf-8")
            for target in TARGETS:
                self.assertIn(target, readme)
            self.assertIn("not proof that a public package source", readme)

    def test_rejects_a_sidecar_that_names_a_different_archive(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            checksums = root / "checksums"
            checksums.mkdir()
            write_sidecars(checksums)
            archive = f"processkit-cli-v{VERSION}-x86_64-pc-windows-msvc.zip"
            (checksums / f"{archive}.sha256").write_text(
                f"{'a' * 64}  different.zip\n", encoding="utf-8"
            )

            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--version",
                    VERSION,
                    "--checksums-dir",
                    str(checksums),
                    "--output-dir",
                    str(root / "output"),
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(completed.returncode, 2)
            self.assertIn("does not name the expected archive", completed.stderr)

    def test_rejects_a_version_that_could_escape_a_manifest_string(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--version",
                    "1.2.3'\\nManifestType: singleton",
                    "--checksums-dir",
                    str(root),
                    "--output-dir",
                    str(root / "output"),
                ],
                capture_output=True,
                text=True,
            )

            self.assertEqual(completed.returncode, 2)
            self.assertIn("version must be a SemVer release", completed.stderr)


if __name__ == "__main__":
    unittest.main()
