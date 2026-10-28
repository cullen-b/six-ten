# State of six-ten

The current picture for agents starting here; `six-ten log` shows how it got here. Rewrite lines your
work changes (replace, don't append); keep it under ~40 lines.

## Works
- File leases, refusals with reasons, waiting, agent-to-agent nudges (T3 threads, Claude Code peers).
- Change notes and stale-write stops; shell writes (`sed -i`, `>`, `mv`, `apply_patch`, ...) count as edits.
- Session branches `agents/<date>` (`.2`, `.3`), Stop-hook commit reminders, `six-ten finish` merging
  into `main` with the session's commits and decisions in the merge body.
- `six-ten log` trail; one STATE.md reminder per agent per session (after its commit, else at turn end).
- Folder-of-repos sessions: hooks and MCP route each path to its own repo.
- `six-ten wake` / `six_ten_wake` until a time or usage reset; works outside a repo.
- Live-verified: Claude Code, Codex, OpenCode 1.x and 2.x (plugin serves V1 and V2 APIs), Hermes, T3 Code.

## In progress
- Nothing open.

## Known broken / limits
- Hermes has no turn-end hook: its leftovers get committed by other agents.
- Codex, OpenCode and Hermes have no peer-messaging channel; stalled holders there need the user's OK
  to release (`six-ten release --agent`).
- Codex/Hermes wake not live-tested (Codex account out of usage until 2026-11-10).
- Third-party OpenCode V1 npm plugins fail to load on OpenCode 2 (not ours to fix).

## Don't try
- Worktrees or per-agent branches: one shared checkout is the point; six-ten refuses them.
- Topic names in session branches: the user found them useless with many agents.
- Blocking Stop hooks for routine nudges in T3 Code: any block shows "Stop hook error"; nudge after
  `git commit` (PostToolUse) instead.
