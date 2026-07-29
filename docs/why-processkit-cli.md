# Why ProcessKit CLI?

ProcessKit CLI is for one shell-free command whose process tree must be cleaned
up on ordinary completion, timeout, or cancellation, while an adapter receives
a stable lifecycle record. It is not a universal service manager, container
runtime, or security sandbox. Choose the tool that owns the widest requirement;
the choices below can also be layered.

## Quick choice

| Primary requirement | Start with |
| --- | --- |
| One portable command surface, tree-scoped teardown, exact ordinary child exit status, and versioned lifecycle JSONL | ProcessKit CLI |
| The shortest available Unix command deadline with no new installation | GNU `timeout` |
| Terminal/session detachment or a new POSIX process group, with lifecycle code supplied by the caller | `setsid` / `start_new_session` |
| A Linux service or scope owned by the host manager, with native cgroup policy, journaling, and service integration | `systemd-run` / a systemd unit |
| Filesystem, network, user, and image isolation; deployment scheduling or restart policy | A container runtime / orchestrator |
| Correct PID 1 signal forwarding and zombie reaping inside a container | Tini or the runtime's `--init` mode |
| PowerShell-native asynchronous objects, streams, or remoting | A PowerShell job |
| A custom Windows-only host that already owns Win32 lifecycle code | A raw Win32 Job Object |

## Comparison at a glance

| Option | Process boundary | If its immediate launcher dies abruptly | Result and observability | Where that option wins |
| --- | --- | --- | --- | --- |
| ProcessKit CLI | The obtained ProcessKit mechanism: Windows Job Object, Linux cgroup v2, or a reported process-group fallback | Explicit `abrupt_cleanup`: `whole_tree` on Windows, `direct_child_only` on Linux, `none` on macOS/other Unix | Ordinary child status is preserved; runner-imposed endings and runner failures are distinct. Versioned JSONL, bounded diagnostics, local inspect/cancel/kill/wait. | One binary and one adapter contract across Windows, Linux, and macOS. |
| GNU `timeout` | A time limit and signal policy around one command; default and foreground modes have different process-group behavior | No separate kill-on-owner-death containment contract | Familiar shell status conventions and stderr diagnostics, not a versioned lifecycle stream | Near-ubiquitous, tiny, and ideal when a deadline is the whole requirement. |
| `setsid` / `start_new_session` | A POSIX session and process group, not a resource container | No owner-death reap; a descendant can create another session | Whatever wait, exit, logging, and cleanup logic the caller writes | Minimal mechanism for terminal detachment and shell/job-control composition. |
| systemd service or scope | A cgroup owned by the system or user service manager | The unit remains manager-owned rather than depending on the short-lived CLI client; stop and restart behavior follows unit policy | Native unit state, cgroup accounting, journal integration, and systemd resource controls | Durable Linux host supervision, delegated cgroups, boot integration, restart policy, and administrator tooling. |
| Container runtime / orchestrator | A container boundary, normally including namespaces and cgroups | The runtime owns container lifetime; daemon/orchestrator and restart settings decide recovery | Runtime-specific status, logs, events, health, and scheduling | Actual workload isolation, image distribution, network/filesystem policy, and fleet orchestration. |
| Tini / subreaper | One child, zombie adoption, and signal forwarding; optional process-group signaling | Tini alone does not add an independent kernel tree container | Reuses the child's exit status and solves PID 1 hygiene; no lifecycle JSONL or live run registry | Very small, transparent container init when reaping and signal forwarding are the missing pieces. |
| PowerShell job | A PowerShell job repository plus a child process, remote command, or thread depending on job type | Session-owned child jobs end with the parent session; this is not the Win32 Job Object kill-on-close contract | Rich PowerShell job state and serialized output/error streams | Interactive PowerShell concurrency, remoting, and object-oriented result handling. |
| Raw Win32 Job Object | A Windows kernel job; descendants normally join unless breakaway policy permits otherwise | `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` can provide whole-job termination when the last handle closes | Whatever schema, exit mapping, IPC, ACLs, and graceful-stop code the host implements | Maximum control inside an existing Windows-native host without adopting another CLI contract. |

