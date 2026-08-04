# Running in containers

ProcessKit CLI can run inside Windows and Linux containers, but an outer
container and an inner ProcessKit run solve different problems. The outer
runtime controls the pod/container; ProcessKit owns one payload tree and its
lifecycle contract inside that boundary.

## Choosing the Linux archive

| Base image | Recommended artifact |
| --- | --- |
| Debian / Ubuntu / glibc distroless | `x86_64-unknown-linux-gnu` or Arm64 glibc |
| Alpine / musl / minimal static image | `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` |

The musl archives are statically linked against libc and are suitable for
images without glibc, on both x86_64 and Arm64. They are a distribution
option, not a different containment mechanism.

## Minimal multi-stage image

```dockerfile
FROM alpine:3.22 AS unpack
ARG PROCESSKIT_CLI_VERSION
ARG ARCHIVE=processkit-cli-v${PROCESSKIT_CLI_VERSION}-x86_64-unknown-linux-musl.tar.gz
ADD https://github.com/ZelAnton/ProcessKit-CLI/releases/download/v${PROCESSKIT_CLI_VERSION}/${ARCHIVE} /tmp/processkit-cli.tar.gz
RUN tar -xzf /tmp/processkit-cli.tar.gz -C /tmp

FROM scratch
COPY --from=unpack /tmp/processkit-cli /usr/local/bin/processkit-cli
ENTRYPOINT ["/usr/local/bin/processkit-cli"]
```

On an Arm64 host, override the build argument instead of the default:
`--build-arg ARCHIVE=processkit-cli-v${PROCESSKIT_CLI_VERSION}-aarch64-unknown-linux-musl.tar.gz`.

Verify the archive checksum and attestation in the build pipeline before this
copy step; the abbreviated example focuses on final-image shape.

## Shell-free entrypoints

The CLI itself does not require a shell. Use JSON/exec-form container commands:

```dockerfile
ENTRYPOINT ["/usr/local/bin/processkit-cli", "run", "--jsonl", "/run/events.jsonl", "--"]
CMD ["/usr/local/bin/worker", "--serve"]
```

In Kubernetes, express the same boundary with `command` and `args`. Avoid
wrapping the runner in `sh -c` unless the payload truly needs shell semantics;
the wrapper changes signal routing and adds another process to diagnose.

## Persist lifecycle data

`--jsonl` is required. Mount or create a writable location whose lifetime
matches the observer's needs:

```yaml
volumeMounts:
  - name: run-state
    mountPath: /var/lib/processkit-cli
args:
  - run
  - --jsonl
  - /var/lib/processkit-cli/events.jsonl
  - --
  - /app/worker
```

Use a second bounded directory for `--capture-dir` when transcripts must
survive the container process. Apply a storage quota outside the CLI in addition
to its per-stream byte cap.

## PID 1 and signals

When `processkit-cli` is PID 1, it receives the container runtime's `SIGTERM`
directly. The runner catches it and performs ordinary cancel teardown, including
terminal JSONL events and registry cleanup, before exit.

Set the orchestrator's termination grace period longer than the runner's
`--grace` plus application shutdown overhead. If the orchestrator sends
`SIGKILL` first, normal teardown cannot run and Linux guarantees only the
`abrupt_cleanup` value reported in `run_started`.

## cgroup v2 is not automatically delegated

Seeing `/sys/fs/cgroup` or a cgroup-v2 mount does not mean the runner may create
and configure child cgroups. Ordinary Docker/Kubernetes containers usually run
inside a delegated subtree without permission to enable controllers at the
effective root.

Consequences:

- an unrestricted run may report `process_group` fallback;
- `--max-memory`, `--max-processes`, or `--cpu-quota` may fail before spawn with
  `limit_hit` / `BACKEND` (`102`);
- mounting cgroup files read-write or running privileged changes the threat
  boundary and should not be done solely to silence that failure.

Prefer outer container limits when the orchestrator is the resource-policy
owner.

## Outer limits and inner limits

| Layer | Typical responsibility |
| --- | --- |
| Kubernetes/Docker/systemd | Scheduling, pod/container memory and CPU, restart policy |
| ProcessKit CLI | One payload tree, timeout/cancel semantics, JSONL, local IPC |

If both layers install limits, the stricter limit wins. ProcessKit currently
cannot attribute an outer OOM kill or every inner kernel limit event; use the
orchestrator's status/events as the source of truth for outer-runtime
termination.

## Registry location and container users

The run registry is per-user and created with owner-only permissions. Keep the
same user identity for `run` and its `inspect` / `cancel` / `kill` / `attest` /
`wait` clients. A sidecar running under a different UID should not expect access.

For detached runs, ensure the registry and JSONL locations survive for as long
as the detached runner. An ephemeral container that exits immediately after
launch cannot host a meaningful detached child.

## Read-only root filesystems

The binary itself works from a read-only image, but it still needs writable
locations for:

- the required JSONL file;
- the per-user registry/control socket directory;
- optional capture files.

Mount explicit writable volumes or tmpfs paths and set the child working
directory accordingly. A setup failure is reported before the payload runs.

## Health and shutdown pattern

The CLI does not implement retries, pooling, or application health checks.
Let the orchestrator own restart policy and use ProcessKit CLI for deterministic
one-run control:

1. start `run` as the container's main process;
2. read `run_started` before declaring startup complete;
3. on shutdown, send `SIGTERM` and allow the configured grace;
4. collect terminal JSONL and capture files;
5. use the outer runtime's status for OOM/eviction attribution.

## See also

- [Installation and distribution](installation.md).
- [Platform support](platform-support.md).
- [Resource limits](resource-limits.md).
- [Detached runs](detached-runs.md).
