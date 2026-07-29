# Agent skills

`using-processkit-cli/` is an installable skill for agents that launch external
commands. It teaches fail-closed preflight, contained execution, JSONL outcome
handling, and detached supervision without duplicating the full documentation.

For Codex, link or copy the skill folder into the personal skill directory:

```sh
mkdir -p "${CODEX_HOME:-$HOME/.codex}/skills"
ln -s "$(pwd)/skills/using-processkit-cli" \
  "${CODEX_HOME:-$HOME/.codex}/skills/using-processkit-cli"
```

On Windows, create the equivalent directory junction or copy the folder into
`$env:CODEX_HOME\skills` (default: `$HOME\.codex\skills`). Restart the harness so
it rediscovers installed skills.

Claude Code users can add this repository as a plugin marketplace and install the
same skill:

```text
/plugin marketplace add ZelAnton/ProcessKit-CLI
/plugin install using-processkit-cli@processkit-cli-skills
```

The skill's factual CLI tokens and exit-code claims are checked against the built
binary and Rust constants by `tests/agent_skill.rs`. Its Markdown links are part of
the ordinary documentation link gate.
