use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::agent::Agent;
use crate::git::{self, Clobber};
use crate::journal::StaleFile;
use crate::store::{Claim, Lease, Store, now};

pub enum Decision {
    Allow,
    /// Allowed; tell the agent this first.
    Note(String),
    /// Lease granted, but the target changed since the agent last saw it: refuse this write once.
    Stale(String),
    /// Held by another agent, or otherwise refused.
    Deny(String),
}

impl Decision {
    /// Text for CLI/MCP callers, where a granted claim is a success even if it carries news.
    pub fn granted_text(self, ok: String) -> Result<String, String> {
        match self {
            Decision::Allow => Ok(ok),
            Decision::Note(n) | Decision::Stale(n) => Ok(format!("{ok}\n\n{n}")),
            Decision::Deny(reason) => Err(reason),
        }
    }
}

const FILE_DIFF_LINES: usize = 80;
const NOTE_CHARS: usize = 6000;

pub fn ttl() -> u64 {
    std::env::var("SIX_TEN_TTL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
}

/// Claims `raw_paths` for `agent` before an edit; paths outside the repo are ignored.
pub fn pre_edit(
    store: &Store,
    agent: &Agent,
    cwd: &Path,
    raw_paths: &[String],
) -> Result<Decision> {
    pre_edit_with_reason(store, agent, cwd, raw_paths, None)
}

/// Claims `raw_paths`, attaching `reason` so blocked agents see why the files are held.
pub fn pre_edit_with_reason(
    store: &Store,
    agent: &Agent,
    cwd: &Path,
    raw_paths: &[String],
    reason: Option<&str>,
) -> Result<Decision> {
    let paths = normalize_all(store, cwd, raw_paths);
    if paths.is_empty() {
        return Ok(Decision::Allow);
    }
    match store.claim_with_reason(agent, &paths, ttl(), reason)? {
        Claim::Granted => {
            store.note_granted(&agent.id, &paths)?;
            let dirty = git::dirty(store.root(), &paths)?;
            let clean: Vec<String> = paths
                .iter()
                .filter(|p| !dirty.contains(*p))
                .cloned()
                .collect();
            store.touch(agent, &paths, &clean)?;
            catch_up(store, agent, &paths)
        }
        Claim::Conflict(leases) => {
            store.note_blocked(&agent.id, &leases)?;
            Ok(Decision::Deny(conflict_message(&leases)))
        }
    }
}

/// Tells `agent` what other agents changed in files it has seen, before it writes `targets`.
fn catch_up(store: &Store, agent: &Agent, targets: &[String]) -> Result<Decision> {
    let stale = store.take_stale(&agent.id, targets)?;
    if stale.is_empty() {
        return Ok(Decision::Allow);
    }
    let mut out = String::new();
    for t in &stale.targets {
        let by: Vec<&str> = t.by.iter().map(String::as_str).collect();
        store.note_stale(&agent.id, &t.path, &by.join(", "))?;
    }
    if !stale.targets.is_empty() {
        let names: Vec<String> = stale
            .targets
            .iter()
            .map(|t| format!("`{}`", t.path))
            .collect();
        out.push_str(&format!(
            "six-ten: you now hold {}, but other agents changed it since you last saw it, so this write was \
             stopped once. Your lease is kept. Re-read what you need, adjust your edit to the current content, \
             and retry.\n",
            names.join(", ")
        ));
        render(store, &stale.targets, &mut out);
    }
    if !stale.seen.is_empty() {
        out.push_str(
            "six-ten: other agents changed files you read earlier. Check that your plan still fits \
             (signatures, names, behaviour) before continuing:\n",
        );
        render(store, &stale.seen, &mut out);
    }
    if !stale.unseen.is_empty() {
        out.push_str(&format!(
            "Also changed by other agents during your turn: {}\n",
            stale.unseen.join(", ")
        ));
    }
    let out = out.trim_end().to_string();
    Ok(if stale.targets.is_empty() {
        Decision::Note(out)
    } else {
        Decision::Stale(out)
    })
}

/// Appends per-file diffs, smallest first, until the note budget runs out.
fn render(store: &Store, files: &[StaleFile], out: &mut String) {
    let mut diffs: Vec<(String, String)> = files
        .iter()
        .map(|f| {
            let by: Vec<&str> = f.by.iter().map(String::as_str).collect();
            let head = format!("--- {} (changed by {})", f.path, by.join(", "));
            let body = match &f.seen {
                None => "you have not read it yet; read it before writing.".to_string(),
                Some(seen) => git::diff_blobs(
                    store.root(),
                    seen.as_deref(),
                    f.now.as_deref(),
                    FILE_DIFF_LINES,
                ),
            };
            (head, body)
        })
        .collect();
    diffs.sort_by_key(|(_, body)| body.len());
    let mut skipped = Vec::new();
    for (head, body) in diffs {
        if out.len() + head.len() + body.len() > NOTE_CHARS {
            skipped.push(head.trim_start_matches("--- ").to_string());
            continue;
        }
        out.push_str(&format!("{head}\n{body}\n"));
    }
    if !skipped.is_empty() {
        out.push_str(&format!(
            "Also changed (re-read them): {}\n",
            skipped.join("; ")
        ));
    }
}

/// Records completed writes so other agents can be told about them.
pub fn post_write(store: &Store, agent: &Agent, cwd: &Path, raw_paths: &[String]) -> Result<()> {
    for path in normalize_all(store, cwd, raw_paths) {
        store.record_write(&agent.id, &path, git::blob(store.root(), &path))?;
    }
    Ok(())
}

/// Records what the agent has read, as the baseline for later change notes.
pub fn post_read(store: &Store, agent: &Agent, cwd: &Path, raw_paths: &[String]) -> Result<()> {
    for path in normalize_all(store, cwd, raw_paths) {
        if store.root().join(&path).is_file() {
            store.record_read(&agent.id, &path, git::blob(store.root(), &path))?;
        }
    }
    Ok(())
}

/// Changes to files the agent has seen that it has not been told about yet.
pub fn news(store: &Store, agent: &Agent) -> Result<Decision> {
    with_worktree_warning(store, agent, catch_up(store, agent, &[])?)
}

/// Start of a turn: report what changed since the last one, then reset the turn cursor.
pub fn turn_start(store: &Store, agent: &Agent) -> Result<Decision> {
    let decision = catch_up(store, agent, &[])?;
    store.record_turn_start(&agent.id)?;
    with_worktree_warning(store, agent, decision)
}

/// Tells an agent working in a linked worktree, once, that its work belongs in the main checkout.
fn with_worktree_warning(store: &Store, agent: &Agent, decision: Decision) -> Result<Decision> {
    let Some(main) = store.main_worktree() else {
        return Ok(decision);
    };
    let branch = git::current_branch(store.root()).unwrap_or_else(|| "this branch".into());
    let warning = format!(
        "six-ten: you are in a separate worktree ({}). Agents in this repo share the main checkout ({}) \
         and its branch, so do new work there. To wrap up what's here, commit your files on `{branch}` \
         and tell the user that branch needs merging; six-ten doesn't merge worktree branches.",
        store.root().display(),
        main.display()
    );
    if !store.first_reminder(&format!("{}#worktree", agent.id), &warning)? {
        return Ok(decision);
    }
    Ok(match decision {
        Decision::Allow => Decision::Note(warning),
        Decision::Note(n) => Decision::Note(format!("{warning}\n\n{n}")),
        other => other,
    })
}

/// Blocks shell commands that would discard or rewrite other agents' uncommitted work.
pub fn pre_shell(store: &Store, agent: &Agent, cwd: &Path, command: &str) -> Result<Decision> {
    let clobbers = git::clobbers(command);
    if clobbers.is_empty() {
        return Ok(Decision::Allow);
    }
    if let Some(Clobber::Worktree(label)) =
        clobbers.iter().find(|c| matches!(c, Clobber::Worktree(_)))
    {
        store.log(
            &agent.id,
            "refused",
            format!("stopped from running `{label}`: agents share one checkout"),
        )?;
        return Ok(Decision::Deny(
            "six-ten: agents don't create worktrees here. Every agent shares this checkout and its branch, \
             and six-ten keeps you from colliding: edit files right here and commit your own files on the \
             session branch (run `six-ten session <topic>` first if you're on the default branch)."
                .into(),
        ));
    }
    let branch = clobbers.iter().find_map(|c| match c {
        Clobber::Branch(label) => Some(label),
        _ => None,
    });
    if let Some(label) = branch.filter(|_| git::session_branches_enabled(store.root())) {
        store.log(
            &agent.id,
            "refused",
            format!("stopped from running `{label}`: agents share the session branch"),
        )?;
        let default = git::default_branch(store.root());
        return Ok(Decision::Deny(format!(
            "six-ten: agents don't create or switch branches; every agent shares this checkout's branch. \
             On `{default}`, run `six-ten session <topic>` before your first commit. To wrap up, commit your \
             own files; the last agent working runs `six-ten finish`, which merges into `{default}` and \
             switches back."
        )));
    }
    let leases = store.others(&agent.id)?;
    let dirty = git::dirty(store.root(), &[])?;
    let touched = store.touched_by_others(&agent.id, &dirty)?;
    let active = store.active_others(&agent.id)?;
    let mut hits: Vec<(String, String)> = Vec::new();
    for clobber in &clobbers {
        let (label, affected): (&str, Option<HashSet<String>>) = match clobber {
            Clobber::Tree(label) => (label, None),
            Clobber::Branch(label) => {
                // Even an agent that hasn't edited yet would find itself on another branch.
                for p in &active {
                    let ago = human(now().saturating_sub(p.at));
                    hits.push((label.clone(), format!("{} (active {ago} ago)", p.agent)));
                }
                (label, None)
            }
            Clobber::Paths(label, raw) => (label, Some(normalize_prefixes(store, cwd, raw))),
            Clobber::Worktree(label) => (label, None),
        };
        let hit = |p: &str| {
            affected.as_ref().is_none_or(|set| {
                set.iter()
                    .any(|a| p == a || p.starts_with(&format!("{a}/")))
            })
        };
        for l in leases.iter().filter(|l| hit(&l.path)) {
            hits.push((
                label.to_string(),
                format!("{} ({}, being edited)", l.path, l.agent),
            ));
        }
        for t in &touched {
            for p in t.paths.iter().filter(|p| hit(p)) {
                hits.push((label.to_string(), format!("{p} ({}, uncommitted)", t.agent)));
            }
        }
    }
    if hits.is_empty() {
        return Ok(Decision::Allow);
    }
    hits.sort();
    hits.dedup();
    let labels: Vec<&str> = {
        let mut l: Vec<&str> = hits.iter().map(|(l, _)| l.as_str()).collect();
        l.dedup();
        l
    };
    let files: Vec<&str> = hits.iter().map(|(_, f)| f.as_str()).take(12).collect();
    store.log(
        &agent.id,
        "refused",
        format!(
            "stopped from running `{}`; it would rewrite: {}",
            labels.join("`, `"),
            files.join(", ")
        ),
    )?;
    Ok(Decision::Deny(format!(
        "six-ten: blocked `{}` because other agents are working in this checkout and it would discard or rewrite \
         their changes:\n  {}\nDo not stash, reset, restore, clean, switch branches or pull while they are active \
         (to start a session branch, run `six-ten session <topic>`, which is safe while others work). \
         Operate only on your own files: discard yours by path, or `git add <your files>` and commit. To get \
         back to the default branch, the last agent working runs `six-ten finish`.",
        labels.join("`, `"),
        files.join("\n  ")
    )))
}

/// Rejects a commit that sweeps in files other live agents are editing or have left uncommitted.
pub fn pre_commit(store: &Store, agent: &Agent) -> Result<Decision> {
    let overridden = std::env::var("SIX_TEN_ALLOW_COMMIT").is_ok_and(|v| v == "1");
    let is_agent = store.is_hooked(crate::store::harness_of(&agent.id));
    let branch = git::current_branch(store.root());
    if !overridden
        && is_agent
        && git::session_branches_enabled(store.root())
        && branch
            .as_deref()
            .is_some_and(|b| git::is_default_branch(store.root(), b))
    {
        let b = branch.unwrap_or_default();
        store.log(
            &agent.id,
            "refused",
            format!("commit on {b} refused: agents commit on a session branch"),
        )?;
        return Ok(Decision::Deny(format!(
            "six-ten: agents don't commit on `{b}`. Run `six-ten session <topic>` first: it starts today's session \
             branch, or reuses the one another agent already started, without touching anyone's files. Then \
             commit again."
        )));
    }
    let staged: HashSet<String> = git::staged(store.root())?.into_iter().collect();
    let mine = store.touched_by(&agent.id)?;
    let mut hits: Vec<String> = store
        .others(&agent.id)?
        .into_iter()
        .filter(|l| staged.contains(&l.path) && l.root.as_ref().is_none_or(|r| r == store.root()))
        .map(|l| {
            if mine.contains(&l.path) {
                format!("{} (you both edited it; {} is still working, so commit it after its turn ends)", l.path, l.agent)
            } else {
                format!("{} (being edited by {})", l.path, l.agent)
            }
        })
        .collect();
    // Work left by agents whose turn has ended is anyone's to commit.
    let working = working_ids(store, agent)?;
    for t in store.touched_by_others(&agent.id, &staged)? {
        if !working.contains(&t.agent) {
            continue;
        }
        hits.extend(t.paths.iter().filter(|p| !mine.contains(*p)).map(|p| {
            format!(
                "{p} (uncommitted work of {}, which is still working and will commit it)",
                t.agent
            )
        }));
    }
    if hits.is_empty() {
        return Ok(Decision::Allow);
    }
    hits.sort();
    hits.dedup();
    if overridden {
        let text = format!(
            "committed other agents' work with SIX_TEN_ALLOW_COMMIT=1: {}",
            hits.join(", ")
        );
        store.log(&agent.id, "override", text)?;
        return Ok(Decision::Allow);
    }
    store.log(
        &agent.id,
        "refused",
        format!("commit refused, it included: {}", hits.join(", ")),
    )?;
    Ok(Decision::Deny(format!(
        "six-ten: commit blocked, it includes other agents' work in progress:\n  {}\nUnstage those \
         (`git restore --staged <path>`) and commit the rest of your files now. The agents working on them commit \
         them; a file you both edited is yours to commit once the other agent's turn ends.",
        hits.join("\n  ")
    )))
}

/// Puts this checkout on a session branch: creates `agents/<date>-<topic>` when on the default
/// branch, or reports the branch already in use. Serialized, so racing agents share one branch.
pub fn session(store: &Store, agent: &Agent, topic: Option<&str>) -> Result<String> {
    store.locked(|| {
        let root = store.root();
        let current =
            git::current_branch(root).context("HEAD is detached; check out a branch first")?;
        if !git::is_default_branch(root, &current) {
            return Ok(format!(
                "session branch: {current} (already checked out; commit here)"
            ));
        }
        let date = std::process::Command::new("date").arg("+%F").output()?;
        let date = String::from_utf8_lossy(&date.stdout).trim().to_string();
        let slug: String = topic
            .unwrap_or("work")
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug: String = if slug.is_empty() {
            "work".into()
        } else {
            slug.chars().take(40).collect()
        };
        // Later sessions of the day continue the first one's name: `<first>.2`, `<first>.3`, ...
        let today = git::branches_with_prefix(root, &format!("agents/{date}-"));
        let base = today
            .first()
            .map(|b| {
                b.rsplit_once('.')
                    .filter(|(_, n)| n.parse::<u32>().is_ok())
                    .map_or(b.as_str(), |(s, _)| s)
            })
            .map_or_else(|| format!("agents/{date}-{slug}"), String::from);
        let mut name = base.clone();
        let mut n = 2;
        while git::branch_exists(root, &name) {
            name = format!("{base}.{n}");
            n += 1;
        }
        git::create_branch(root, &name)?;
        store.log(
            &agent.id,
            "session",
            format!("started session branch {name}"),
        )?;
        Ok(format!(
            "started session branch {name}; every agent in this checkout now commits here"
        ))
    })
}

/// Trailers for a commit by `agent`: who made it, and who else edited its files.
pub fn commit_trailers(store: &Store, agent: &Agent) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // Only agents get an `Agent:` trailer; a human's shell isn't a hooked harness.
    if store.is_hooked(crate::store::harness_of(&agent.id)) {
        out.push(format!("Agent: {}", agent.id));
    }
    let staged: HashSet<String> = git::staged(store.root())?.into_iter().collect();
    for t in store.touched_by_others(&agent.id, &staged)? {
        out.push(format!("Co-edited-by: {}", t.agent));
    }
    out.dedup();
    Ok(out)
}

