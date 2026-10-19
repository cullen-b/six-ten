<p align="center">
  <img src="assets/six-ten-sign.svg" alt="six-ten: keeps your agents in their lanes" width="640">
</p>

# six-ten

**Keep your agents in their lanes.** Run as many coding agents as you like in one repo, or across a
whole folder of repos, without them stepping on each other's toes.

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
<sub>Live traffic report from `six-ten watch`: Claude Code, Codex and Hermes all merging onto one file at rush hour. Three clean commits, no pileups.</sub>

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
| **File leases** | The first agent to edit a file holds it. Others are refused with the holder's name and when it was last active, and are told to work on other files or call `six_ten_wait`. Races have exactly one winner. Claim with `-m "<reason>"` so blocked agents see the why. |
| **Agent-to-agent nudges** | six-ten finds each agent's T3 Code thread on its own (or the agent records it with `six_ten_contact`). A refusal names the holder's thread, so a blocked agent can ask it to release a lease that looks stale. An agent that ends its turn still blocked is sent back once to retry the file or say it's no longer needed. |
| **Decisions** | `six-ten decide "<text>"` records a decision (and `six_ten_decide` from any agent) in the event feed, so it outlives the chat session. |
| **Change notes** | When another agent changes a file you've read, you get a compact diff once: after your next tool call, at your next turn, or in your `six_ten_wait` result. |
| **Stale-write stop** | About to write a file that changed since you read it? That write is refused once with the diff, and you keep the lease while you re-read. The retry goes through. |
| **Shell coverage** | `sed -i`, `perl -pi`, `>`/`>>`, `tee`, `mv`, `cp`, `rm` and `apply_patch` heredocs count as edits, so a refusal can't be dodged through the shell. |
| **Git guard** | `stash`, `reset --hard`, `restore`, `clean`, `pull`, `merge` and `rebase` are refused while they would touch another agent's uncommitted work. Branch switches and new worktrees are always refused for agents. |
| **Wrap-up** | At the end of its turn an agent is sent back once to commit its own files. The last agent working is told to run `six-ten finish`, which merges into `main` and leaves the checkout there. The commit itself tells the last agent to finish before writing its final summary, so the summary isn't buried under a follow-up; an agent sent back anyway is told to repeat it. An agent waiting on a background task isn't sent back, since its turn resumes when the task ends. |
| **Commit guard** | A pre-commit check refuses commits that include files another agent is editing or has left uncommitted. |
| **Wake on reset** | `six_ten_wake` (CLI: `six-ten wake`) puts an agent to sleep until a time or until your Claude Code or Codex usage window resets, then tells it to start working. See [Starting work when your usage resets](#starting-work-when-your-usage-resets). |
| **Folders of repos** | An agent started in a folder that holds several repositories (`~/code`), or in a scratch directory, is still coordinated. Each edit is claimed in the repository it lands in, a shell command is checked in the repository it `cd`s into, and turn-end reminders and releases cover every repository the agent worked in. The MCP tools route the same way. |
| **Visibility** | `six-ten watch` shows a live feed of blocks, how they were resolved, and refusals. `six-ten stats` counts them: blocks and how they ended, refusals by kind, sessions, finishes, and the files agents collide on most. `six-ten notify on` sends desktop notifications. |

## Quick start

### Let your agent install it

Paste this into Claude Code, Codex, OpenCode or Hermes:

> Install six-ten for me by following https://raw.githubusercontent.com/cullen-b/six-ten/main/AGENTS.txt

[`AGENTS.txt`](AGENTS.txt) walks the agent through it: check prerequisites, build, ask you whether to
install machine-wide or for one repo, back up your configs, install, verify, and list the steps only
you can do. It also offers to update your agent instructions for a shared checkout.

### Or do it yourself

Requirements: macOS or Linux, git, and Rust 1.85 or newer (to build).

```sh
cargo install --locked --git https://github.com/cullen-b/six-ten
six-ten install all --global     # every repo on this machine, for every harness
```

This writes each harness's user-level config (`~/.claude`, `~/.codex`, `~/.config/opencode`,
`~/.hermes`). It keeps your existing hooks, settings and JSONC comments, and running it again changes
nothing. Back up those files first if you'd like a one-step undo.

For a single repo instead, run `six-ten install all` inside it and commit the generated files. Pick one
scope per harness; installing both makes each hook run twice.

The git commit guard installs itself: the first time an agent works in a repo, six-ten adds its check
to that repo's `pre-commit` hook, alongside any hook you already have. Repos that keep their hooks in
tracked files (Husky-style `core.hooksPath`) are left alone.

**The only steps left are the harnesses' own security prompts, which six-ten won't bypass:**

- **Codex:** start `codex` and run `/hooks` once to approve the six-ten hooks. Until then Codex skips
  them.
