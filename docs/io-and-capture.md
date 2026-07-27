# Standard I/O and capture

I/O mode determines whether ProcessKit can observe the child's streams, whether
the child sees a terminal, and whether the runner can capture or suppress
output. Choose the mode from the child's actual contract rather than treating
terminal inheritance as a cosmetic switch.

## Mode matrix

| Mode | Child stdin | Child stdout/stderr | TTY preserved | Pump available |
| --- | --- | --- | --- | --- |
| Default | Closed / null | Pipe → echo | No | Yes |
| `--inherit-stdin` | Runner stdin | Pipe → echo | Output: no | Yes |
| `--stdin-file FILE` | File until EOF | Pipe → echo | No | Yes |
| `--inherit-stdio` | Runner stdin | Direct inherited handles | If caller has one | No |
| Detached | Null | Pump with echo discarded | No | Yes |

The JSONL file is independent of every mode. Events always go to `--jsonl`,
never to stdout.

## Default: closed stdin, pipe and echo

```sh
processkit-cli run --jsonl run.jsonl -- cargo test
```

The child cannot wait forever for input the runner does not own. Its stdout and
stderr are drained through separate pipes and retransmitted to the runner's
stdout and stderr.

Because the child sees pipes rather than a terminal, it may disable color,
progress bars, cursor movement, and interactive prompts. This is intentional
and makes the default suitable for CI and automation.

## Inherit all three handles

```sh
processkit-cli run --inherit-stdio --jsonl interactive.jsonl -- cargo watch
```

`--inherit-stdio` passes the caller's stdin, stdout, and stderr handles directly
to the child. If the caller is attached to a terminal, the child sees that same
terminal. The runner does not create a PTY.

Direct inheritance means there is no output pump. The following features are
therefore unavailable and rejected at parse time:

- `--capture-dir` and `--capture-max-bytes`;
- `--no-echo`;
- `--idle-timeout`;
- `--create-no-window`;
- `--inherit-stdin` and `--stdin-file`;
- `--detach`.

Terminal signal behavior is platform-dependent. On Unix a child in a separate
foreground process group may receive `Ctrl-C` directly and report a signal exit;
on Windows both child and runner can observe the console event. Use the local
control-plane `cancel` command when an orchestrator needs one deterministic
runner-owned cancellation outcome.

## Inherit stdin only

```sh
processkit-cli run --inherit-stdin --jsonl input.jsonl -- sort
```

The child reads from the runner's input handle, while stdout/stderr remain on
the default pump. The runner neither records nor mediates stdin bytes. This mode
does not create a terminal; redirected caller input remains redirected input.

## Feed a file

```sh
processkit-cli run \
  --stdin-file requests.ndjson \
  --jsonl importer.jsonl \
  -- importer --format jsonl
```

The checked file is streamed to the child and stdin closes at EOF. File bytes do
not enter argv or lifecycle JSONL. A missing or unreadable file is a pre-spawn
`SETUP` (`111`) failure.

`--stdin-file` and `--inherit-stdin` are mutually exclusive. For small literal
input, create a file or explicitly invoke a shell/pipeline outside the runner;
the CLI intentionally has no string-to-stdin flag that could encourage secrets
inside process listings or shell history.

## Suppress live echo

```sh
processkit-cli run \
  --no-echo \
  --capture-dir ./capture \
  --jsonl run.jsonl \
  -- noisy-worker
```

`--no-echo` removes only the runner's retransmission. Pipes stay open and the
pump continues to:

- drain the child so it cannot block on a full pipe;
- write bounded capture files;
- re-arm `--idle-timeout` on observed output;
- preserve lifecycle events.

This is the normal embedding mode for an orchestrator that owns presentation
and does not want the runner's live copy of child output interleaved with its own
logs.

## Bounded transcript capture

`--capture-dir DIR` creates two files:

```text
DIR/
├── stdout.log
└── stderr.log
```

Streams remain separate and are still echoed unless `--no-echo` is present.
Each file is capped independently at 8 MiB by default:

```sh
processkit-cli run \
  --capture-dir ./capture \
  --capture-max-bytes 32m \
  --jsonl run.jsonl \
  -- compiler
```

The terminal `output_captured` event reports, per stream:

| Field | Meaning |
| --- | --- |
| `path` | Capture file location. |
| `bytes_seen` | Total bytes produced, including bytes beyond the file cap. |
| `bytes_written` | Bytes retained in the file. |
| `sha256` | Digest of the retained bytes. |
| `truncated` | Whether output exceeded the configured cap. |
| `write_error` | Capture write failure, if one occurred. |

Do not infer completeness from file size. Use `truncated`; a file whose length
equals the cap may still be complete when the stream ended at exactly that
boundary.

## Two independent bounds

`--capture-max-bytes` limits retained disk bytes per stream. The ProcessKit
pump also has a fixed 64 MiB in-flight line-assembly ceiling for a single
unterminated line. They protect different resources and neither derives from
the other.

## Binary output

The live echo and capture path operate on bytes. Capture files preserve the
bytes written up to the cap; they are not decoded or line-normalized. Runner
diagnostics remain on stderr and are not written into `stdout.log` or
`stderr.log`.

When stdout itself is a binary protocol, use `--no-echo` plus capture, or invoke
the child directly if no lifecycle control is needed. The JSONL stream never
shares stdout, so it cannot corrupt a binary child stream.

## Secret hygiene

Argv is redacted in events by default, but child output is not. A command may
echo credentials, environment values, file contents, or tokens into live output
and capture files. Apply access controls and retention policies to:

- the JSONL file when `--argv-raw` is used;
- `stdout.log` and `stderr.log`;
- any parent process that records runner stdout/stderr.

## Diagnosing a degraded terminal UI

If a tool loses color, switches to plain progress messages, or refuses an
interactive prompt under the default mode, confirm that it requires a TTY. Use
`--inherit-stdio` only when the caller already owns a real console/terminal and
does not need capture or idle detection.

ProcessKit CLI does not emulate a PTY. A program that specifically requires PTY
semantics cannot obtain them from this runner today.

## See also

- [Running commands](running-commands.md) — argv, cwd, environment, and run ids.
- [Timeouts and cancellation](timeouts-and-cancellation.md) — how output drives
  the idle deadline.
- [JSONL event schema](schema.md) — normative `output_captured` fields.
- [Troubleshooting](troubleshooting.md) — terminal and capture symptoms.
