mod agent;
mod events;
mod git;
mod hook;
mod install;
mod journal;
mod mcp;
mod policy;
mod store;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use agent::Agent;
use policy::Decision;
use store::{Store, now};

/// Coordinates file edits between coding agents sharing one git checkout.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Claim files before editing them; exits 2 if another agent holds any of them.
    Claim {
        paths: Vec<String>,
        /// Why you are editing; shown to agents you block so they can coordinate.
        #[arg(long, short = 'm')]
        reason: Option<String>,
    },
    /// Block until files are free, then claim them.
    Wait {
        paths: Vec<String>,
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Release your claims (all of them when no paths are given).
    Release { paths: Vec<String> },
    /// Put this checkout on a session branch (agents/<date>-<topic>), or report the one in use.
    Session { topic: Option<String> },
    /// Record a decision so it outlives the chat session; shown in status and watch.
    Decide { text: Vec<String> },
    /// Show who is editing what.
    Status,
    /// Print the agent id six-ten sees for the calling process.
    Whoami,
    /// Follow what agents are doing: blocks, how they were resolved, and refusals.
    Watch {
        /// How many past events to show first.
        #[arg(long, default_value_t = 20)]
        history: usize,
    },
    /// Turn desktop notifications for this repo on or off (no argument: show the setting).
    Notify { state: Option<String> },
    /// Remove expired leases and records of agents that have exited.
    Gc,
    /// Handle a harness hook event read from stdin (claude, codex, opencode, hermes, generic).
    Hook { harness: String },
    /// Serve the MCP tools over stdio.
    Mcp,
    /// Git pre-commit check: refuse commits that include other agents' work.
    Precommit,
    /// Git commit-msg hook: add Agent and Co-edited-by trailers to the message file.
    CommitMsg { file: PathBuf },
    /// Wire six-ten into a harness for the repository at --repo (default: current directory).
    Install {
        /// claude, codex, opencode, hermes, generic (any other harness), git, or all
        harness: String,
        #[arg(long, conflicts_with = "global")]
        repo: Option<PathBuf>,
        /// Install into the user-level config, for every repository on this machine.
        #[arg(long)]
        global: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("six-ten: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(command: Command) -> Result<u8> {
    let cwd = std::env::current_dir()?;
    let decided = |d: Decision, ok: String| match d.granted_text(ok) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(reason) => {
            eprintln!("{reason}");
            2
        }
    };
    match command {
        Command::Hook { harness } => Ok(hook::run(&harness) as u8),
        Command::Mcp => mcp::serve().map(|_| 0),
        Command::Install {
            harness,
            repo,
            global,
        } => install::run(&harness, &repo.unwrap_or(cwd), global).map(|_| 0),
        Command::Whoami => {
            println!("{}", Agent::current().id);
            Ok(0)
        }
        command => {
            let store = Store::open(&cwd)?;
            let agent = Agent::current();
            match command {
                Command::Claim { paths, reason } => Ok(decided(
                    policy::pre_edit_with_reason(
                        &store,
                        &agent,
                        &cwd,
                        &paths,
                        reason.as_deref(),
                    )?,
                    format!("claimed {}", paths.join(" ")),
                )),
                Command::Wait { paths, timeout } => {
                    let d =
                        policy::wait(&store, &agent, &cwd, &paths, Duration::from_secs(timeout))?;
                    Ok(decided(d, format!("claimed {}", paths.join(" "))))
                }
                Command::Release { paths } => {
                    let only: Option<Vec<String>> = (!paths.is_empty()).then(|| {
                        paths
                            .iter()
                            .filter_map(|p| store.normalize(&cwd, p))
                            .collect()
                    });
                    let released = store.release(&agent.id, only.as_deref())?;
                    println!("released {} lease(s)", released.len());
                    Ok(0)
                }
                Command::Session { topic } => {
                    println!("{}", policy::session(&store, &agent, topic.as_deref())?);
                    Ok(0)
                }
                Command::Decide { text } => {
                    let text = text.join(" ");
                    anyhow::ensure!(!text.is_empty(), "give the decision to record");
                    store.log(&agent.id, "decision", text.clone())?;
                    println!("recorded decision: {text}");
                    Ok(0)
                }
                Command::Status => {
                    print!("{}", status_text(&store, &agent)?);
                    Ok(0)
                }
                Command::Watch { history } => events::watch(&store, history).map(|_| 0),
                Command::Notify { state } => {
                    match state.as_deref() {
                        Some("on") => store.set_notify(true)?,
                        Some("off") => store.set_notify(false)?,
                        Some(other) => anyhow::bail!("expected `on` or `off`, got `{other}`"),
                        None => {}
                    }
                    let on = store.notify_enabled();
                    println!(
                        "desktop notifications are {}",
                        if on { "on" } else { "off" }
                    );
                    Ok(0)
                }
                Command::Gc => {
                    let day = 24 * 60 * 60;
                    let removed =
                        store.gc()? + store.trim_journal(day)? + store.trim_events(day)?;
                    println!("removed {removed} stale record(s)");
                    Ok(0)
                }
                Command::CommitMsg { file } => {
                    for trailer in policy::commit_trailers(&store, &agent)? {
                        let status = std::process::Command::new("git")
                            .args([
                                "interpret-trailers",
                                "--in-place",
                                "--if-exists",
                                "addIfDifferent",
                                "--trailer",
                                &trailer,
                            ])
                            .arg(&file)
                            .status()?;
                        anyhow::ensure!(status.success(), "git interpret-trailers failed");
                    }
                    Ok(0)
                }
                Command::Precommit => {
                    Ok(decided(policy::pre_commit(&store, &agent)?, String::new()))
                }
                _ => unreachable!(),
            }
        }
    }
}

pub fn status_text(store: &Store, me: &Agent) -> Result<String> {
    let now = now();
    let mut out = format!("you are {}\n", me.id);
    let active = store.active_others(&me.id)?;
    if active.is_empty() {
        out.push_str("no other agents active\n");
    } else {
        let list: Vec<String> = active
            .iter()
            .map(|p| {
                format!(
                    "{} ({} ago)",
                    p.agent,
                    policy::human(now.saturating_sub(p.at))
                )
            })
            .collect();
        out.push_str(&format!("other agents active: {}\n", list.join(", ")));
    }
    let leases = store.live()?;
    if leases.is_empty() {
        out.push_str("no files are being edited\n");
    } else {
        out.push_str("being edited:\n");
        for l in leases {
            let who = if store::blocks(&l.agent, &me.id) {
                l.agent.clone()
            } else {
                format!("{} (you)", l.agent)
            };
            let why = l
                .reason
                .as_deref()
                .map_or(String::new(), |r| format!(": {r}"));
            out.push_str(&format!(
                "  {}  {}  expires in {}{why}\n",
                l.path,
                who,
                policy::human(l.expires_at.saturating_sub(now))
            ));
        }
    }
    let dirty = git::dirty(store.root(), &[])?;
    let others = store.touched_by_others(&me.id, &dirty)?;
    let events = store.events();
    if !events.is_empty() {
        out.push_str("recent:\n");
        for e in events.iter().skip(events.len().saturating_sub(8)) {
            out.push_str(&format!(
                "  {}  ({} ago)\n",
                events::line(e),
                policy::human(now.saturating_sub(e.at))
            ));
        }
    }
    if !others.is_empty() {
        out.push_str("uncommitted work of other agents (do not stash, reset or commit these):\n");
        for t in others {
            for p in t.paths {
                out.push_str(&format!("  {p}  {}\n", t.agent));
            }
        }
    }
    Ok(out)
}