/// Releases everything held by `agent` (and its subagents).
pub fn end(store: &Store, agent: &Agent) -> Result<Vec<String>> {
    store.note_end(&agent.id)?;
    store.mark_ended(&agent.id)?;
    store.release(&agent.id, None)
}

/// Ids of other agents in the middle of a turn.
fn working_ids(store: &Store, agent: &Agent) -> Result<HashSet<String>> {
    Ok(store
        .working_others(&agent.id, ttl())?
        .into_iter()
        .map(|p| p.agent)
        .collect())
}

/// End of the agent's turn: once per situation, sends it back to commit its files, or to run
/// `six-ten finish` when it is the last agent working. `Allow` ends the turn.
pub fn stop(store: &Store, agent: &Agent) -> Result<Decision> {
    if store.main_worktree().is_some() {
        return end(store, agent).map(|_| Decision::Allow);
    }
    let reminder = stop_reminder(store, agent)?;
    match reminder {
        Some(text) if store.first_reminder(&agent.id, &text)? => {
            store.log(
                &agent.id,
                "reminded",
                text.lines().next().unwrap_or_default().into(),
            )?;
            Ok(Decision::Deny(text))
        }
        _ => end(store, agent).map(|_| Decision::Allow),
    }
}

fn stop_reminder(store: &Store, agent: &Agent) -> Result<Option<String>> {
    let root = store.root();
    let dirty = git::dirty(root, &[])?;
    let working = working_ids(store, agent)?;
    let others = store.touched_by_others(&agent.id, &dirty)?;
    let held_by_working = |p: &String| {
        others
            .iter()
            .any(|t| working.contains(&t.agent) && t.paths.contains(p))
    };
    let mut mine: Vec<String> = store
        .touched_by(&agent.id)?
        .into_iter()
        .filter(|p| dirty.contains(p) && !held_by_working(p))
        .collect();
    let branch = git::current_branch(root);
    let default = git::default_branch(root);
    let on_default = branch
        .as_deref()
        .is_some_and(|b| git::is_default_branch(root, b));
    if !mine.is_empty() {
        mine.sort();
        let session = if on_default && git::session_branches_enabled(root) {
            "Run `six-ten session <topic>` first, then commit"
        } else {
            "Commit them"
        };
        return Ok(Some(format!(
            "six-ten: before you finish, commit your work. These files have uncommitted changes from you and no \
             other agent is working on them:\n  {}\n{session} with a message that describes the change: \
             `git add <paths>` and `git commit`. Leave out anything you don't want kept, and discard it by path.",
            mine.join("\n  ")
        )));
    }
    if !working.is_empty() {
        return Ok(None);
    }
    let Some(branch) = branch.filter(|b| b.starts_with("agents/")) else {
        return Ok(None);
    };
    let leftover: Vec<String> = others
        .iter()
        .flat_map(|t| t.paths.iter().map(move |p| format!("{p} ({})", t.agent)))
        .collect();
    let ahead = git::ahead(root, &default, &branch);
    if ahead == 0 && leftover.is_empty() {
        return Ok(None);
    }
    let pushes = if git::has_remote(root, "origin") {
        ", pushes,"
    } else {
        ""
    };
    let mut text = format!(
        "six-ten: you're the last agent working. Run `six-ten finish`: it merges `{branch}` ({ahead} commit(s)) \
         into `{default}`{pushes} and leaves the checkout on `{default}`."
    );
    if !leftover.is_empty() {
        text.push_str(&format!(
            " First commit what finished agents left uncommitted (their turns are over, so it's yours to commit; \
             they're credited automatically):\n  {}",
            leftover.join("\n  ")
        ));
    }
    Ok(Some(text))
}

