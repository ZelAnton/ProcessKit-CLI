# Platform support

ProcessKit CLI exposes one command surface across Windows, Linux, macOS, and
source-built BSD targets,
but it does not pretend their kernel containment primitives are equivalent.
Every run reports both the mechanism it obtained and what happens if the runner
dies before it can execute teardown.

## Release targets

| Operating system | Target triple | Distribution |
| --- | --- | --- |
| Windows x86_64 | `x86_64-pc-windows-msvc` | Prebuilt release archive |
| Windows Arm64 | `aarch64-pc-windows-msvc` | Prebuilt release archive |
| Linux x86_64 glibc | `x86_64-unknown-linux-gnu` | Prebuilt release archive |
| Linux Arm64 glibc | `aarch64-unknown-linux-gnu` | Prebuilt release archive |
| Linux x86_64 musl | `x86_64-unknown-linux-musl` | Static prebuilt archive |
| Linux Arm64 musl | `aarch64-unknown-linux-musl` | Static prebuilt archive |
| macOS Apple Silicon | `aarch64-apple-darwin` | Prebuilt release archive |

Other Rust-supported targets may build from source, but are not part of the
release-artifact or CI promise unless listed here.

## Mechanism and abrupt cleanup

`run_started` contains two independent fields:

- `mechanism`: how normal teardown addresses the tree;
- `abrupt_cleanup`: what the kernel guarantees when the runner never executes
  normal teardown.

| Platform / obtained mechanism | `mechanism` | `abrupt_cleanup` |
| --- | --- | --- |
| Windows Job Object | `job_object` | `whole_tree` |
| Linux cgroup v2 | `cgroup_v2` | `direct_child_only` |
| Linux process-group fallback | `process_group` | `direct_child_only` |
| FreeBSD process reaper | `process_reaper` | `none` |
| macOS / non-FreeBSD Unix process group | `process_group` | `none` |

Normal completion, timeout, caught cancellation signals, and control-plane
actions still run the full owned-container teardown on every platform. The
last column applies only to a crash, `SIGKILL`, outer Job termination, or an
equivalent event that prevents the runner from reaching its cleanup code.

## Windows Job Objects

A run owns a Job Object configured for kill-on-close. Child processes and their
descendants are assigned to the job, and closing the last owning handle reaps
the whole job.

Properties:

- strongest abrupt-owner-death guarantee (`whole_tree`);
- whole-tree memory, CPU, and active-process limits;
- member snapshots through Job queries;
- atomic hard kill on timeout/cancel/kill teardown;
- best-effort graceful close before hard kill where a member exposes a window;
- opt-in `CTRL_BREAK` for console children via `--windows-graceful-ctrl-break`.

### Nested jobs

Modern Windows versions allow nested Job Objects when the outer job's policy
permits it. A CI runner, service host, or container runtime may already place
the CLI in a job. The E2E suite covers nested-job launch behavior, but an outer
job remains authoritative and can terminate the runner plus its child job.

### Console behavior

The runner does not allocate a console. A normal child inherits what the
platform would ordinarily provide. `--create-no-window` is an explicit
Windows-only request and conflicts with `--inherit-stdio`.

`--windows-graceful-ctrl-break` keeps the shared console, creates a child console
process group, and lets ProcessKit address that group during graceful teardown. It
therefore conflicts with `--create-no-window` and detached execution.

## Linux cgroup v2

When cgroup v2 is available and delegated, ProcessKit creates a run cgroup and
moves the child tree into it. Normal hard teardown addresses all members in the
cgroup.

If controller/delegation requirements are not met, an unrestricted run may
fall back to a POSIX process group and reports `process_group`. A run that
explicitly requests resource limits fails instead, because falling back would
discard a required policy.

Linux parent-death signaling kills the direct child if the runner dies
abruptly. The cgroup itself persists and does not automatically kill every
grandchild, hence `direct_child_only` rather than `whole_tree`.

## FreeBSD process reaper

On FreeBSD, ProcessKit 3.3 uses the kernel's `procctl(2)` process reaper.
Normal teardown and member discovery cover the whole reaper tree, including
descendants that call `setsid` or double-fork. This is stronger than the POSIX
process-group fallback for membership and kill scope, so it reports
`process_reaper` rather than `process_group`.