The ProcessKit row is deliberately conditional. Read the actual `mechanism` and
`abrupt_cleanup` values from `run_started`; do not infer Windows' `whole_tree`
owner-death guarantee on Linux or macOS. The normative matrix is in
[Platform support](platform-support.md#mechanism-and-abrupt-cleanup).

## Compared with GNU `timeout`

GNU [`timeout`](https://www.gnu.org/s/coreutils/timeout) is the right answer for
many shell scripts: it is already installed, concise, supports a configurable
signal and kill-after interval, and normally preserves a command's status when
the command finishes before the deadline.

Choose ProcessKit CLI when the deadline is only one part of the contract:
cleanup must be tied to the obtained container, stdout and stderr must remain
separate from machine events, another process must inspect or stop a live run,
or an adapter must distinguish child exit, timeout, cancellation, and runner
failure without reverse-engineering shell status conventions. ProcessKit CLI is
larger and requires a JSONL destination; `timeout` is simpler when no consumer
needs those guarantees.

## Compared with `setsid` or `start_new_session`

POSIX [`setsid`](https://man7.org/linux/man-pages/man2/setsid.2.html) creates a
session and a new process group. Python's `start_new_session=True` requests the
same primitive. That is useful for terminal detachment and for giving the
caller a group to signal, but it is not a persistent list of every descendant:
a cooperating or hostile descendant may start another session, and the session
does not gain a kill-on-owner-death policy.

ProcessKit itself may honestly fall back to a process group, especially on
macOS or when Linux cgroup delegation is unavailable. In that case the CLI does
not claim a stronger boundary: `mechanism` is `process_group`, resource-limit
requests fail rather than becoming no-ops, and the documented escape and abrupt
death limitations apply. Use plain `setsid` when that primitive plus your own
wait/signal code is sufficient.

## Compared with `systemd-run --scope`

On a systemd Linux host, systemd is the natural owner of long-lived service
lifecycle and the cgroup tree. A transient service or scope gives administrators
native unit state, resource policy, accounting, and journal integration.
Systemd's documented
[delegation model](https://systemd.io/CGROUP_DELEGATION/) is also the correct
way to grant a nested manager its own cgroup subtree. Prefer it for host services,
boot integration, restart policy, and durable supervision.

ProcessKit CLI instead standardizes one child run across operating systems and
emits its own portable event schema. It does not replace systemd. They can be
layered: let systemd own the service and its outer limits, and use ProcessKit CLI
inside it when an adapter still needs per-invocation JSONL and the same command
contract on Windows and macOS. In a normal non-delegated systemd unit, ProcessKit
may report `process_group`; do not mount or rewrite cgroup state merely to force a
different mechanism.

## Compared with a container runtime

A container runtime solves a wider isolation and deployment problem. Docker, for
example, provides cgroup-backed
[resource constraints](https://docs.docker.com/engine/containers/resource_constraints/)
and configurable
[restart policies](https://docs.docker.com/engine/containers/start-containers-automatically/),
while an orchestrator adds scheduling, health, networking, secrets, and rollout
policy. ProcessKit CLI provides none of that isolation and intentionally trusts
other processes running as the same OS user.

Use the outer runtime for container lifetime and limits. ProcessKit CLI can be
the container entrypoint when one payload still needs its portable JSONL,
timeout/cancel outcome, and local control plane. Cgroup v2 is not automatically
delegated inside ordinary containers, so this layering can legitimately report
the process-group fallback; see [Running in containers](containers.md).

## Compared with Tini or another subreaper

[Tini](https://github.com/krallin/tini#readme) is intentionally small: as PID 1
or a Linux subreaper it adopts and reaps zombies, forwards signals to its child,
can signal the child's process group, and exits with the child's status. Docker's
[`--init`](https://docs.docker.com/engine/containers/multi-service_container/)
mode addresses the same PID 1 hygiene problem.

That is a feature, not an omission: Tini is an excellent choice when zombie
reaping and signal forwarding are the whole problem. ProcessKit CLI adds a
command deadline, reported containment, capture, resource-limit negotiation,
versioned events, and live-run IPC. It is correspondingly more opinionated and
requires persistent paths for JSONL and registry state.

## Compared with PowerShell jobs and raw Windows Job Objects

A PowerShell background job is a shell-level concurrency and result abstraction.
It exposes job state and PowerShell's output, error, warning, and other streams;
remote and thread jobs cover still different execution shapes. The official
[`about_Jobs`](https://learn.microsoft.com/powershell/module/microsoft.powershell.core/about/about_jobs)
documentation describes that session-owned repository. Prefer it when a
PowerShell operator wants asynchronous objects or remoting rather than a
language-neutral binary contract.

A PowerShell job is not the same thing as the Win32 kernel Job Object used by
ProcessKit on Windows. A raw
[Win32 Job Object](https://learn.microsoft.com/windows/win32/procthread/job-objects)
can group processes, enforce limits, and terminate the job on last-handle close.
It is the right primitive for a Windows-native host willing to implement its own
spawn race handling, nested-job policy, exit mapping, diagnostics, IPC security,
and wire schema. ProcessKit CLI packages that work behind a tested command
surface, but a custom host can expose deeper application-specific integration.

## Exit status, telemetry, and secrets

ProcessKit CLI preserves the child's exact status only when the child chose the
ordinary outcome. A timeout, cancellation, output-overflow stop, or runner failure
uses a documented reserved code and a terminal `runner_exit` event, so those
cases cannot masquerade as the child's decision. See the
[exit-code contract](exit-codes.md) and [JSONL schema](schema.md).

The JSONL stream is separate from child stdout/stderr and versioned for adapters.
Argv is hashed by default, with only a classified worker hint; raw argv is an
explicit `--argv-raw` opt-in. Other supervisors may provide richer native logs
or object streams, but their command metadata has its own disclosure rules. Do
not assume that switching wrappers preserves ProcessKit CLI's redaction contract.

## A practical decision rule

1. Need isolation, deployment, restart, or a durable host service? Choose the
   container runtime/orchestrator or systemd first.
2. Need only a Unix deadline, a new session, PowerShell concurrency, or PID 1
   reaping? Use the smaller native tool.
3. Need one cross-platform invocation contract plus tree teardown and lifecycle
   JSONL? Use ProcessKit CLI, then validate `mechanism` and `abrupt_cleanup` for
   the actual host.
4. Need both layers? Let the outer supervisor own machine/container policy and
   ProcessKit CLI own one inner command. Treat the outer supervisor's OOM,
   eviction, and restart events as authoritative for that outer boundary.

Continue with [Installation and distribution](installation.md), or start from
the copyable [Cookbook](cookbook.md).
