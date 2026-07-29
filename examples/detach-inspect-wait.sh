#!/bin/sh
set -eu

bin=${PROCESSKIT_CLI_BIN:-processkit-cli}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/processkit-cli-example.XXXXXX")
run_id="example-detached-$$"
events=$scratch/events.jsonl
cleanup() {
  "$bin" cancel --run-id "$run_id" >/dev/null 2>&1 || true
  "$bin" wait --run-id "$run_id" --timeout 5s >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

"$bin" run --detach --run-id "$run_id" --timeout 10s --jsonl "$events" \
  -- /bin/sh -c 'sleep 2'
"$bin" inspect --run-id "$run_id" --json |
  python3 -c 'import json,sys; value=json.load(sys.stdin); assert value["run_id"] == sys.argv[1]' "$run_id"
"$bin" wait --run-id "$run_id" --timeout 10s

python3 - "$events" <<'PY'
import json
import pathlib
import sys

terminal = json.loads(pathlib.Path(sys.argv[1]).read_text().splitlines()[-1])
assert terminal["event"] == "runner_exit", terminal
assert terminal["code"] == 0, terminal
PY