- **Claude Code, per-repo installs only:** open the repo interactively once and accept the trust
  prompt, or Claude Code ignores the project's permissions. Machine-wide installs don't need this.

Then start your agents as usual. There's nothing to run in the background.

```sh
six-ten watch            # live feed of what agents are doing
six-ten status           # who is editing what, uncommitted work, recent events
six-ten stats            # what six-ten has done: blocks, refusals, merges, most contended files
```

### Check that it works

```sh
claude mcp list; codex mcp list; opencode mcp list; hermes mcp list   # six-ten connected / enabled
SIX_TEN_AGENT=a six-ten claim README.md && SIX_TEN_AGENT=b six-ten claim README.md; echo "exit $?"   # exit 2
SIX_TEN_AGENT=a six-ten release
```

## Supported harnesses

| Harness | Enforcement | What `install` sets up | One-time step |
|---|---|---|---|
| **Claude Code** | Hooks (blocking) | `.claude/settings.json` hooks + tool permissions, `.mcp.json`, `CLAUDE.md` section | Open the repo interactively once and accept the trust prompt |
| **Codex** | Hooks (blocking) | `.codex/hooks.json`, MCP server and a sandbox profile in `.codex/config.toml`, `AGENTS.md` section | Trust the project and approve the hooks via `/hooks` |
| **OpenCode** | Plugin (blocking) | `.opencode/plugins/six-ten.ts`, MCP in `opencode.json`, `AGENTS.md` section | None |
| **Hermes** | Plugin (blocking) | Plugin in `~/.hermes/plugins/six-ten` (or `$HERMES_HOME`), enabled and MCP-registered through the `hermes` CLI | None. The plugin is global and does nothing outside git repos |
| **Anything with MCP** | Advisory | `six-ten install generic`: `AGENTS.md` section and pre-commit check; register `six-ten mcp` yourself | See [Adding a harness](#adding-a-harness) |
| **git** | Commit-time | `pre-commit` hook running `six-ten precommit` | None: it installs itself in each repo the first time an agent works there. It's the backstop for every harness |

Verified live with Claude Code 2.1.285, Codex 0.159, OpenCode 1.18 and Hermes 0.21, including all of
them running at once against one file. Sessions started in a folder of repos were verified live with
Claude Code 2.1.303.

**The Codex sandbox profile.** Codex keeps `.git` read-only, which stops agents from committing. The
installed `six-ten` profile makes `.git` writable but keeps `.git/hooks` and `.git/config` read-only.
Writing either of those would let an agent run code outside the sandbox. If you already set
`default_permissions`, the installer leaves it alone and prints the three rules to add to your
profile.

## Workflow

A checkout can only have one branch checked out, so agents sharing one can't each have a feature
branch. six-ten changes the workflow to fit:

- **One checkout, one branch per session.** `six-ten session` puts the checkout on
  `agents/<date>`. It's safe while others work: a new branch at the same commit doesn't touch
  anyone's files. If a branch is already checked out, it reuses it, so every agent ends up on the same
  one. Later sessions the same day are `agents/<date>.2`, `.3`, and so on. Branches are named by
  date because with many agents on different tasks, a topic name describes one agent's work at best;
  the merge commit lists what each session did instead. An agent that runs `git commit` on
  `main`/`master` is put on the session branch first; an agent commit that gets past the hooks is
  refused with a pointer to `six-ten session`. Your own commits aren't affected. Agents can't create
  or switch branches or add worktrees themselves
  (`git worktree add`, Claude Code's `EnterWorktree`). Opt out of session branches per repo with
  `git config six-ten.sessionBranches false`.
- **Every agent commits its own work before its turn ends.** When an agent's turn ends with
  uncommitted files that only it changed, six-ten sends it back once to commit them (Claude Code and
  Codex through their Stop hook, OpenCode by re-prompting the session). In Claude Code, a turn that
  ends while one of its background tasks is still running isn't the end of its work, so it isn't sent
  back. A commit that includes files
  another agent is still working on is refused. Once an agent's turn is over, anything it left
  uncommitted is anyone's to commit, with a `Co-edited-by:` trailer crediting it. Every agent commit
  gets an `Agent:` trailer, so history stays attributable and each piece can be reverted on its own.
- **The last agent out runs `six-ten finish`.** When the agent ending its turn is the only one left
  working, six-ten tells it to run `six-ten finish`, right after its commit so it can finish before
  writing its summary, and again at turn end if it didn't. That pushes the session branch, merges it into
  `main` with `--no-ff`, pushes `main`, and leaves the checkout on `main`. No PR. It does nothing while
  another agent is mid-turn, and refuses until every agent's work is committed. If the merge
  conflicts, it aborts and goes back to the session branch, so nothing is half-merged. It stops before
  touching anything if uncommitted changes nobody can be credited with would be overwritten, and asks
  for them to be committed or discarded. In a new repository whose first commits all went on the
  session branch, it creates `main` from that branch. The merge commit's message lists every commit
  in the session with the agent that made it, so `main`'s history reads as a per-session changelog:

  ```sh
  git log --first-parent main          # one merge per session, each listing its commits
  ```
- **Other agents' work in progress is visible.** A test can fail in a file another agent is halfway
  through. `six-ten status` shows which files those are, whether their agent is still working, and
  that they aren't yours to fix.
- **No worktrees.** Sharing one checkout is the point. An agent that finds itself in an existing
  worktree is told once to do new work in the main checkout. Commits there only count work in that
  worktree, but six-ten won't merge worktree branches.

These rules are part of the protocol section that `six-ten install` adds to your agents'
instructions (`CLAUDE.md`, `AGENTS.md`).

## Starting work when your usage resets

Want agents to start coding at 2am, when your 5-hour usage window resets? Give them the task and ask them
to wait for the reset:

> When my usage resets, fix the flaky auth tests. Use six_ten_wake.

The agent calls `six_ten_wake`, which blocks until then and returns:

```
six-ten wake-up: it is 2:20am; your claude usage window has reset. Start working now: fix the flaky auth tests
```

- **When to wake.** With no time given, six-ten wakes the agent a minute after its provider's usage resets. It
  reads the usage Claude Code caches in `~/.claude.json` and the rate limits Codex logs in
  `~/.codex/sessions`, with no network calls. If the weekly limit is used up, it waits for that reset instead.
  For other harnesses, or when there's no data, the agent is told to ask you for the time and pass `at`
  (`"2:30am"`, `"14:05"`, RFC 3339 or Unix seconds) or `minutes`. `six-ten usage` shows what six-ten sees.
- **No tokens while waiting.** It is one tool call: the model makes no requests until it returns. The call
  sends progress pings so the harness doesn't time out, and an interrupt cancels it. The sleeping agent's
  file claims are released, so commit before sleeping.
- **Harness timeouts.** Claude Code waits up to about 28 hours. OpenCode is fine as long as progress pings
  arrive. `install codex` sets `tool_timeout_sec = 86400` for six-ten (Codex's default is 60s). Hermes stops
  tool calls after 5 minutes, so add `timeout: 86400` under `mcp_servers.six-ten` in `~/.hermes/config.yaml`.
  Cursor stops every MCP call after 60 seconds, so for longer waits the tool hands a Cursor agent the
  equivalent `six-ten wake --at <time> '<task>'` command to run in its terminal, which blocks the same way.
  Register `six-ten mcp` in `~/.cursor/mcp.json` to give Cursor the tools. Waits are capped at 24 hours.

The agent has to make the call while it still has usage left. An agent that already hit its limit can't
call anything.

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
  `SIX_TEN_AGENT` to override the id. Codex's sandbox blocks `ps`, so commands there find their agent through
  `CODEX_SESSION_ID`, which Codex's hooks record.
- **Leases end on their own.** A lease is released at the end of the agent's turn, when its process
  exits (dead pids are ignored), or after 10 idle minutes (`SIX_TEN_TTL`, in seconds). A crashed agent
  never blocks anyone for long. If an agent stops mid-turn (say it hits its usage limit), a blocked agent
  asks you whether to release its files; with your OK it runs `six-ten release --agent <id>`, which frees
  them and ends that agent's turn, so its uncommitted work becomes anyone's to commit.
- **Fewer tool calls for hooked harnesses.** When a harness's hooks already claim edits, its MCP server
  hides `six_ten_claim` and tells the agent to just edit. In live runs this cut Codex from 4–6 six-ten
  calls per task to 1.
- **Works from anywhere.** Hooks and MCP calls find the repository from each file's path, not the
  agent's directory, so an agent started in `~/code` or a scratch directory is coordinated in every
  repo it touches. Turn-end reminders name the repo they're about.
- **Fail-open.** If six-ten errors, or the file isn't in any git repo, the edit goes ahead. A broken
  coordinator never wedges an agent.

## Token cost

Measured, at roughly 4 characters per token:

| | When | Tokens |
|---|---|---|
| Protocol section in `AGENTS.md` / `CLAUDE.md` | per session (usually cached) | ~600 |
| MCP instructions + tool definitions | per session (some harnesses load them on demand) | ~430–480 |
| Allowed edit | every tool call | **0**: hooks print nothing |
| "File is held" refusal | per block | ~70, plus one extra model round trip |
| Change note, one-line diff | once per change | ~60 (capped at ~1,500) |
| `six_ten_wait` | while blocked | nothing until it returns |

Without collisions, the whole cost is a few hundred cached tokens per session. When agents do
collide, a 60-token diff replaces re-reading a whole file, redoing clobbered work, or debugging a
broken merge.

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Codex edits aren't blocked | Its hooks aren't approved yet: run `/hooks` in Codex. A per-repo install also needs the project trusted |
| Codex can't commit (`.git/index.lock: Operation not permitted`) | The `six-ten` sandbox profile isn't active. If you set your own `default_permissions`, add the three `.git` rules the installer printed to that profile |
| Headless Claude (`claude -p`) can't call `six_ten_wait` | The workspace isn't trusted, so project permissions are ignored. Open it interactively once, or use `--global` |
| Hooks never fire | `six-ten` isn't on PATH for that app. Apps started from a GUI may not see `~/.cargo/bin` |
| An agent shows up as `python3:…` or `node:…` | Its launcher hides the harness name. Set `SIX_TEN_AGENT` for a stable id |
| `not inside a git repository` | A CLI command that needs one repo (`session`, `finish`, `decide`) ran in a folder of repos. Run it as `cd <repo> && six-ten …`; the error lists the candidates. MCP tools take a `repo` argument instead |
| `six-ten finish` says it "can't attribute" changes | Someone edited files outside six-ten's hooks, and merging would overwrite them. Commit or discard those files, then run it again |
| A lease is stuck | Leases expire after 10 idle minutes, and dead processes don't hold leases. `six-ten status` shows who holds what; `six-ten gc` cleans up |

## Uninstall

Restore your config backups (or delete the `six-ten` entries). Then delete
`~/.config/opencode/plugins/six-ten.ts`, `~/.hermes/plugins/six-ten/` and `~/.config/six-ten/`, and run
`claude mcp remove six-ten` and `hermes mcp remove six-ten`. In a repo, `.git/six-ten/` holds all of
six-ten's state and is safe to delete.

## Adding a harness

Any agent harness can use six-ten, at one of two levels. Start with `six-ten install generic`.

**1. MCP only (advisory).** Register `six-ten mcp` as a stdio MCP server. The agent gets
`six_ten_claim`, `six_ten_wait`, `six_ten_release`, `six_ten_status` and `six_ten_contact`, and instructions to claim
files before editing them. Paths route to their own repository, so the server works even when
launched outside one.

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
six-ten install <harness>        claude | codex | opencode | hermes | generic | git | all  [--global]
six-ten status                   active agents, leases, other agents' uncommitted work, recent events
six-ten session                  start (or reuse) today's session branch: agents/<date>, then .2, .3, ...
six-ten contact <thread> [--app A]   record your chat thread (default app: t3) so blocked agents can reach you
six-ten finish                   last agent out: merge the session branch into main, push, switch to main
six-ten watch [--history N]      live event feed
six-ten stats                    counts from the event log (in a repo, or each repo in a folder)
six-ten notify [on|off]          desktop notifications for this repo (off by default)
six-ten claim <paths>            claim manually; exit 2 with the reason if another agent holds any of them
six-ten claim <paths> -m "<why>"  claim with a reason blocked agents can see
six-ten decide "<text>"          record a decision in the event feed
six-ten wake [--at T | --minutes N] [--provider P] [task]   sleep until T, N minutes, or the usage reset
six-ten usage                    Claude Code's and Codex's last-reported usage and reset times
six-ten wait <paths> [--timeout S]
six-ten release [paths]          release yours (all of them if no paths are given)
six-ten release --agent <id>     release a stopped agent's files and end its turn (when the user asks)
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
  exception: it writes the plugin to your Hermes home. `wake` and `usage` also read the usage figures
  Claude Code caches in `~/.claude.json` and Codex logs in `~/.codex/sessions`. For agents started
  outside a repository, it lists the repositories each one worked in under `$TMPDIR/six-ten-agents/`,
  next to each agent's chat thread. To find a T3 Code thread it reads the first 8 KB of the logs in
  `~/.t3/userdata/logs/provider`, once per agent.
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

## Why "six-ten"?

It's named after Houston's **610 Loop**, the freeway ring around the city's core. Every lane is
always merging with every other one, and everybody is trying to get somewhere at the same time.
If you've run five agents in one repo, you've driven it.

| On the Loop | In your repo |
|---|---|
| One car per lane | One agent per file (a lease) |
| Merge ahead: yield | Blocked: work elsewhere, or `six_ten_wait` |
| Traffic alert: lane shift ahead | Change note: someone changed a file you read |
| Nobody closes the freeway at rush hour | No `git stash` / `reset --hard` while others are driving |
| The morning traffic report | `six-ten watch` |
| Inside the Loop | Inside `.git/six-ten/`: no server, no daemon |

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your
option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion
in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
