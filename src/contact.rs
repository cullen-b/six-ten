use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store::{hex_sha, read_json, write_json};

/// How other agents can reach an agent: its chat thread in an app such as T3 Code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub app: String,
    /// Empty when six-ten looked and found none.
    pub thread: String,
}

/// Subagents share their parent's thread.
fn file(agent_id: &str) -> PathBuf {
    let base = agent_id.split('/').next().unwrap_or(agent_id);
    std::env::temp_dir()
        .join("six-ten-agents")
        .join(format!("{}.contact", hex_sha(base)))
}

/// The thread `agent_id` can be messaged in, if known.
pub fn of(agent_id: &str) -> Option<Contact> {
    read_json::<Contact>(&file(agent_id)).filter(|c| !c.thread.is_empty())
}

pub fn save(agent_id: &str, contact: &Contact) -> Result<()> {
    let file = file(agent_id);
    fs::create_dir_all(file.parent().expect("contact file has a parent"))?;
    write_json(&file, contact)
}

/// Looks up the calling agent's thread once: `SIX_TEN_THREAD`, else T3 Code's log for the harness
/// session (`session`, from a hook payload, or the harness's own environment).
pub fn detect(agent_id: &str, session: Option<&str>) -> Result<()> {
    if file(agent_id).exists() {
        return Ok(());
    }
    let found = match std::env::var("SIX_TEN_THREAD")
        .ok()
        .filter(|t| !t.is_empty())
    {
        Some(thread) => Some(Contact {
            app: std::env::var("SIX_TEN_THREAD_APP").unwrap_or_else(|_| "t3".into()),
            thread,
        }),
        None => {
            // With SIX_TEN_AGENT set, the harness session in the environment isn't this agent's.
            let own_env = std::env::var("SIX_TEN_AGENT").is_err();
            let session = session.map(String::from).or_else(|| {
                ["CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"]
                    .iter()
                    .filter(|_| own_env)
                    .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
            });
            session.and_then(|s| t3_thread(&s)).map(|thread| Contact {
                app: "t3".into(),
                thread,
            })
        }
    };
    // Recorded even when nothing was found, so the search runs once per agent.
    save(
        agent_id,
        &found.unwrap_or(Contact {
            app: String::new(),
            thread: String::new(),
        }),
    )
}

/// T3 Code logs each thread to `events.<thread>.log`, opening with the provider session id.
fn t3_thread(session: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".t3/userdata/logs/provider");
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    logs.sort_by_key(|l| std::cmp::Reverse(l.0));
    let needle = format!("\"{session}\"");
    for (_, path) in logs {
        let mut head = vec![0; 8192];
        let Ok(n) = File::open(&path).and_then(|mut f| f.read(&mut head)) else {
            continue;
        };
        if String::from_utf8_lossy(&head[..n]).contains(&needle) {
            let name = path.file_name()?.to_str()?;
            return Some(
                name.strip_prefix("events.")?
                    .strip_suffix(".log")?
                    .to_string(),
            );
        }
    }
    None
}

/// A Claude Code session's peer name and status, from `~/.claude/sessions/<pid>.json`; peers
/// message each other by that name with Claude Code's SendMessage tool.
fn claude_peer(agent_id: &str) -> Option<(String, String)> {
    let base = agent_id.split('/').next()?;
    let pid = base.strip_prefix("claude:")?;
    let home = std::env::var("HOME").ok()?;
    let file = PathBuf::from(home).join(format!(".claude/sessions/{pid}.json"));
    let v: serde_json::Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    Some((
        v["name"].as_str()?.to_string(),
        v["status"].as_str().unwrap_or("unknown").to_string(),
    ))
}

/// The ways a blocked agent can wake `holder` directly, for when it seems stalled.
pub fn wake_channels(holder: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(c) = of(holder) {
        out.push(if c.app == "t3" {
            format!("T3 Code thread {} (t3_thread_send)", c.thread)
        } else {
            format!("{} thread {}", c.app, c.thread)
        });
    }
    if let Some((name, status)) = claude_peer(holder) {
        out.push(format!(
            "Claude Code session `{name}`, now {status} (SendMessage)"
        ));
    }
    out
}
