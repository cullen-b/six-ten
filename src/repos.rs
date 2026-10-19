use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::store::{Store, hex_sha, resolve};

/// The nearest existing directory at or above `path`, which may not exist yet.
fn existing_dir(path: &Path) -> PathBuf {
    let mut dir = resolve(path);
    while !dir.is_dir() && dir.pop() {}
    dir
}

/// Opens the store of the repository containing `path`, which may not exist yet.
pub fn open_at(path: &Path) -> Result<Store> {
    Store::open(&existing_dir(path))
}

/// Groups `raw` paths (relative to `cwd`) by the repository each lies in, so sessions started in a
/// folder of repositories still coordinate. Paths outside every repository are dropped.
pub fn by_repo(cwd: &Path, raw: &[String]) -> Vec<(Store, Vec<String>)> {
    let mut groups: Vec<(Store, Vec<String>)> = Vec::new();
    let mut outside: Vec<PathBuf> = Vec::new();
    for p in raw {
        if let Some((_, paths)) = groups
            .iter_mut()
            .find(|(s, _)| s.normalize(cwd, p).is_some())
        {
            paths.push(p.clone());
            continue;
        }
        let dir = existing_dir(&cwd.join(p));
        if outside.contains(&dir) {
            continue;
        }
        match Store::open(&dir) {
            Ok(store) if store.normalize(cwd, p).is_some() => groups.push((store, vec![p.clone()])),
            Ok(_) => {}
            Err(_) => outside.push(dir),
        }
    }
    groups
}

/// Where the repositories an agent has worked in are listed; subagents share their parent's list.
fn registry(agent_id: &str) -> PathBuf {
    let base = agent_id.split('/').next().unwrap_or(agent_id);
    std::env::temp_dir()
        .join("six-ten-agents")
        .join(hex_sha(base))
}

/// Records that `agent_id` works in the repository at `root`.
pub fn remember(agent_id: &str, root: &Path) -> Result<()> {
    let file = registry(agent_id);
    let text = fs::read_to_string(&file).unwrap_or_default();
    let line = root.to_string_lossy();
    if text.lines().any(|l| l == line) {
        return Ok(());
    }
    // Repositories deleted since (scratch repos, mostly) are dropped as the list is rewritten.
    let kept: String = text
        .lines()
        .filter(|l| Path::new(l).is_dir())
        .map(|l| format!("{l}\n"))
        .collect();
    fs::create_dir_all(file.parent().expect("registry has a parent"))?;
    fs::write(&file, format!("{kept}{line}\n"))?;
    Ok(())
}

/// Every repository `agent_id` has worked in.
pub fn known(agent_id: &str) -> Vec<Store> {
    let text = fs::read_to_string(registry(agent_id)).unwrap_or_default();
    text.lines()
        .map(PathBuf::from)
        .filter_map(|root| Store::open(&root).ok().filter(|s| s.root() == root))
        .collect()
}

/// The repository at `cwd`, or, for a session started outside one (a folder of repositories, a
/// scratch directory), every repository the agent has worked in.
pub fn here_or_known(cwd: &Path, agent_id: &str) -> Vec<Store> {
    match Store::open(cwd) {
        Ok(store) => vec![store],
        Err(_) => known(agent_id),
    }
}

/// The repository at `cwd`, or, in a folder of repositories, each one six-ten has worked in.
pub fn here_or_nearby(cwd: &Path) -> Vec<Store> {
    if let Ok(store) = Store::open(cwd) {
        return vec![store];
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(cwd)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs.iter()
        .filter(|d| d.join(".git/six-ten").is_dir())
        .filter_map(|d| Store::open(d).ok())
        .collect()
}

/// The error for a call that needs one repository but ran outside any, naming the ones nearby;
/// `how` tells the caller how to name the repository it means.
pub fn not_a_repo(cwd: &Path, agent_id: &str, how: &str) -> anyhow::Error {
    let mut near: Vec<String> = known(agent_id)
        .iter()
        .map(|s| s.root().display().to_string())
        .collect();
    if let Ok(entries) = fs::read_dir(cwd) {
        for e in entries.flatten() {
            let path = e.path();
            if path.join(".git").exists() {
                near.push(path.display().to_string());
            }
        }
    }
    near.sort();
    near.dedup();
    let hint = if near.is_empty() {
        String::new()
    } else {
        format!(
            " Repositories here or that you worked in: {}.",
            near.join(", ")
        )
    };
    anyhow!(
        "not inside a git repository: {}. {how}{hint}",
        cwd.display()
    )
}
