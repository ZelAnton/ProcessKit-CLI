#!/bin/sh
# Install a verified processkit-cli GitHub Release binary.
set -eu

repo="ZelAnton/ProcessKit-CLI"
version=""
install_dir="${HOME}/.local/bin"
target=""
release_base="${PROCESSKIT_CLI_RELEASE_BASE:-https://github.com/${repo}/releases/download}"

usage() {
  cat <<'EOF'
Usage: install.sh [--version X.Y.Z] [--install-dir DIR] [--target TRIPLE]

Downloads a GitHub Release archive, verifies its published SHA-256 checksum,
and installs processkit-cli. Omit --version to install the latest release.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) version=${2:?missing value for --version}; shift 2 ;;
    --install-dir) install_dir=${2:?missing value for --install-dir}; shift 2 ;;
    --target) target=${2:?missing value for --target}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "processkit-cli installer: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done
[ -n "$install_dir" ] || {
  echo "processkit-cli installer: install directory cannot be empty" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || {
  echo "processkit-cli installer: curl is required" >&2
  exit 1
}

if [ -z "$version" ]; then
  latest_url=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
    "https://github.com/${repo}/releases/latest")
  version=${latest_url##*/v}
fi
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo "processkit-cli installer: invalid release version: $version" >&2
  exit 1
}

if [ -z "$target" ]; then
  system=$(uname -s)
  machine=$(uname -m)
  case "$system:$machine" in
    Linux:x86_64|Linux:amd64) target=x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin:arm64|Darwin:aarch64) target=aarch64-apple-darwin ;;
    *)
      echo "processkit-cli installer: no prebuilt archive for $system/$machine" >&2
      echo "processkit-cli installer: pass --target explicitly or install with cargo" >&2
      exit 1
      ;;
  esac
fi
printf '%s\n' "$target" | grep -Eq '^[A-Za-z0-9_-]+$' || {
  echo "processkit-cli installer: invalid target triple: $target" >&2
  exit 1
}

archive="processkit-cli-v${version}-${target}.tar.gz"
base="${release_base}/v${version}"
scratch=$(mktemp -d)
staged=""
cleanup() {
  [ -z "$staged" ] || rm -f -- "$staged"
  rm -rf -- "$scratch"
}
trap cleanup EXIT HUP INT TERM

echo "processkit-cli installer: downloading $archive"
curl -fsSL "$base/$archive" -o "$scratch/$archive"
curl -fsSL "$base/$archive.sha256" -o "$scratch/$archive.sha256"

expected=$(awk 'NR == 1 { print tolower($1) }' "$scratch/$archive.sha256")
printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || {
  echo "processkit-cli installer: malformed checksum sidecar" >&2
  exit 1
}
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$scratch/$archive" | awk '{ print tolower($1) }')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$scratch/$archive" | awk '{ print tolower($1) }')
else
  echo "processkit-cli installer: sha256sum or shasum is required" >&2
  exit 1
fi
[ "$actual" = "$expected" ] || {
  echo "processkit-cli installer: SHA-256 mismatch for $archive" >&2
  exit 1
}

tar -xzf "$scratch/$archive" -C "$scratch" processkit-cli
downloaded_version=$("$scratch/processkit-cli" --version 2>/dev/null || true)
[ "$downloaded_version" = "processkit-cli $version" ] || {
  echo "processkit-cli installer: verified archive contains '$downloaded_version', expected processkit-cli $version" >&2
  exit 1
}
destination="$install_dir/processkit-cli"
if [ -e "$destination" ]; then
  installed_version=$(
    "$destination" --version 2>/dev/null || true
  )
  printf '%s\n' "$installed_version" | grep -Eq '^processkit-cli [0-9]+\.[0-9]+\.[0-9]+$' || {
    echo "processkit-cli installer: $destination exists but is not processkit-cli; refusing to overwrite" >&2
    exit 1
  }
fi

mkdir -p "$install_dir"
staged="$destination.tmp.$$"
install -m 0755 "$scratch/processkit-cli" "$staged"
mv -f "$staged" "$destination"
staged=""
"$destination" --version
echo "processkit-cli installer: installed to $destination"

case ":${PATH}:" in
  *":${install_dir}:"*) ;;
  *) echo "processkit-cli installer: add $install_dir to PATH" ;;
esac
