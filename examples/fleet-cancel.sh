#!/bin/sh
set -eu

bin=${PROCESSKIT_CLI_BIN:-processkit-cli}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/processkit-cli-example.XXXXXX")
label="example=fleet-$$"
cleanup() {
  "$bin" cancel --all --label "$label" >/dev/null 2>&1 || true
  "$bin" wait --all --label "$label" --timeout 5s >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

for member in one two; do
  "$bin" run --detach --run-id "example-fleet-$member-$$" --label "$label" \
    --timeout 30s --jsonl "$scratch/$member.jsonl" -- /bin/sh -c 'sleep 20'
done

"$bin" list --json --label "$label" |
  python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; assert len(rows) == 2, rows'
"$bin" cancel --all --label "$label"
"$bin" wait --all --label "$label" --timeout 10s

python3 - "$scratch/one.jsonl" "$scratch/two.jsonl" <<'PY'
import json
import pathlib
import sys

for path in sys.argv[1:]:
    terminal = json.loads(pathlib.Path(path).read_text().splitlines()[-1])
    assert terminal["event"] == "runner_exit", terminal
    assert terminal["source"] == "control_cancel", terminal
PY
