# six-ten

**Run as many coding agents as you like in one repo, without them stepping on each other's toes.**

six-ten is a small Rust binary that sits between your agents and your working tree. Before an agent
edits a file, six-ten checks whether another agent is already editing it. If it is, the agent is told
who has the file and goes to work on something else, or waits. Agents are also told when someone else
changes a file they've read, and they can't wipe out or commit each other's uncommitted work.

It works across harnesses and model providers at the same time: **Claude Code, Codex, OpenCode and
Hermes** are supported out of the box, and anything with MCP or tool hooks can be added.

```
⏸ codex:97632   blocked on src/shared.rs (held by claude:96994)
⏸ hermes:97635  blocked on src/shared.rs (held by claude:96994)
✅ hermes:97635  got src/shared.rs after 10s (claude:96994 held it): waited with six_ten_wait
✅ codex:97632   got src/shared.rs after 57s (claude:96994 held it): edited 1 other file meanwhile, waited with six_ten_wait
🔄 codex:97632   stopped from overwriting src/shared.rs: hermes:97635 changed it since it was read
✅ codex:97632   re-read src/shared.rs and retried after hermes:97635's change
```
<sub>A real `six-ten watch` feed: Claude Code, Codex and Hermes editing one file at once. Three clean commits, nothing lost.</sub>

## Why

Several agents in one checkout go wrong in predictable ways:

- **Clobbered edits.** Two agents write the same file, and the second one overwrites the first.
- **Stale plans.** Agent A reads `api.rs`, agent B changes a signature in it, and A writes code
  against the old one.
- **Destroyed work.** One agent runs `git stash`, `checkout .` or `reset --hard` and takes everyone
  else's uncommitted changes with it.
- **Tangled commits.** `git add -A` sweeps another agent's half-finished files into your commit.

Separate worktrees avoid all of this by giving up on sharing a checkout. six-ten lets agents share one.

## What it does

| | |
|---|---|
| **File leases** | The first agent to edit a file holds it. Others are refused with the holder's name, and are told to work on other files or call `six_ten_wait`. Races have exactly one winner. |
| **Change notes** | When another agent changes a file you've read, you get a compact diff once: after your next tool call, at your next turn, or in your `six_ten_wait` result. |
| **Stale-write stop** | About to write a file that changed since you read it? That write is refused once with the diff, and you keep the lease while you re-read. The retry goes through. |
| **Shell coverage** | `sed -i`, `perl -pi`, `>`/`>>`, `tee`, `mv`, `cp`, `rm` and `apply_patch` heredocs count as edits, so a refusal can't be dodged through the shell. |
| **Git guard** | `stash`, `reset --hard`, `checkout`/`switch`, `restore`, `clean`, `pull`, `merge` and `rebase` are refused while they would touch another agent's uncommitted work. |
| **Commit guard** | A pre-commit check refuses commits that include files another agent is editing or has left uncommitted. |
| **Visibility** | `six-ten watch` shows a live feed of blocks, how they were resolved, and refusals. `six-ten notify on` sends desktop notifications. |

## Quick start

Requirements: macOS or Linux, git, and Rust 1.85 or newer (to build).

```sh
cargo install --git https://github.com/cullen-b/six-ten   # or: git clone … && cargo install --path .
cd your-repo
six-ten install all      # or pick harnesses: claude | codex | opencode | hermes | generic | git
git add -A && git commit -m "Add six-ten"
```

Then start your agents as usual. There's nothing to run in the background.

```sh
six-ten watch            # optional: live feed of what agents are doing
six-ten status           # who is editing what, uncommitted work, recent events
```

## Supported harnesses

