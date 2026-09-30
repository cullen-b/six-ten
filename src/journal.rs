use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store::{Store, foreign, hex_sha, now, read_json, write_json};

/// One completed write: `after` is the git blob of the new content (None = deleted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub agent: String,
    pub path: String,
    pub after: Option<String>,
    pub at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeenFile {
    pub seq: u64,
    pub blob: Option<String>,
}

/// What one agent last saw of each file, plus its turn cursors.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Seen {
    files: BTreeMap<String, SeenFile>,
    turn_seq: u64,
    listed_seq: u64,
}

/// A file other agents changed since this agent last saw it.
#[derive(Debug, PartialEq)]
pub struct StaleFile {
    pub path: String,
    pub by: BTreeSet<String>,
    /// Content the agent last saw; None when it never saw the file.
    pub seen: Option<Option<String>>,
    pub now: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Stale {
    /// Stale files the agent is about to write.
    pub targets: Vec<StaleFile>,
    /// Other stale files the agent has read or written.
    pub seen: Vec<StaleFile>,
    /// Files others changed this turn that the agent never looked at.
    pub unseen: Vec<String>,
}

impl Stale {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.seen.is_empty() && self.unseen.is_empty()
    }
}

impl Store {
    /// Appends a write by `agent_id`, which has now seen the result.
    pub fn record_write(&self, agent_id: &str, path: &str, after: Option<String>) -> Result<()> {
        self.locked(|| {
            let entries = self.entries()?;
            let seq = entries.last().map_or(0, |e| e.seq) + 1;
            let entry = Entry {
                seq,
                agent: agent_id.into(),
                path: path.into(),
                after: after.clone(),
                at: now(),
            };
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.journal())?;
            writeln!(file, "{}", serde_json::to_string(&entry)?)?;
            self.update_seen(agent_id, seq - 1, |s| {
                s.files.insert(path.into(), SeenFile { seq, blob: after });
            })
        })
    }

    /// Notes that `agent_id` has read `path` with content `blob`.
    pub fn record_read(&self, agent_id: &str, path: &str, blob: Option<String>) -> Result<()> {
        self.locked(|| {
            let seq = self.head()?;
            self.update_seen(agent_id, seq, |s| {
                s.files.insert(path.into(), SeenFile { seq, blob });
            })
        })
    }

    pub fn record_turn_start(&self, agent_id: &str) -> Result<()> {
        self.locked(|| {
            let seq = self.head()?;
            self.update_seen(agent_id, seq, |s| s.turn_seq = seq)
        })
    }

    /// Changes by other agents that `agent_id` has not been told about, marking them as told.
    pub fn take_stale(&self, agent_id: &str, targets: &[String]) -> Result<Stale> {
        self.locked(|| {
            let entries = self.entries()?;
            let head = entries.last().map_or(0, |e| e.seq);
            let file = self.seen_file(agent_id);
            let mut seen = self.load_seen(agent_id, head);
            let mut stale = Stale::default();

            let mut changed: BTreeMap<&str, (BTreeSet<String>, Option<String>)> = BTreeMap::new();
            let since = |path: &str| seen.files.get(path).map_or(seen.turn_seq, |f| f.seq);
            for e in entries.iter().filter(|e| e.seq > since(&e.path)) {
                let slot = changed.entry(&e.path).or_default();
                if foreign(&e.agent, agent_id) {
                    slot.0.insert(e.agent.clone());
                }
                slot.1 = e.after.clone();
            }
            for (path, (by, now)) in changed {
                if by.is_empty() {
                    continue;
                }
                let prior = seen.files.get(path).map(|f| f.blob.clone());
                if prior.as_ref() == Some(&now) {
                    continue;
                }
                let is_target = targets.iter().any(|t| t == path);
                if prior.is_none() && !is_target {
                    let fresh = seen.listed_seq.max(seen.turn_seq);
                    if entries
                        .iter()
                        .any(|e| e.path == path && e.seq > fresh && foreign(&e.agent, agent_id))
                    {
                        stale.unseen.push(path.to_string());
                    }
                    continue;
                }
                let item = StaleFile {
                    path: path.to_string(),
                    by,
                    seen: prior,
                    now: now.clone(),
                };
                if is_target {
                    stale.targets.push(item)
                } else {
                    stale.seen.push(item)
                }
                seen.files.insert(
                    path.to_string(),
                    SeenFile {
                        seq: head,
                        blob: now,
                    },
                );
            }
            seen.listed_seq = head;
            write_json(&file, &seen)?;
            Ok(stale)
        })
    }

    /// Drops journal entries older than `max_age` seconds; returns how many were removed.
    pub fn trim_journal(&self, max_age: u64) -> Result<usize> {
        self.locked(|| {
            let entries = self.entries()?;
            let cutoff = now().saturating_sub(max_age);
            let keep: Vec<&Entry> = entries.iter().filter(|e| e.at >= cutoff).collect();
            let removed = entries.len() - keep.len();
            if removed > 0 {
                // Keep the newest entry so sequence numbers keep increasing.
                let keep = if keep.is_empty() {
                    entries.last().into_iter().collect()
                } else {
                    keep
                };
                let body: String = keep
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap_or_default() + "\n")
                    .collect();
                let tmp = self.journal().with_extension("tmp");
                fs::write(&tmp, body)?;
                fs::rename(tmp, self.journal())?;
            }
            Ok(removed)
        })
    }

    fn entries(&self) -> Result<Vec<Entry>> {
        match fs::read_to_string(self.journal()) {
            Ok(text) => Ok(text
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn head(&self) -> Result<u64> {
        Ok(self.entries()?.last().map_or(0, |e| e.seq))
    }

    fn update_seen(&self, agent_id: &str, head: u64, f: impl FnOnce(&mut Seen)) -> Result<()> {
        let mut seen = self.load_seen(agent_id, head);
        f(&mut seen);
        write_json(&self.seen_file(agent_id), &seen)
    }

    /// An agent six-ten has not seen before starts with nothing to catch up on.
    fn load_seen(&self, agent_id: &str, head: u64) -> Seen {
        read_json(&self.seen_file(agent_id)).unwrap_or(Seen {
            turn_seq: head,
            listed_seq: head,
            ..Seen::default()
        })
    }

    fn journal(&self) -> PathBuf {
        self.dir.join("journal.jsonl")
    }

    fn seen_file(&self, agent_id: &str) -> PathBuf {
        self.dir
            .join("seen")
            .join(format!("{}.json", hex_sha(agent_id)))
    }
}
