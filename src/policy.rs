use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

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
    let paths = normalize_all(store, cwd, raw_paths);
    if paths.is_empty() {
        return Ok(Decision::Allow);
    }
    match store.claim(agent, &paths, ttl())? {
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
    catch_up(store, agent, &[])
}

/// Start of a turn: report what changed since the last one, then reset the turn cursor.
pub fn turn_start(store: &Store, agent: &Agent) -> Result<Decision> {
    let decision = catch_up(store, agent, &[])?;
    store.record_turn_start(&agent.id)?;
    Ok(decision)
}

/// Blocks shell commands that would discard or rewrite other agents' uncommitted work.
pub fn pre_shell(store: &Store, agent: &Agent, cwd: &Path, command: &str) -> Result<Decision> {
    let clobbers = git::clobbers(command);
    if clobbers.is_empty() {
        return Ok(Decision::Allow);
    }
    let leases = store.others(&agent.id)?;
    let dirty = git::dirty(store.root(), &[])?;
    let touched = store.touched_by_others(&agent.id, &dirty)?;
    let mut hits: Vec<(String, String)> = Vec::new();
    for clobber in &clobbers {
        let (label, affected): (&str, Option<HashSet<String>>) = match clobber {
            Clobber::Tree(label) => (label, None),
            Clobber::Paths(label, raw) => (label, Some(normalize_prefixes(store, cwd, raw))),
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
         their changes:\n  {}\nDo not stash, reset, restore, clean, switch branches or pull while they are active. \
         Operate only on your own files (e.g. `git add <your files>` then `git commit`), or ask the user.",
        labels.join("`, `"),
        files.join("\n  ")
    )))
}

/// Rejects a commit that sweeps in files other live agents are editing or have left uncommitted.
pub fn pre_commit(store: &Store, agent: &Agent) -> Result<Decision> {
    let overridden = std::env::var("SIX_TEN_ALLOW_COMMIT").is_ok_and(|v| v == "1");
    let staged: HashSet<String> = git::staged(store.root())?.into_iter().collect();
    let mine = store.touched_by(&agent.id)?;
    let mut hits: Vec<String> = store
        .others(&agent.id)?
        .into_iter()
        .filter(|l| staged.contains(&l.path))
        .map(|l| {
            if mine.contains(&l.path) {
                format!("{} (you both edited it; {} is still working, so commit it after its turn ends)", l.path, l.agent)
            } else {
                format!("{} (being edited by {})", l.path, l.agent)
            }
        })
        .collect();
    for t in store.touched_by_others(&agent.id, &staged)? {
        hits.extend(
            t.paths
                .iter()
                .filter(|p| !mine.contains(*p))
                .map(|p| format!("{p} (uncommitted work of {})", t.agent)),
        );
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
        "six-ten: commit blocked, it includes other agents' work:\n  {}\nUnstage those (`git restore --staged <path>`) \
         and commit the rest of your files now. If they belong in this commit, ask the user.",
        hits.join("\n  ")
    )))
}

/// Trailers for a commit by `agent`: who made it, and who else edited its files.
pub fn commit_trailers(store: &Store, agent: &Agent) -> Result<Vec<String>> {
    let mut out = Vec::new();
    // Only agents get an `Agent:` trailer; a human's shell isn't a hooked harness.
    if store.is_hooked(crate::store::harness_of(&agent.id)) {
        out.push(format!("Agent: {}", agent.id));
    }
    let staged: HashSet<String> = git::staged(store.root())?.into_iter().collect();
    let mine = store.touched_by(&agent.id)?;
    for t in store.touched_by_others(&agent.id, &staged)? {
        if t.paths.iter().any(|p| mine.contains(p)) {
            out.push(format!("Co-edited-by: {}", t.agent));
        }
    }
    out.dedup();
    Ok(out)
}

/// Releases everything held by `agent` (and its subagents).
pub fn end(store: &Store, agent: &Agent) -> Result<Vec<String>> {
    store.note_end(&agent.id)?;
    store.release(&agent.id, None)
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
            format!(
                "`{}` is being edited by {} (lease expires in {})",
                l.path,
                l.agent,
                human(l.expires_at.saturating_sub(now))
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
