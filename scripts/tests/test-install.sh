#!/bin/sh
set -eu

root=$(mktemp -d)
server_pid=""
cleanup() {
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$root"
}
trap cleanup EXIT HUP INT TERM

version=0.0.0
target=x86_64-unknown-linux-gnu
archive="processkit-cli-v${version}-${target}.tar.gz"
release="$root/v$version"
payload="$root/payload"

if command -v python3 >/dev/null 2>&1; then
  python_bin=python3
elif command -v python >/dev/null 2>&1; then
  python_bin=python
else
  echo "python3 or python is required for the fixture server" >&2
  exit 1
fi

mkdir -p "$release" "$payload"
cat > "$payload/processkit-cli" <<'EOF'
#!/bin/sh
if [ "${1:-}" = "--version" ]; then
  echo "processkit-cli 0.0.0"
else
  exit 2
fi
EOF
chmod +x "$payload/processkit-cli"
tar -czf "$release/$archive" -C "$payload" processkit-cli
sha256sum "$release/$archive" > "$release/$archive.sha256"

port=$("$python_bin" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')
"$python_bin" -m http.server "$port" --bind 127.0.0.1 --directory "$root" >/dev/null 2>&1 &
server_pid=$!
attempt=0
until curl -fsS "http://127.0.0.1:$port/v$version/$archive.sha256" >/dev/null; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 20 ] || { echo "fixture server did not start" >&2; exit 1; }
  sleep 0.1
done

install_dir="$root/installed"
PROCESSKIT_CLI_RELEASE_BASE="http://127.0.0.1:$port" \
  sh ./install.sh --version "$version" --target "$target" --install-dir "$install_dir"
[ "$("$install_dir/processkit-cli" --version)" = "processkit-cli $version" ]

cat > "$install_dir/processkit-cli" <<'EOF'
#!/bin/sh
echo foreign-program
EOF
chmod +x "$install_dir/processkit-cli"
if PROCESSKIT_CLI_RELEASE_BASE="http://127.0.0.1:$port" \
  sh ./install.sh --version "$version" --target "$target" --install-dir "$install_dir"; then
  echo "installer overwrote a foreign destination" >&2
  exit 1
fi

printf '%064d  %s\n' 0 "$archive" > "$release/$archive.sha256"
if PROCESSKIT_CLI_RELEASE_BASE="http://127.0.0.1:$port" \
  sh ./install.sh --version "$version" --target "$target" --install-dir "$root/bad-hash"; then
  echo "installer accepted a checksum mismatch" >&2
  exit 1
fi

echo "POSIX installer smoke passed"
