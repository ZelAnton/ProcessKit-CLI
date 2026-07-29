#!/bin/sh
set -eu

bin=${PROCESSKIT_CLI_BIN:-processkit-cli}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/processkit-cli-example.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
events=$scratch/events.jsonl

"$bin" run --run-id "example-foreground-$$" --timeout 10s --grace 1s \
  --jsonl "$events" -- /bin/sh -c 'printf "example child output\n"'

python3 - "$events" <<'PY'
import json
import pathlib
import sys

events = [json.loads(line) for line in pathlib.Path(sys.argv[1]).read_text().splitlines()]
terminal = events[-1]
assert terminal["event"] == "runner_exit", terminal
assert terminal["code"] == 0, terminal
print(f"terminal event: source={terminal['source']} code={terminal['code']}")
PY
