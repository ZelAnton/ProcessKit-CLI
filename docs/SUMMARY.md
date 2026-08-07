# Summary

<!-- These draft prefix chapters form the ProcessKit-family switcher. mdBook
     cannot point a SUMMARY entry at an external URL, so theme/nav-links.js
     upgrades the other implementations to links and marks CLI runner as the
     current project. -->
[Rust version]()
[CLI runner]()
[Python wrapper]()
[.NET version]()

---

[Overview](README.md)

---

# Start here

- [Why ProcessKit CLI?](why-processkit-cli.md)
- [Installation and distribution](installation.md)
- [Cookbook](cookbook.md)
- [Runnable examples](runnable-examples.md)
- [Agent and automation workflows](agent-workflows.md)
- [Running commands](running-commands.md)
- [Standard I/O and capture](io-and-capture.md)
- [Detached runs](detached-runs.md)
- [Timeouts and cancellation](timeouts-and-cancellation.md)
- [Resource limits](resource-limits.md)
- [Platform support](platform-support.md)
- [Running in containers](containers.md)

# Operate live runs

- [Live-run control plane](control-plane.md)
- [Run registry](registry.md)
- [Troubleshooting](troubleshooting.md)

# Integrate

- [Integration guide](integration.md)
- [Compatibility and upgrades](compatibility.md)
- [JSONL event schema](schema.md)
- [Exit-code contract](exit-codes.md)
- [Threat model](threat-model.md)

# Project

- [Architecture](architecture.md)
- [Architecture decision records](adr/README.md)
  - [Strict stream separation](adr/0001-strict-stream-separation.md)
  - [Redact argv by default](adr/0002-redact-argv-by-default.md)
  - [Keep control in the live runner](adr/0003-live-runner-control-plane.md)
  - [Scope cleanup to owned containers](adr/0004-container-scoped-cleanup.md)
  - [Keep command execution shell-free](adr/0005-shell-free-command-contract.md)
  - [Poll the registry for wait](adr/0006-registry-polled-wait.md)
  - [No terminal receipt file](adr/0007-no-terminal-receipt-file.md)
  - [Do not expose external PID adoption in the CLI](adr/0008-no-cli-external-pid-adoption.md)
- [Release process](release-process.md)
- [Roadmap](ROADMAP.md)