| Harness | Enforcement | What `install` sets up | One-time step |
|---|---|---|---|
| **Claude Code** | Hooks (blocking) | `.claude/settings.json` hooks + tool permissions, `.mcp.json`, `CLAUDE.md` section | Open the repo interactively once and accept the trust prompt |
| **Codex** | Hooks (blocking) | `.codex/hooks.json`, MCP server and a sandbox profile in `.codex/config.toml`, `AGENTS.md` section | Trust the project and approve the hooks via `/hooks` |
| **OpenCode** | Plugin (blocking) | `.opencode/plugins/six-ten.ts`, MCP in `opencode.json`, `AGENTS.md` section | None |
| **Hermes** | Plugin (blocking) | Plugin in `~/.hermes/plugins/six-ten` (or `$HERMES_HOME`), enabled and MCP-registered through the `hermes` CLI | None. The plugin is global and does nothing outside git repos |
| **Anything with MCP** | Advisory | `six-ten install generic`: `AGENTS.md` section and pre-commit check; register `six-ten mcp` yourself | See [Adding a harness](#adding-a-harness) |
| **git** | Commit-time | `pre-commit` hook running `six-ten precommit` | Installed by `all`. It's the backstop for every harness |

Verified live with Claude Code 2.1.285, Codex 0.159, OpenCode 1.18 and Hermes 0.21, including all of
them running at once against one file.

**The Codex sandbox profile.** Codex keeps `.git` read-only, which stops agents from committing. The
installed `six-ten` profile makes `.git` writable but keeps `.git/hooks` and `.git/config` read-only.
Writing either of those would let an agent run code outside the sandbox. If you already set
`default_permissions`, the installer leaves it alone and prints the three rules to add to your
profile.

## What agents see

When the file is held by another agent:

```
six-ten: blocked, `src/shared.rs` is being edited by claude:20992 (lease expires in 10m). Do not edit it now
and do not work around this with shell commands. Work on other files first and retry this edit later, or call
the six_ten_wait tool (CLI: `six-ten wait <path>`) to block until it is free.
```

When another agent changed a file this agent read earlier:

```
six-ten: other agents changed files you read earlier. Check that your plan still fits (signatures, names,
behaviour) before continuing:
--- src/api.rs (changed by codex:14281)
@@ -1 +1 @@
-pub fn greet(name: &str) -> String {
+pub fn greet(name: &str, excited: bool) -> String {
```

Each note is delivered once. Your own edits and your subagents' edits are never reported back to you.
Large diffs are summarised ("changed substantially (+300/-1 lines); re-read it"), and a note is capped
at about 1,500 tokens.

## How it works

- **No server.** Every harness hook calls the `six-ten` binary, which exits in about 70ms. State lives
  in `<git-common-dir>/six-ten/`, so it's never committed and is shared by all worktrees. It consists of
  one JSON lease file per path, a change journal (file contents are kept as git blobs for diffing), a
  record of what each agent has read, and an event log. Every change to that state takes an `flock`
  first.
- **Identity without configuration.** An agent is its harness process: `claude:73273`, `codex:22055`,
  `hermes:97635/<session>`. Hooks, CLI calls and the MCP server all run as children of that process,
  so they agree on who's who. Subagents and sessions get their own id. A parent's lease never blocks
  its own subagents, and the git and commit checks treat a parent and its subagents as one agent. Set
  `SIX_TEN_AGENT` to override the id.
- **Leases end on their own.** A lease is released at the end of the agent's turn, when its process
  exits (dead pids are ignored), or after 10 idle minutes (`SIX_TEN_TTL`, in seconds). A crashed agent
  never blocks anyone for long.
- **Fewer tool calls for hooked harnesses.** When a harness's hooks already claim edits, its MCP server
  hides `six_ten_claim` and tells the agent to just edit. In live runs this cut Codex from 4–6 six-ten
  calls per task to 1.
- **Fail-open.** If six-ten errors, or the directory isn't a git repo, the edit goes ahead. A broken
  coordinator never wedges an agent.

## Token cost

Measured, at roughly 4 characters per token:

| | When | Tokens |
|---|---|---|
| Protocol section in `AGENTS.md` / `CLAUDE.md` | per session (usually cached) | ~230 |
| MCP instructions + tool definitions | per session (some harnesses load them on demand) | ~370–420 |
| Allowed edit | every tool call | **0**: hooks print nothing |
| "File is held" refusal | per block | ~70, plus one extra model round trip |
| Change note, one-line diff | once per change | ~60 (capped at ~1,500) |
| `six_ten_wait` | while blocked | nothing until it returns |

Without collisions, the whole cost is a few hundred cached tokens per session. When agents do
collide, a 60-token diff replaces re-reading a whole file, redoing clobbered work, or debugging a
broken merge.

## Adding a harness

Any agent harness can use six-ten, at one of two levels. Start with `six-ten install generic`.

**1. MCP only (advisory).** Register `six-ten mcp` as a stdio MCP server. The agent gets
`six_ten_claim`, `six_ten_wait`, `six_ten_release` and `six_ten_status`, and instructions to claim
files before editing them.

**2. Hooks (enforced).** If the harness can run a command before and after tool calls, pipe JSON to
`six-ten hook generic`:

| `event` | Send when | Fields |
|---|---|---|
| `pre_edit` | a tool is about to write files | `paths` |
| `pre_shell` | a shell command is about to run | `command` |
| `post_write`, `post_read` | a tool wrote or read files | `paths` |
| `post_shell` | a shell command finished | `command` |
| `turn_start` | a new user message arrives | |
| `end` | the agent's turn or session ends | |

Every payload also carries `cwd`, and `sub` to tell subagents or sessions apart.

The reply tells you what to do:
- **Exit code 2:** refuse the tool call and show stderr to the model.
- **stdout `{"note": "…"}`:** allow the call and add the note to the model's context.
- **Anything else:** allow the call.

`pre_edit` and `end` are the minimum; the other events enable change notes. Run `six-ten` as a direct
child of the harness process, because that's how agents are identified.

```python
def six_ten(event, **fields):
    p = subprocess.run(["six-ten", "hook", "generic"], capture_output=True, text=True,
                       input=json.dumps({"event": event, "cwd": os.getcwd(), **fields}))
    if p.returncode == 2:
        raise ToolRefused(p.stderr)                    # show to the model, skip the call
    return json.loads(p.stdout or "{}").get("note")    # add to the model's context if set
```

Complete examples: [`integrations/opencode/six-ten.ts`](integrations/opencode/six-ten.ts) (TypeScript)
and [`integrations/hermes/six-ten/`](integrations/hermes/six-ten/__init__.py) (Python). Use
`six-ten watch` to confirm that blocks and releases show up. PRs adding native adapters are welcome.

## Reference

```
six-ten install <harness>        claude | codex | opencode | hermes | generic | git | all
six-ten status                   leases, other agents' uncommitted work, recent events
six-ten watch [--history N]      live event feed
six-ten notify [on|off]          desktop notifications for this repo (off by default)
six-ten claim <paths>            claim manually; exit 2 with the reason if another agent holds any of them
six-ten wait <paths> [--timeout S]
six-ten release [paths]          release yours (all of them if no paths are given)
six-ten whoami | gc              show your agent id | prune stale state older than a day
six-ten mcp | hook <harness> | precommit    used by integrations
```

| Environment variable | Effect |
|---|---|
| `SIX_TEN_AGENT` | Override the agent id (scripts, humans) |
| `SIX_TEN_TTL` | Idle lease lifetime in seconds (default 600) |
| `SIX_TEN_ALLOW_COMMIT=1` | Human override for the commit check. Agents are never told about it, and every use is logged |
| `HERMES_HOME` | Where `install hermes` puts the plugin |

## Security

- Everything is local. six-ten makes no network calls and runs no daemon.
- It only reads and writes inside the repository and its `.git/six-ten/`. `install hermes` is the
  exception: it writes the plugin to your Hermes home.
- The Codex profile widens the sandbox to `.git` only. Hooks and config stay read-only.
- Hooks fail open by design. six-ten is a coordination tool for cooperating agents, not a security
  boundary against a hostile one.

## Limitations

- Shell-write detection is best-effort. A script that writes files (`python gen.py`) isn't seen until
  commit time, where the pre-commit check still catches collisions.
- Change notes cover files an agent read or wrote. A change to a file it never opened is listed by
  name only. Codex has no read tool, so its reads are inferred from shell commands (`cat`, `sed -n`,
  `rg`, …).
- A resumed session (`--resume`) is a new process, so it starts with a fresh view of what it has read.
- six-ten prevents clobbered files, not semantic conflicts: two agents editing different files can
  still break each other's build.
- macOS and Linux only for now (uses `ps` and `flock`).

## Development

```sh
cargo test     # unit tests + end-to-end tests that drive the real binary against temp git repos
```

The e2e suite covers lease races (16 processes, one winner), expiry, waiting, every harness's hook
payloads, the git and commit guards, stale-write and change-note delivery, the MCP protocol, and the
installers.
