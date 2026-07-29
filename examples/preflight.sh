#!/bin/sh
set -eu

bin=${PROCESSKIT_CLI_BIN:-processkit-cli}

"$bin" probe --json \
  --require-schema-version 1 \
  --require-exit-code-band 100-119 \
  --require-surface run:--jsonl \
  --require-surface run:--timeout \
  --require-surface inspect:--json \
  --require-surface wait:--all