FreeBSD does not provide the resource-limit or statistics primitives used by
ProcessKit's other backends. A run that requests a resource cap fails before
spawn with the normal `limit_hit`/backend-error sequence; an unrestricted run
reports `resource_summary` with all measurements `null` and
`read_error: false`. Parent-death cleanup remains `none`: `procctl(2)` gives
normal teardown a whole-tree scope, but ProcessKit 3.3 has no supported
owner-death primitive for this path.

## macOS and non-FreeBSD Unix

The runner uses a POSIX process group. Normal teardown signals and kills the
group, covering ordinary descendants that remain in it.

Limitations:

- a descendant can deliberately escape with `setsid` / double-fork;
- the backend cannot provide whole-tree resource limits;
- a just-exited member may still appear briefly during diagnostics;
- no portable owner-death primitive reaps the group after an uncatchable runner
  death, so `abrupt_cleanup` is `none`.

The mechanism field exists so an adapter can reject this weaker contract when
its workload requires stronger containment.

## Capability matrix

| Capability | Windows Job | Linux cgroup v2 | FreeBSD process reaper | POSIX process group |
| --- | --- | --- | --- | --- |
| Normal whole-tree hard teardown | Yes | Yes | Yes | Group members only |
| Whole-tree abrupt runner-death reap | Yes | No | No | No |
| Enriched member snapshots | Yes | Yes | Backend-dependent | Backend-dependent |
| Memory limit | Yes | With controller access | No | No |
| Process-count limit | Yes | With controller access | No | No |
| CPU quota | Yes | With controller access | No | No |
| Soft-stop request | Window close plus opt-in console `CTRL_BREAK` | `SIGTERM` | Whole-tree hard-stop | `SIGTERM` |
| Direct inherited terminal | Yes | Yes | Yes | Yes |
| PTY emulation | No | No | No | No |

## CI coverage

The repository's GitHub Actions matrix builds and tests Windows, Linux, and
macOS, including Arm runners where available. FreeBSD is source-build-only and
is not currently a CI or release-artifact target. The opt-in E2E tier drives the
built binary through real containment scenarios: leaked descendants, nonzero
roots, abrupt runner death, nested Windows jobs, PID reuse, real console/terminal
I/O, and cancellation.

That matrix proves the repository's release contract; it does not prove that a
particular Linux deployment grants cgroup controllers. Test mechanism selection
and limit application in the actual service/container environment.

## Confirming the table on the machine in front of you

Everything above is what a platform *can* give. Which of it this particular host
actually gives is a question about the host — cgroup delegation may be absent, a
registry directory may be unwritable, a local endpoint may not bind — and
`doctor` answers it by running a bounded scratch containment and reporting what it
observed:

```sh
processkit-cli doctor --json
```

Its `containment.mechanism` and `containment.abrupt_cleanup` are the same two fields
this page's first table lists, read off a real run on this host rather than inferred
from the platform; `--check-resource-controller` additionally reports whether the
limit rows of the capability matrix are available here. It is the setup-time
counterpart to the run-time acceptance policy below, and unlike `probe` it proves the
containment path end to end — see [`docs/troubleshooting.md`](troubleshooting.md),
"Qualifying a host: `doctor`".

## Choosing an acceptance policy

An adapter can read the first `run_started` event and fail closed:

```text
require mechanism == job_object or cgroup_v2
require abrupt_cleanup == whole_tree        # Windows-only today
```

The same two policies can be applied one step earlier, to the host rather than to a
run, with the same vocabulary and an exit code instead of a field to compare:

```sh
processkit-cli doctor --require-mechanism cgroup_v2
processkit-cli doctor --require-abrupt-cleanup whole_tree
```

Each is an exact match against what this host reports — deliberately not an "at least
this strong" comparison, because the three `abrupt_cleanup` levels are platform facts
and this project publishes no ordering between them. An unmet requirement exits
`HOST_UNQUALIFIED` (116) and still prints the full report.

These are separate policies. A Linux cgroup gives strong normal teardown but
not Windows-equivalent abrupt cleanup.

## See also

- [Running in containers](containers.md).
- [Resource limits](resource-limits.md).
- [Timeouts and cancellation](timeouts-and-cancellation.md).
- [JSONL event schema](schema.md#run_started).
- [Troubleshooting](troubleshooting.md#qualifying-a-host-doctor) — qualifying a
  host with `doctor`, and reading a negative verdict.
