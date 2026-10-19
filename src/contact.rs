use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

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
    // Outside T3 Code a miss is final. Inside it, the thread's log may not show the session yet at
    // the first hook, so a miss is retried after a couple of seconds.
    let in_t3 = std::env::var_os("T3CODE_CLI_PATH").is_some();
    let retry_after = Duration::from_secs(if in_t3 { 2 } else { u64::MAX / 4 });
    let known = read_json::<Contact>(&file(agent_id)).is_some_and(|c| !c.thread.is_empty());
    let fresh_miss = fs::metadata(file(agent_id))
        .and_then(|m| m.modified())
        .is_ok_and(|at| at.elapsed().unwrap_or_default() < retry_after);
    if known || fresh_miss {
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
    let recent = SystemTime::now() - Duration::from_secs(30 * 60);
    let mut logs: Vec<(SystemTime, PathBuf)> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(at, _)| *at >= recent)
        .collect();
    logs.sort_by_key(|l| std::cmp::Reverse(l.0));
    // Long file names are cut short, so several threads can share one log; a live session's lines
    // are near the end, each naming its thread in the provider session id.
    let needle = format!("\"{session}\"");
    for (_, path) in logs {
        let Ok(mut f) = File::open(&path) else {
            continue;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL)));
        let mut tail = Vec::new();
        if f.take(TAIL).read_to_end(&mut tail).is_err() {
            continue;
        }
        let tail = String::from_utf8_lossy(&tail);
        let found = tail
            .lines()
            .rev()
            .filter(|l| l.contains(&needle))
            .find_map(thread_in_session_id);
        if found.is_some() {
            return found;
        }
    }
    None
}

/// How much of the end of each recent T3 log to search.
const TAIL: u64 = 2 << 20;

/// The thread in `"providerSessionId":"provider-session:...:thread:<url-encoded thread>:<run>"`.
fn thread_in_session_id(text: &str) -> Option<String> {
    let (_, rest) = text.split_once("\"providerSessionId\":\"")?;
    let id = rest.split('"').next()?;
    let (_, rest) = id.split_once(":thread:")?;
    let (encoded, _) = rest.rsplit_once(':')?;
    Some(percent_decode(encoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = s
            .get(i + 1..i + 3)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
            format!("T3 Code: t3_thread_send with threadId \"{}\"", c.thread)
        } else {
            format!("{} thread {}", c.app, c.thread)
        });
    }
    if let Some((name, status)) = claude_peer(holder) {
        out.push(format!(
            "Claude Code: SendMessage to \"{name}\" (that session is {status} now)"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_thread_from_t3_provider_session_ids() {
        let line = r#"{"providerSessionId":"provider-session:provider-instance:claudeAgent:thread:thread%3Adelegated-task%3Acommand%253Amcp%253A59:1681aed5"}"#;
        assert_eq!(
            super::thread_in_session_id(line).as_deref(),
            Some("thread:delegated-task:command%3Amcp%3A59")
        );
        let plain = r#""providerSessionId":"provider-session:provider-instance:claudeAgent:thread:bbcaeff0-1a19:2988""#;
        assert_eq!(
            super::thread_in_session_id(plain).as_deref(),
            Some("bbcaeff0-1a19")
        );
    }
}
