use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::policy::human;
use crate::store::{Lease, Store, blocks, hex_sha, now, read_json, write_json};

/// Something the user may want to know about: a block, how it was resolved, or a refusal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    pub agent: String,
    /// blocked | resolved | stale | refused | unresolved
    pub kind: String,
    pub text: String,
}

/// An agent is waiting on `path`, either held by `by` or changed by `by` since it last read it.
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    agent: String,
    path: String,
    by: String,
    stale: bool,
    since: u64,
    via_wait: bool,
    meanwhile: BTreeSet<String>,
}

impl Store {
    pub fn note_blocked(&self, agent_id: &str, held: &[Lease]) -> Result<()> {
        for lease in held {
            let file = self.pending_file(agent_id, &lease.path);
            if file.exists() {
                continue;
            }
            self.save_pending(&Pending {
                agent: agent_id.into(),
                path: lease.path.clone(),
                by: lease.agent.clone(),
                stale: false,
                since: now(),
                via_wait: false,
                meanwhile: BTreeSet::new(),
            })?;
            self.log(
                agent_id,
                "blocked",
                format!("blocked on {} (held by {})", lease.path, lease.agent),
            )?;
        }
        Ok(())
    }

    pub fn note_stale(&self, agent_id: &str, path: &str, by: &str) -> Result<()> {
        self.save_pending(&Pending {
            agent: agent_id.into(),
            path: path.into(),
            by: by.into(),
            stale: true,
            since: now(),
            via_wait: false,
            meanwhile: BTreeSet::new(),
        })?;
        self.log(
            agent_id,
            "stale",
            format!("stopped from overwriting {path}: {by} changed it since it was read"),
        )
    }

    pub fn note_waiting(&self, agent_id: &str, paths: &[String]) -> Result<()> {
        for path in paths {
            if let Some(mut p) = read_json::<Pending>(&self.pending_file(agent_id, path)) {
                p.via_wait = true;
                self.save_pending(&p)?;
            }
        }
        Ok(())
    }

    /// `agent_id` now holds `paths`: resolve its blocks on them, and count them as work done meanwhile.
    pub fn note_granted(&self, agent_id: &str, paths: &[String]) -> Result<()> {
        for mut p in self.pending_of(agent_id)? {
            let file = self.pending_file(&p.agent, &p.path);
            if paths.contains(&p.path) {
                fs::remove_file(&file)?;
                let text = if p.stale {
                    format!("re-read {} and retried after {}'s change", p.path, p.by)
                } else {
                    let meanwhile = match p.meanwhile.len() {
                        0 => String::new(),
                        1 => "edited 1 other file meanwhile, ".into(),
                        n => format!("edited {n} other files meanwhile, "),
                    };
                    let how = if p.via_wait {
                        "waited with six_ten_wait"
                    } else {
                        "retried"
                    };
                    format!(
                        "got {} after {} ({} held it): {meanwhile}{how}",
                        p.path,
                        human(now().saturating_sub(p.since)),
                        p.by
                    )
                };
                self.log(&p.agent, "resolved", text)?;
            } else {
                let before = p.meanwhile.len();
                p.meanwhile.extend(paths.iter().cloned());
                if p.meanwhile.len() != before {
                    self.save_pending(&p)?;
                }
            }
        }
        Ok(())
    }

    /// End of turn: report blocks the agent never got past.
    pub fn note_end(&self, agent_id: &str) -> Result<()> {
        for p in self.pending_of(agent_id)? {
            fs::remove_file(self.pending_file(&p.agent, &p.path))?;
            if !p.stale {
                self.log(
                    &p.agent,
                    "unresolved",
                    format!(
                        "finished its turn without editing {} (held by {})",
                        p.path, p.by
                    ),
                )?;
            }
        }
        Ok(())
    }

    pub fn log(&self, agent_id: &str, kind: &str, text: String) -> Result<()> {
        let event = Event {
            at: now(),
            agent: agent_id.into(),
            kind: kind.into(),
            text,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_file())?;
        writeln!(file, "{}", serde_json::to_string(&event)?)?;
        if self.notify_enabled() {
            desktop_notify(&event);
        }
        Ok(())
    }

    pub fn events(&self) -> Vec<Event> {
        fs::read_to_string(self.events_file())
            .map(|t| {
                t.lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn events_file(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub fn notify_enabled(&self) -> bool {
        self.dir.join("notify").exists()
    }

    pub fn set_notify(&self, on: bool) -> Result<()> {
        let flag = self.dir.join("notify");
        if on {
            fs::write(flag, "")?;
        } else if flag.exists() {
            fs::remove_file(flag)?;
        }
        Ok(())
    }

    /// Drops events older than `max_age` seconds.
    pub fn trim_events(&self, max_age: u64) -> Result<usize> {
        let events = self.events();
        let cutoff = now().saturating_sub(max_age);
        let keep: Vec<&Event> = events.iter().filter(|e| e.at >= cutoff).collect();
        let removed = events.len() - keep.len();
        if removed > 0 {
            let body: String = keep
                .iter()
                .filter_map(|e| serde_json::to_string(e).ok())
                .map(|l| l + "\n")
                .collect();
            fs::write(self.events_file(), body)?;
        }
        Ok(removed)
    }

    /// Pending blocks of `agent_id` and its subagents.
    fn pending_of(&self, agent_id: &str) -> Result<Vec<Pending>> {
        let dir = self.dir.join("pending");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in entries {
            if let Some(p) = read_json::<Pending>(&entry?.path()) {
                if !blocks(agent_id, &p.agent) {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }

    fn save_pending(&self, p: &Pending) -> Result<()> {
        fs::create_dir_all(self.dir.join("pending"))?;
        write_json(&self.pending_file(&p.agent, &p.path), p)
    }

    fn pending_file(&self, agent_id: &str, path: &str) -> PathBuf {
        self.dir
            .join("pending")
            .join(format!("{}.json", hex_sha(&format!("{agent_id}\0{path}"))))
    }
}

pub fn icon(kind: &str) -> &'static str {
    match kind {
        "blocked" => "⏸",
        "resolved" => "✅",
        "stale" => "🔄",
        "refused" => "⛔",
        "session" => "🛣️",
        _ => "⚠️",
    }
}

pub fn line(event: &Event) -> String {
    format!("{} {}  {}", icon(&event.kind), event.agent, event.text)
}

/// Fire-and-forget desktop notification; never blocks or fails the hook.
fn desktop_notify(event: &Event) {
    let body = format!("{} {}", icon(&event.kind), event.text);
    let child = if cfg!(target_os = "macos") {
        let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"six-ten\" subtitle \"{}\"",
            quote(&body),
            quote(&event.agent)
        );
        Command::new("osascript")
            .args(["-e", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    } else {
        Command::new("notify-send")
            .args([format!("six-ten: {}", event.agent), body])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    drop(child);
}

/// Prints recent events, then follows the log until interrupted.
pub fn watch(store: &Store, history: usize) -> Result<()> {
    let events = store.events();
    for e in events.iter().skip(events.len().saturating_sub(history)) {
        println!("{}  ({} ago)", line(e), human(now().saturating_sub(e.at)));
    }
    let mut seen = events.len();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let events = store.events();
        if events.len() < seen {
            seen = 0;
        }
        for e in &events[seen..] {
            println!("{}", line(e));
        }
        seen = events.len();
    }
}
