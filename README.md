# six-ten

File-lease coordinator that lets multiple coding agents (Claude Code, Codex, OpenCode, Hermes, …)
work in one repo without clobbering each other's edits.

Before an agent edits a file it claims a lease from six-ten. If another agent holds the file, the
edit is refused with the holder's name so the agent can work on other files, or block on `wait`.

Status: work in progress.
