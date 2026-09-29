use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::{Agent, pid_alive};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lease {
    pub path: String,
    pub agent: String,
    pub pid: Option<u32>,
    pub acquired_at: u64,
    pub expires_at: u64,
}

/// Files an agent has edited; kept past its leases so its uncommitted work stays attributable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Touched {
    pub agent: String,
    pub pid: Option<u32>,
    pub paths: BTreeSet<String>,
}

#[derive(Debug, PartialEq)]
pub enum Claim {
    Granted,
    Conflict(Vec<Lease>),
}

/// Lease store for one repository: one JSON file per leased path, mutations serialized by an flock.
pub struct Store {
    root: PathBuf,
    pub(crate) dir: PathBuf,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Store {
    /// Opens the store of the git repository containing `cwd`, shared by all of its worktrees.
    pub fn open(cwd: &Path) -> Result<Store> {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--show-toplevel", "--git-common-dir"])
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!("not inside a git repository: {}", cwd.display());
        }
        let text = String::from_utf8(out.stdout)?;
        let mut lines = text.lines();
        let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
            bail!("unexpected git rev-parse output: {text}");
        };
        let root = PathBuf::from(top).canonicalize()?;
        let dir = cwd.join(common).canonicalize()?.join("six-ten");
        Store::at(root, dir)
    }

    pub fn at(root: PathBuf, dir: PathBuf) -> Result<Store> {
        for sub in ["locks", "touched", "seen"] {
            fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(Store { root, dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Repo-relative form of `path`, or None when it lies outside the repository.
    pub fn normalize(&self, cwd: &Path, path: &str) -> Option<String> {
        let abs = resolve(&cwd.join(path));
        let rel = abs.strip_prefix(&self.root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        (!rel.is_empty() && rel != ".git" && !rel.starts_with(".git/")).then_some(rel)
    }

    pub fn claim(&self, agent: &Agent, paths: &[String], ttl: u64) -> Result<Claim> {
        self.locked(|| {
            let now = now();
            let current: Vec<(String, Option<Lease>)> =
                paths.iter().map(|p| (p.clone(), self.read(p))).collect();
            let conflicts: Vec<Lease> = current
                .iter()
                .filter_map(|(_, l)| l.clone())
                .filter(|l| blocks(&l.agent, &agent.id) && is_live(l, now))
                .collect();
            if !conflicts.is_empty() {
                return Ok(Claim::Conflict(conflicts));
            }
            for (path, existing) in current {
                let acquired_at = existing
                    .filter(|l| l.agent == agent.id)
                    .map_or(now, |l| l.acquired_at);
                let lease = Lease {
                    path,
                    agent: agent.id.clone(),
                    pid: agent.pid,
                    acquired_at,
                    expires_at: now + ttl,
                };
                self.write(&lease)?;
            }
            Ok(Claim::Granted)
        })
    }

    /// Retries `claim` until granted or `timeout` elapses; returns the last conflict on timeout.
    pub fn wait(
        &self,
        agent: &Agent,
        paths: &[String],
        ttl: u64,
        timeout: Duration,
    ) -> Result<Claim> {
        let start = Instant::now();
        loop {
            let claim = self.claim(agent, paths, ttl)?;
            if claim == Claim::Granted || start.elapsed() >= timeout {
                return Ok(claim);
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    /// Releases leases held by `agent_id` or its subagents (all when `paths` is None); returns released paths.
    pub fn release(&self, agent_id: &str, paths: Option<&[String]>) -> Result<Vec<String>> {
        self.locked(|| {
            let mut released = Vec::new();
            for lease in self.list()? {
                let wanted = paths.is_none_or(|ps| ps.contains(&lease.path));
                if wanted && !blocks(agent_id, &lease.agent) {
                    fs::remove_file(self.lease_file(&lease.path))?;
                    released.push(lease.path);
                }
            }
            Ok(released)
        })
    }

    /// Live leases held by agents other than `agent_id`.
    pub fn others(&self, agent_id: &str) -> Result<Vec<Lease>> {
        let now = now();
        Ok(self
            .list()?
            .into_iter()
            .filter(|l| blocks(&l.agent, agent_id) && is_live(l, now))
            .collect())
    }

    /// All live leases, sorted by path.
    pub fn live(&self) -> Result<Vec<Lease>> {
        self.others("")
    }

    /// Deletes expired or orphaned leases; returns how many were removed.
    pub fn gc(&self) -> Result<usize> {
        self.locked(|| {
            let now = now();
            let mut removed = 0;
            for lease in self.list()?.into_iter().filter(|l| !is_live(l, now)) {
                fs::remove_file(self.lease_file(&lease.path))?;
                removed += 1;
            }
            for entry in fs::read_dir(self.dir.join("touched"))? {
                let path = entry?.path();
                if read_json::<Touched>(&path).is_none_or(|t| !t.pid.is_none_or(pid_alive)) {
                    fs::remove_file(&path)?;
                    removed += 1;
                }
            }
            Ok(removed)
        })
    }

    /// Records that `agent` edited `paths`; `clean` paths had no uncommitted changes, so other
    /// agents' stale claims on them are dropped.
    pub fn touch(&self, agent: &Agent, paths: &[String], clean: &[String]) -> Result<()> {
        self.locked(|| {
            if !clean.is_empty() {
                for entry in fs::read_dir(self.dir.join("touched"))? {
                    let path = entry?.path();
                    if let Some(mut t) = read_json::<Touched>(&path) {
                        let before = t.paths.len();
                        t.paths.retain(|p| !clean.contains(p));
                        if t.paths.len() != before {
                            write_json(&path, &t)?;
                        }
                    }
                }
            }
            let file = self.touched_file(&agent.id);
            let mut touched = read_json::<Touched>(&file).unwrap_or(Touched {
                agent: agent.id.clone(),
                pid: agent.pid,
                paths: BTreeSet::new(),
            });
            let before = touched.paths.len();
            touched.paths.extend(paths.iter().cloned());
            if touched.paths.len() != before {
                write_json(&file, &touched)?;
            }
            Ok(())
        })
    }

    /// Other live agents with uncommitted edits, restricted to paths in `dirty`.
    pub fn touched_by_others(
        &self,
        agent_id: &str,
        dirty: &HashSet<String>,
    ) -> Result<Vec<Touched>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join("touched"))? {
            let Some(mut t) = read_json::<Touched>(&entry?.path()) else {
                continue;
            };
            if !blocks(&t.agent, agent_id) || !t.pid.is_none_or(pid_alive) {
                continue;
            }
            t.paths.retain(|p| dirty.contains(p));
            if !t.paths.is_empty() {
                out.push(t);
            }
        }
        out.sort_by(|a, b| a.agent.cmp(&b.agent));
        Ok(out)
    }

    /// Paths `agent_id` (or its subagents) has touched.
    pub fn touched_by(&self, agent_id: &str) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for entry in fs::read_dir(self.dir.join("touched"))? {
            if let Some(t) = read_json::<Touched>(&entry?.path()) {
                if !blocks(agent_id, &t.agent) {
                    out.extend(t.paths);
                }
            }
        }
        Ok(out)
    }

    fn list(&self) -> Result<Vec<Lease>> {
        let mut leases = Vec::new();
        for entry in fs::read_dir(self.dir.join("locks"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                leases.extend(read_json::<Lease>(&path));
            }
        }
        leases.sort_by(|a: &Lease, b| a.path.cmp(&b.path));
        Ok(leases)
    }

    fn read(&self, path: &str) -> Option<Lease> {
        read_json(&self.lease_file(path))
    }

    fn write(&self, lease: &Lease) -> Result<()> {
        write_json(&self.lease_file(&lease.path), lease)
    }

    fn lease_file(&self, path: &str) -> PathBuf {
        self.dir
            .join("locks")
            .join(format!("{}.json", hex_sha(path)))
    }

    fn touched_file(&self, agent_id: &str) -> PathBuf {
        self.dir
            .join("touched")
            .join(format!("{}.json", hex_sha(agent_id)))
    }

    pub(crate) fn locked<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let mutex: File = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".mutex"))?;
        FileExt::lock(&mutex)?;
        let result = f();
        FileExt::unlock(&mutex)?;
        result
    }
}

/// Whether a lease held by `holder` stops `me`; an agent is never blocked by itself or its parent.
pub fn blocks(holder: &str, me: &str) -> bool {
    holder != me
        && !me
            .strip_prefix(holder)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub(crate) fn hex_sha(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn is_live(lease: &Lease, now: u64) -> bool {
    lease.expires_at > now && lease.pid.is_none_or(pid_alive)
}

/// Canonicalizes the longest existing prefix of `path`, so not-yet-created files resolve too.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for part in rest.iter().rev() {
        match Path::new(part).components().next() {
            Some(Component::ParentDir) => {
                out.pop();
            }
            Some(Component::CurDir) => {}
            _ => out.push(part),
        }
    }
    out
}
