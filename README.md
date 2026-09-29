# six-ten

Run as many coding agents as you like in one git checkout without them stepping on each other.

six-ten is a single Rust binary that every agent harness calls before it edits a file. The first agent
to touch a file gets a lease; any other agent that tries to edit it is told who holds it and to work
on other files, or to block on `six_ten_wait` until it is free. It also stops agents from running git
commands that would wipe out each other's uncommitted work, and from committing each other's files.

Works with **Claude Code**, **Codex**, **OpenCode** and **Hermes**, plus anything that speaks MCP.

## Install

```sh
cargo install --path .          # puts `six-ten` on PATH (~/.cargo/bin)
cd /path/to/your/repo
six-ten install all             # or: claude | codex | opencode | hermes | git
```

| harness | what `install` writes | notes |
|---|---|---|
| Claude Code | `.claude/settings.json` hooks + permissions, `.mcp.json`, `CLAUDE.md` section | Trust the folder once interactively, or project permissions are ignored |
| Codex | `.codex/hooks.json`, `.codex/config.toml` MCP, `AGENTS.md` section | Codex loads project config only for trusted projects; approve the hooks once via `/hooks` |
| OpenCode | `.opencode/plugins/six-ten.ts`, `opencode.json` MCP, `AGENTS.md` section | |
| Hermes | `AGENTS.md` section; prints a snippet for `~/.hermes/config.yaml` | Hermes config is global only |
| git | `pre-commit` hook running `six-ten precommit` | Backstop for every harness |

Commit the generated files so every agent (and teammate) gets them.

## How it works

- **Leases.** A `PreToolUse`-style hook claims a file before each edit. Leases live as one JSON file
  per path in `<git-common-dir>/six-ten/locks/`, never committed and shared by all worktrees.
  Changes are serialized with an `flock`, so exactly one agent wins a race.
- **Identity.** An agent is its harness process (`claude:73273`, `opencode:26642/ses_…`). Hooks, CLI
  calls and the MCP server all run as children of that process, so they agree on who you are without
  any configuration. Subagents get their own id (`claude:73273/<agent_id>`); a parent never blocks its
  own subagents. Set `SIX_TEN_AGENT` to override.
- **Release.** Leases are released at the end of each turn (`Stop`/`session.idle`/`on_session_end`),
  when the agent exits (dead pids are ignored), or after 10 idle minutes (`SIX_TEN_TTL`, seconds).
- **Shell workarounds.** `sed -i`, `perl -pi`, `>`/`>>` redirects, `tee`, `mv`, `cp`, `rm` and
  `apply_patch` heredocs count as edits of their target files.
- **Uncommitted work.** six-ten remembers which live agent edited which dirty file. While another
  agent has uncommitted work, `git stash`, `reset --hard`, `checkout`/`switch`, `restore`, `clean`,
  `pull`, `merge` and `rebase` are blocked if they would touch it, and `six-ten precommit` refuses
  commits that include it (`SIX_TEN_ALLOW_COMMIT=1` to override).
- **Stale context.** Every completed write goes into a journal (`journal.jsonl`, contents kept as git
  blobs), and six-ten tracks what each agent last read. When another agent changes a file you have
  read, you are told once, with a compact diff:
  - right after your next tool call (Claude Code/Codex `additionalContext`, appended to the tool
    result in OpenCode), at your next turn start, or in a `six_ten_wait` result;
  - if you are about to write that very file, the write is refused once with the diff (the lease is
    kept), so you re-read before overwriting; the retry goes through;
  - files others changed during your turn that you never read are listed by name.
  Hermes shell hooks can't annotate a call, so there the next write is refused once with the note.
- **Fail open.** If six-ten itself errors, or the directory isn't a git repo, the edit is allowed.

## CLI

```
six-ten claim <paths>          # exit 2 + reason if another agent holds any of them
six-ten wait <paths> [--timeout 300]
six-ten release [paths]        # all of yours when no paths are given
six-ten status                 # who is editing what, and whose uncommitted work is where
six-ten whoami | gc | mcp | hook <harness> | precommit | install <harness>
```

MCP tools: `six_ten_claim`, `six_ten_wait`, `six_ten_release`, `six_ten_status`.

## Limits

- Detecting shell writes is best-effort. A script that writes files (`python gen.py`) isn't seen until
  commit time, where the pre-commit check still catches collisions.
- Stale-context notes cover files an agent has read or written; a change to a file it only depends on
  indirectly (never opened) shows up by name only. Codex reads are inferred from shell commands.
- Agent identity is the harness process, so a resumed session (`--resume`) starts with a fresh view.
- macOS and Linux only (uses `ps` and `flock`).

## Development

```sh
cargo test      # unit tests + end-to-end tests that drive the real binary in temp git repos
```
