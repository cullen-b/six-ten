use std::collections::HashMap;
use std::env;
use std::process::Command;

/// Who holds a lease. Hooks, CLI calls and the MCP server spawned by one harness session all
/// resolve to the same id because they share the harness process as their nearest real ancestor.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub id: String,
    pub pid: Option<u32>,
}

/// Wrapper processes skipped when walking up to the harness process.
const WRAPPERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "env",
    "sandbox-exec",
    "timeout",
    "six-ten",
    "git",
];

impl Agent {
    pub fn current() -> Agent {
        if let Some(id) = env::var("SIX_TEN_AGENT").ok().filter(|s| !s.is_empty()) {
            return Agent { id, pid: None };
        }
        match harness_ancestor() {
            Some((pid, name)) => Agent {
                id: format!("{name}:{pid}"),
                pid: Some(pid),
            },
            None => Agent {
                id: format!("pid:{}", std::process::id()),
                pid: None,
            },
        }
    }

    /// Identity of a subagent running inside this agent's harness process.
    pub fn sub(&self, sub_id: &str) -> Agent {
        Agent {
            id: format!("{}/{sub_id}", self.id),
            pid: self.pid,
        }
    }
}

fn harness_ancestor() -> Option<(u32, String)> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
        .ok()?;
    let table: HashMap<u32, (u32, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            let comm = parts.collect::<Vec<_>>().join(" ");
            let name = comm.rsplit('/').next()?.trim_start_matches('-').to_string();
            Some((pid, (ppid, name)))
        })
        .collect();
    let mut pid = table.get(&std::process::id())?.0;
    for _ in 0..16 {
        let (ppid, name) = table.get(&pid)?;
        if pid <= 1 {
            return None;
        }
        if !WRAPPERS.contains(&name.as_str()) {
            return Some((pid, name.clone()));
        }
        pid = *ppid;
    }
    None
}

pub fn pid_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|o| o.status.success())
}
