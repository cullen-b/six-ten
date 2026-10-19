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

/// How a blocked agent can ask `holder` to release a lease that looks stale.
pub fn ask_hint(holder: &str, me: &str, path: &str) -> Option<String> {
    let c = of(holder)?;
    let ask = format!(
        "six-ten: {me} is waiting for `{path}`, which you hold. If you're done with it, release it \
         (six_ten_release, or `six-ten release {path}`); if not, carry on."
    );
    Some(if c.app == "t3" {
        format!(
            "{holder} works in T3 Code thread {}. If its lease looks stale and you have T3 Code's \
             t3_thread_send tool, send that thread this once: \"{ask}\"",
            c.thread
        )
    } else {
        format!(
            "{holder} can be reached in {} thread {}. If its lease looks stale, send it this once: \"{ask}\"",
            c.app, c.thread
        )
    })
}
