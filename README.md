# AgentLatch

Lightweight coordination and collision prevention for coding agents working in the same workspace.

AgentLatch is a small Rust CLI that prevents multiple coding-agent tasks from mutating the same files at the same time. It uses local project state, has no daemon or database, and keeps coordination logic inside the tool instead of making agents reason about lock internals.

## Why AgentLatch?

Running several agent chats against one codebase is useful until two tasks reach the same file at once. AgentLatch gives those tasks a shared view of file ownership so they can avoid destructive races.

When a file is busy, the intended behavior is:

1. Do other independent work from the user's request first.
2. Re-read the blocked file before retrying because another task may have changed it.
3. If no independent work remains, wait for the claim to become available and continue afterward.

The goal is safe concurrency without stopping an entire task just because one file is temporarily busy.

## Current status

AgentLatch is an early preview, but the core coordination path is implemented:

- Rust CLI executable named `agentlatch`
- idempotent project initialization
- local `agent_latch.json` state owned exclusively by AgentLatch
- automatic `/agent_latch.json` entry in `.gitignore`
- managed AgentLatch instructions in `AGENTS.md`
- Codex hook installation and reconciliation
- atomic file-claim acquisition and release
- conflict detection between concurrent tasks
- optional waiting for busy files
- stale-claim recovery through expiring leases
- physical-path identity across symlinks and Windows junctions
- cleanup on post-tool, stop, interrupt, subagent-stop, and session-end hooks

## Build

```bash
cargo build --release
```

The executable is produced at:

```text
target/release/agentlatch
```

Make sure `agentlatch` is available on `PATH` before configuring an agent integration.

## Quick start

Initialize a project:

```bash
cd D:\Projects\Moonbase
agentlatch init
```

Then configure Codex once for the current user:

```bash
agentlatch configure codex
```

Codex may ask you to review or trust newly installed hooks. Open `/hooks` in Codex after configuration when required.

After that, normal Codex file coordination is automatic inside initialized AgentLatch projects.

Claude and other coding-agent hosts can use the same initialized project through manual coordination. Give each worker one stable owner id, lock the relevant paths before mutation, and unlock them when that mutation scope finishes. AgentLatch currently installs automatic hooks only for Codex.

## Commands

### `agentlatch init [path]`

Initialize the current project or an optional project directory.

```bash
agentlatch init
agentlatch init D:\Projects\Moonbase
```

`init` is idempotent. Running it again updates only AgentLatch-managed project integration:

- creates `agent_latch.json` only when it is missing
- ensures `/agent_latch.json` appears once in `.gitignore`
- creates or updates the managed AgentLatch section in `AGENTS.md`
- preserves everything outside that managed section

It also reports whether the Codex integration is currently configured.

### `agentlatch configure codex`

Install or update the global Codex hooks used by AgentLatch.

```bash
agentlatch configure codex
```

The command is safe to run repeatedly. AgentLatch owns only its marked section in the Codex configuration and preserves unrelated configuration.

### `agentlatch configure all`

Configure every AgentLatch integration supported by the installed version.

```bash
agentlatch configure all
```

At the moment, Codex is the supported integration. The command exists so future integrations can be added without changing the setup workflow.

### `agentlatch status`

Show active claims in the current AgentLatch project.

```bash
agentlatch status
```

This is primarily a diagnostic command. Agents should not inspect `agent_latch.json` directly.

### `agentlatch wait [--timeout <seconds>] <paths...>`

Wait until one or more busy paths become available.

```bash
agentlatch wait src\renderer.rs
agentlatch wait --timeout 120 src\renderer.rs src\assets.rs
```

Codex normally reaches this command only after it has exhausted other independent work that can be completed while a conflicting claim is active.

### Manual coordination

Automatic hooks are the normal path for Codex. Claude and hosts without an AgentLatch hook integration must use `lock` and `unlock` explicitly around mutations; the same commands remain available for diagnostics and deliberate reservations on Codex.

```bash
agentlatch lock --owner maintenance --reason "Updating generated bindings" generated\bindings.rs
agentlatch unlock --owner maintenance generated\bindings.rs
```

Release every manual claim owned by the same owner:

```bash
agentlatch unlock --owner maintenance --mine
```

Manual locks may also use `--wait <seconds>` and `--ttl <seconds>` when a custom wait or lease duration is useful.

Lock targets must be concrete file or directory paths. Directory claims apply recursively to their descendants. Wildcard/glob targets such as `*` or `*.rs`, and the AgentLatch project root itself, are rejected so a claim can never silently become a whole-project or pattern-wide lock. Automatic hooks also block potentially mutating calls when they cannot determine a concrete target instead of falling back to a broad claim.

Shell integration is deliberately conservative about what it claims: AgentLatch reacts to shell mutations it can positively identify, but does not treat an unknown command or script as mutating merely because its behavior is opaque. This keeps unrelated tools and read-only project scripts from being blocked by AgentLatch. Commands that mutate through opaque custom scripts should acquire explicit AgentLatch claims for the concrete files or directories they change.

Codex shell hooks identify commands at shell token boundaries and extract concrete file targets from common filesystem and Git working-tree operations. Quoted search patterns are treated as data. Git staging and commits do not require working-tree file claims because they normally update Git metadata; custom Git hooks that edit source files still need explicit coordination. Commands with unresolvable targets or complex shell substitutions are rejected rather than implicitly claiming the whole project. This parser intentionally supports common command forms, not every Bash or PowerShell construct.

## Project state

`agent_latch.json` is private runtime state owned completely by AgentLatch. Coding agents should never read, edit, replace, delete, or derive protocol decisions from it directly.

The schema is intentionally internal and may change between AgentLatch versions without changing the public CLI.

## `AGENTS.md` integration

AgentLatch manages only a marked section of `AGENTS.md`:

```md
<!-- agentlatch:start -->
...AgentLatch instructions...
<!-- agentlatch:end -->
```

Everything outside those markers belongs to the project and is preserved. Running `agentlatch init` after upgrading AgentLatch refreshes the managed section when the bundled instructions change.

## Physical path identity

Claims follow the physical filesystem target rather than only the path spelling used by an agent.

For example, imagine these two fictional projects:

```text
D:\Projects\OrionEngine

D:\Projects\SkyHarbor\ThirdParty\OrionEngine
    -> junction -> D:\Projects\OrionEngine
```

If one task claims:

```text
D:\Projects\OrionEngine\src\renderer.rs
```

and another task reaches the same file through:

```text
D:\Projects\SkyHarbor\ThirdParty\OrionEngine\src\renderer.rs
```

AgentLatch resolves both paths to the same physical project and treats them as the same resource.

## Safety model

AgentLatch serializes state changes with a small operating-system file mutex stored in the temporary directory, then writes project state atomically. Claims use leases so an abandoned task cannot block a file forever.

For automatic Codex coordination, claims are acquired immediately before a potentially mutating tool call and released after that tool call completes. If a conflict is found, the mutation is denied before it runs and Codex receives instructions to continue independent work or wait and retry safely.

If AgentLatch cannot read or update its own coordination state, mutation hooks fail closed rather than allowing an uncoordinated write.