/// Merges the session branch into the default branch and leaves the checkout there. Only the last
/// agent working may do this, and only once every agent's work is committed.
pub fn finish(store: &Store, agent: &Agent) -> Result<Decision> {
    let root = store.root();
    if store.main_worktree().is_some() {
        return Ok(Decision::Deny(
            "six-ten: `finish` runs in the main checkout; this is a separate worktree. Commit here and tell the \
             user which branch needs merging."
                .into(),
        ));
    }
    let default = git::default_branch(root);
    let branch = git::current_branch(root).context("HEAD is detached; nothing to finish")?;
    if branch == default {
        return Ok(Decision::Note(format!(
            "already on `{default}`; nothing to finish"
        )));
    }
    let working: Vec<String> = working_ids(store, agent)?.into_iter().collect();
    if !working.is_empty() {
        return Ok(Decision::Note(format!(
            "not finishing yet: {} still working. Whichever agent finishes last merges `{branch}`; there's \
             nothing more for you to do.",
            working.join(", ")
        )));
    }
    let dirty = git::dirty(root, &[])?;
    let mut owned: Vec<String> = store
        .touched_by(&agent.id)?
        .into_iter()
        .filter(|p| dirty.contains(p))
        .map(|p| format!("{p} (yours)"))
        .collect();
    for t in store.touched_by_others(&agent.id, &dirty)? {
        owned.extend(
            t.paths
                .iter()
                .map(|p| format!("{p} (left by {}, whose turn is over)", t.agent)),
        );
    }
    if !owned.is_empty() {
        owned.sort();
        return Ok(Decision::Deny(format!(
            "six-ten: commit these first, then run `six-ten finish` again (other agents' leftovers are yours to \
             commit; they're credited automatically):\n  {}",
            owned.join("\n  ")
        )));
    }
    let ahead = git::ahead(root, &default, &branch);
    let remote = git::has_remote(root, "origin");
    let mut report = Vec::new();
    if remote && ahead > 0 {
        match git::run(root, &["push", "-q", "-u", "origin", &branch]) {
            Ok(_) => report.push(format!("pushed `{branch}`")),
            Err(e) => report.push(format!("could not push `{branch}` ({e:#})")),
        }
    }
    git::run(root, &["switch", "-q", &default])?;
    if ahead > 0 {
        let msg = format!("Merge {branch}");
        if let Err(e) = git::run(root, &["merge", "-q", "--no-ff", "-m", &msg, &branch]) {
            let conflicts =
                git::run(root, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
            let _ = git::run(root, &["merge", "--abort"]);
            git::run(root, &["switch", "-q", &branch])?;
            return Ok(Decision::Deny(format!(
                "six-ten: merging `{branch}` into `{default}` failed, so nothing changed and you're back on \
                 `{branch}`. Conflicting files: {}. Merge `{default}` into `{branch}` (`git merge {default}`), \
                 resolve and commit, then run `six-ten finish` again.\n({e:#})",
                if conflicts.is_empty() {
                    "none listed".into()
                } else {
                    conflicts.replace('\n', ", ")
                }
            )));
        }
        report.insert(
            0,
            format!("merged `{branch}` ({ahead} commit(s)) into `{default}`"),
        );
        if remote {
            match git::run(root, &["push", "-q", "origin", &default]) {
                Ok(_) => report.push(format!("pushed `{default}`")),
                Err(e) => report.push(format!(
                    "could not push `{default}` ({e:#}); push it when you can"
                )),
            }
        }
    } else {
        report.push(format!("`{branch}` had nothing new"));
    }
    if !remote {
        report.push("no `origin` remote, so nothing was pushed".into());
    }
    report.push(format!("the checkout is on `{default}`"));
    let text = report.join("; ");
    store.log(&agent.id, "finish", text.clone())?;
    Ok(Decision::Note(text))
}

pub fn wait(
    store: &Store,
    agent: &Agent,
    cwd: &Path,
    raw_paths: &[String],
    timeout: Duration,
) -> Result<Decision> {
    let paths = normalize_all(store, cwd, raw_paths);
    if let Claim::Conflict(leases) = store.claim(agent, &paths, ttl())? {
        store.note_blocked(&agent.id, &leases)?;
        store.note_waiting(&agent.id, &paths)?;
    }
    match store.wait(agent, &paths, ttl(), timeout)? {
        Claim::Granted => {
            store.note_granted(&agent.id, &paths)?;
            catch_up(store, agent, &paths)
        }
        Claim::Conflict(leases) => Ok(Decision::Deny(format!(
            "still busy after {}s. {}",
            timeout.as_secs(),
            conflict_message(&leases)
        ))),
    }
}

pub fn conflict_message(leases: &[Lease]) -> String {
    let now = now();
    let held: Vec<String> = leases
        .iter()
        .map(|l| {
            let why = l
                .reason
                .as_deref()
                .map_or(String::new(), |r| format!(": {r}"));
            format!(
                "`{}` is being edited by {} (lease expires in {}){why}",
                l.path,
                l.agent,
                human(l.expires_at.saturating_sub(now)),
            )
        })
        .collect();
    format!(
        "six-ten: blocked, {}. Do not edit it now and do not work around this with shell commands. Work on other \
         files first and retry this edit later, or call the six_ten_wait tool (CLI: `six-ten wait <path>`) to block \
         until it is free.",
        held.join("; ")
    )
}

pub fn human(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m", secs.div_ceil(60))
    } else {
        format!("{secs}s")
    }
}

fn normalize_all(store: &Store, cwd: &Path, raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = raw.iter().filter_map(|p| store.normalize(cwd, p)).collect();
    out.sort();
    out.dedup();
    out
}

fn normalize_prefixes(store: &Store, cwd: &Path, raw: &[String]) -> HashSet<String> {
    raw.iter()
        .filter_map(|p| store.normalize(cwd, p.trim_end_matches('/')))
        .collect()
}
