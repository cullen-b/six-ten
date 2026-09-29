use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::agent::Agent;
use crate::git::{self, Clobber};
use crate::store::{Claim, Lease, Store, now};

pub enum Decision {
    Allow,
    Deny(String),
}

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
            let dirty = git::dirty(store.root(), &paths)?;
            let clean: Vec<String> = paths
                .iter()
                .filter(|p| !dirty.contains(*p))
                .cloned()
                .collect();
            store.touch(agent, &paths, &clean)?;
            Ok(Decision::Allow)
        }
        Claim::Conflict(leases) => Ok(Decision::Deny(conflict_message(&leases))),
    }
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
    if std::env::var("SIX_TEN_ALLOW_COMMIT").is_ok_and(|v| v == "1") {
        return Ok(Decision::Allow);
    }
    let staged: HashSet<String> = git::staged(store.root())?.into_iter().collect();
    let mine = store.touched_by(&agent.id)?;
    let mut hits: Vec<String> = store
        .others(&agent.id)?
        .into_iter()
        .filter(|l| staged.contains(&l.path))
        .map(|l| format!("{} (being edited by {})", l.path, l.agent))
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
    Ok(Decision::Deny(format!(
        "six-ten: commit blocked, it includes other agents' work:\n  {}\nUnstage them (`git restore --staged <path>`) \
         and commit only the files you changed. Set SIX_TEN_ALLOW_COMMIT=1 if the user wants them committed together.",
        hits.join("\n  ")
    )))
}

/// Releases everything held by `agent` (and its subagents).
pub fn end(store: &Store, agent: &Agent) -> Result<Vec<String>> {
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
    match store.wait(agent, &paths, ttl(), timeout)? {
        Claim::Granted => Ok(Decision::Allow),
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
