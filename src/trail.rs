use anyhow::Result;

use crate::agent::Agent;
use crate::git;
use crate::store::Store;

/// The repo's curated picture of where the software stands; agents rewrite it, never append.
pub const STATE_FILE: &str = "STATE.md";

/// Decisions recorded since the last finish, as `- text [agent]` lines.
pub fn pending_decisions(store: &Store) -> String {
    let events = store.events();
    let since = events
        .iter()
        .rposition(|e| e.kind == "finish")
        .map_or(0, |i| i + 1);
    events[since..]
        .iter()
        .filter(|e| e.kind == "decision")
        .map(|e| format!("- {} [{}]\n", e.text, e.agent))
        .collect()
}

/// The session's commits, as `- subject [agent]` lines.
pub fn session_commits(store: &Store, default: &str, branch: &str) -> String {
    git::run(
        store.root(),
        &[
            "log",
            "--reverse",
            "--format=%s%x1f%(trailers:key=Agent,valueonly,separator=%x2C )",
            &format!("{default}..{branch}"),
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|l| {
        let (subject, agent) = l.split_once('\x1f')?;
        Some(if agent.is_empty() {
            format!("- {subject}\n")
        } else {
            format!("- {subject} [{agent}]\n")
        })
    })
    .collect()
}

/// Body of a session merge: its commits, then the decisions recorded along the way.
pub fn merge_body(store: &Store, default: &str, branch: &str) -> String {
    let mut body = session_commits(store, default, branch);
    let decisions = pending_decisions(store);
    if !decisions.is_empty() {
        body.push_str(&format!("\nDecisions:\n{decisions}"));
    }
    body
}

/// What happened here: the session in progress, then the default branch's history, newest first.
pub fn log_text(store: &Store, limit: usize) -> Result<String> {
    let root = store.root();
    let default = git::default_branch(root);
    let mut out = String::new();
    if root.join(STATE_FILE).exists() {
        out.push_str(&format!(
            "Current state: read {STATE_FILE}. This log is how it got there.\n\n"
        ));
    }
    if let Some(branch) = git::current_branch(root).filter(|b| *b != default) {
        let commits = session_commits(store, &default, &branch);
        let decisions = pending_decisions(store);
        if !commits.is_empty() || !decisions.is_empty() {
            out.push_str(&format!(
                "in progress on `{branch}` (not merged yet):\n{commits}"
            ));
            if !decisions.is_empty() {
                out.push_str(&format!("Decisions:\n{decisions}"));
            }
            out.push('\n');
        }
    }
    if !git::branch_exists(root, &default) {
        out.push_str(&format!("`{default}` has no commits yet\n"));
        return Ok(out);
    }
    let history = git::run(
        root,
        &[
            "log",
            "--first-parent",
            &format!("-n{limit}"),
            "--date=short",
            "--format=%x1e%ad %h %s%x1f%b",
            &default,
        ],
    )?;
    out.push_str(&format!("`{default}`, newest first:\n"));
    for entry in history.split('\x1e').filter(|e| !e.trim().is_empty()) {
        let (head, body) = entry.split_once('\x1f').unwrap_or((entry, ""));
        out.push_str(head.trim());
        out.push('\n');
        // Session merges carry their commits and decisions; other commit bodies stay in git log.
        if head.contains(" Merge agents/") {
            for line in body.lines().filter(|l| !l.trim().is_empty()) {
                if line.starts_with("- ") || line == "Decisions:" {
                    out.push_str(&format!("    {line}\n"));
                }
            }
        }
    }
    Ok(out)
}

/// Once per session: an agent that committed here but left STATE.md untouched is asked to update it.
pub fn state_reminder(store: &Store, agent: &Agent) -> Option<String> {
    let root = store.root();
    if !root.join(STATE_FILE).exists() {
        return None;
    }
    let default = git::default_branch(root);
    let branch = git::current_branch(root).filter(|b| *b != default)?;
    let range = format!("{default}..{branch}");
    let authors = git::run(
        root,
        &["log", "--format=%(trailers:key=Agent,valueonly)", &range],
    )
    .ok()?;
    if !authors.lines().any(|a| a.trim() == agent.id) {
        return None;
    }
    let touched = git::run(
        root,
        &["log", "--format=", "--name-only", &range, "--", STATE_FILE],
    )
    .ok()?;
    if !touched.trim().is_empty() {
        return None;
    }
    Some(format!(
        "six-ten: you committed on `{branch}`, but nobody has updated {STATE_FILE} this session. It's the short, \
         current picture the next agent reads first: what works, what's in progress, what's known broken, what \
         not to try. Rewrite the parts your work changed (replace, don't append) and commit it. Keep its layout: \
         `## Works`, `## In progress`, `## Known broken`, `## Don't try` (each with its reason), one short bullet \
         per fact, under ~40 lines, current facts only. If your work changed nothing there, say so in your \
         summary instead."
    ))
}
