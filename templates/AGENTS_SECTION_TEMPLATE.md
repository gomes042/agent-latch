## AgentLatch

This project uses AgentLatch to coordinate file access between coding agents.

- Do not read, edit, or rewrite `agent_latch.json` directly. AgentLatch owns this file completely.
- Codex coordination is automatic after `agentlatch configure codex`.
- On Claude or another host without AgentLatch hook integration, use explicit `agentlatch lock --owner <agent-id> <paths...>` before mutating files and `agentlatch unlock --owner <agent-id> <paths...>` when that mutation scope is finished. Keep one stable owner id per worker and claim only the relevant paths.
- Never bypass or disable an AgentLatch hook or skip required manual claims to force an edit through.
- If AgentLatch reports that another task owns a file, do other independent work from the user's request first. Do not wait while independent requested work remains.
- Before retrying a previously blocked file, re-read it because another task may have changed it.
- Only when no independent work remains, use `agentlatch wait <path>`, re-read the file after the wait, and retry after the claim becomes available.
- Use the `agentlatch` CLI only for explicit/manual coordination or diagnostics.
