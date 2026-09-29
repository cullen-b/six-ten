use std::io::Read;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::policy::{self, Decision};
use crate::store::Store;

/// What a harness hook event means for six-ten.
#[derive(Debug, PartialEq)]
pub enum Event {
    PreEdit(Vec<String>),
    PreShell(String),
    PostWrite(Vec<String>),
    PostRead(Vec<String>),
    PostShell(String),
    TurnStart,
    End,
    Ignore,
}

/// Reads one hook payload from stdin and answers in the harness's own protocol; returns the exit code.
pub fn run(harness: &str) -> i32 {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("six-ten: could not read hook input: {e}");
        return 0;
    }
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let kind = ["hook_event_name", "event"]
        .iter()
        .find_map(|k| payload[*k].as_str())
        .unwrap_or_default();
    match handle(harness, &payload) {
        Ok(decision) => respond(harness, kind, decision),
        // Fail open: a broken coordinator must never wedge the agent.
        Err(e) => {
            eprintln!("six-ten: hook error (edit allowed): {e:#}");
            0
        }
    }
}

fn respond(harness: &str, kind: &str, decision: Decision) -> i32 {
    let block = |reason: String| {
        if harness == "hermes" {
            println!("{}", json!({"decision": "block", "reason": reason}));
            0
        } else {
            eprintln!("{reason}");
            2
        }
    };
    match decision {
        Decision::Allow => 0,
        Decision::Deny(reason) | Decision::Stale(reason) => block(reason),
        Decision::Note(note) => match (harness, kind) {
            ("claude" | "codex", "PreToolUse" | "UserPromptSubmit") => {
                println!(
                    "{}",
                    json!({"hookSpecificOutput": {"hookEventName": kind, "additionalContext": note}})
                );
                0
            }
            ("hermes", "pre_llm_call") => {
                println!("{}", json!({"context": note}));
                0
            }
            // Hermes shell hooks cannot annotate an allowed call, so refuse it once with the note.
            ("hermes", _) => block(format!(
                "{note}\n\n(Nothing was changed. Retry the same call now.)"
            )),
            _ => {
                println!("{}", json!({"note": note}));
                0
            }
        },
    }
}

pub fn handle(harness: &str, payload: &Value) -> Result<Decision> {
    let (event, sub) = parse(harness, payload)?;
    if event == Event::Ignore {
        return Ok(Decision::Allow);
    }
    let cwd = ["cwd", "directory"]
        .iter()
        .find_map(|k| payload[*k].as_str())
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    // Outside a git repository there is nothing to coordinate.
    let Ok(store) = Store::open(&cwd) else {
        return Ok(Decision::Allow);
    };
    let base = Agent::current();
    let agent = match &sub {
        Some(s) => base.sub(s),
        None => base,
    };
    let done = |r: Result<()>| r.map(|_| Decision::Allow);
    match event {
        Event::PreEdit(paths) => policy::pre_edit(&store, &agent, &cwd, &paths),
        Event::PreShell(command) => {
            let writes = shell_writes(&command);
            let edit = if writes.is_empty() {
                Decision::Allow
            } else {
                policy::pre_edit(&store, &agent, &cwd, &writes)?
            };
            if matches!(edit, Decision::Deny(_) | Decision::Stale(_)) {
                return Ok(edit);
            }
            match policy::pre_shell(&store, &agent, &cwd, &command)? {
                Decision::Allow => Ok(edit),
                refused => Ok(refused),
            }
        }
        Event::PostWrite(paths) => done(policy::post_write(&store, &agent, &cwd, &paths)),
        Event::PostRead(paths) => done(policy::post_read(&store, &agent, &cwd, &paths)),
        Event::PostShell(command) => done(
            policy::post_write(&store, &agent, &cwd, &shell_writes(&command))
                .and_then(|_| policy::post_read(&store, &agent, &cwd, &shell_reads(&command))),
        ),
        Event::TurnStart => policy::turn_start(&store, &agent),
        Event::End => policy::end(&store, &agent).map(|_| Decision::Allow),
        Event::Ignore => Ok(Decision::Allow),
    }
}

pub fn parse(harness: &str, p: &Value) -> Result<(Event, Option<String>)> {
    Ok(match harness {
        "claude" | "codex" => (claude_or_codex(p), text(&p["agent_id"])),
        "opencode" => opencode(p),
        "hermes" => hermes(p),
        "generic" => generic(p),
        other => {
            bail!("unknown harness `{other}` (expected claude, codex, opencode, hermes or generic)")
        }
    })
}

/// Claude Code and Codex share the hook protocol; Codex sends `apply_patch` with the raw patch text.
fn claude_or_codex(p: &Value) -> Event {
    let input = &p["tool_input"];
    let tool = p["tool_name"].as_str().unwrap_or_default();
    let patch = || input["command"].as_str().or(input["input"].as_str());
    match p["hook_event_name"].as_str().unwrap_or_default() {
        "PreToolUse" => match tool {
            "Edit" | "Write" | "MultiEdit" => strings(&[&input["file_path"]]),
            "NotebookEdit" => strings(&[&input["notebook_path"]]),
            "apply_patch" => patch_event(patch()),
            "Bash" => shell_event(input["command"].as_str()),
            _ => Event::Ignore,
        },
        "PostToolUse" => match tool {
            "Edit" | "Write" | "MultiEdit" => post_write(strings(&[&input["file_path"]])),
            "NotebookEdit" => post_write(strings(&[&input["notebook_path"]])),
            "Read" => post_read(strings(&[&input["file_path"]])),
            "apply_patch" => post_write(patch_event(patch())),
            "Bash" => post_shell(shell_event(input["command"].as_str())),
            _ => Event::Ignore,
        },
        "UserPromptSubmit" => Event::TurnStart,
        "Stop" | "SessionEnd" | "SubagentStop" => Event::End,
        _ => Event::Ignore,
    }
}

/// Turns a pre-write event into the matching post-write one.
fn post_write(pre: Event) -> Event {
    match pre {
        Event::PreEdit(paths) => Event::PostWrite(paths),
        _ => Event::Ignore,
    }
}

fn post_read(pre: Event) -> Event {
    match pre {
        Event::PreEdit(paths) => Event::PostRead(paths),
        _ => Event::Ignore,
    }
}

fn post_shell(pre: Event) -> Event {
    match pre {
        Event::PreEdit(paths) => Event::PostWrite(paths),
        Event::PreShell(command) => Event::PostShell(command),
        _ => Event::Ignore,
    }
}

/// Payload built by integrations/opencode/six-ten.ts: `{event, tool, sessionID, args, directory}`.
fn opencode(p: &Value) -> (Event, Option<String>) {
    let args = &p["args"];
    let tool = p["tool"].as_str().unwrap_or_default();
    let pre = || match tool {
        "edit" | "write" => strings(&[&args["filePath"]]),
        "apply_patch" => patch_event(args["patchText"].as_str()),
        "bash" => shell_event(args["command"].as_str()),
        _ => Event::Ignore,
    };
    let event = match p["event"].as_str().unwrap_or_default() {
        "tool.execute.before" => pre(),
        "tool.execute.after" if tool == "read" => post_read(strings(&[&args["filePath"]])),
        "tool.execute.after" if tool == "bash" => post_shell(pre()),
        "tool.execute.after" => post_write(pre()),
        "chat.message" => Event::TurnStart,
        "session.idle" | "session.deleted" => Event::End,
        _ => Event::Ignore,
    };
    (event, text(&p["sessionID"]))
}

/// Hermes shell-hook payload: `{hook_event_name, tool_name, tool_input, session_id, cwd}`.
fn hermes(p: &Value) -> (Event, Option<String>) {
    let input = &p["tool_input"];
    let tool = p["tool_name"].as_str().unwrap_or_default();
    let pre = || match tool {
        "write_file" => strings(&[&input["path"]]),
        "patch" if input["mode"].as_str() == Some("patch") => patch_event(input["patch"].as_str()),
        "patch" => strings(&[&input["path"]]),
        "terminal" => shell_event(input["command"].as_str()),
        _ => Event::Ignore,
    };
    let event = match p["hook_event_name"].as_str().unwrap_or_default() {
        "pre_tool_call" => pre(),
        "post_tool_call" if tool == "read_file" => post_read(strings(&[&input["path"]])),
        "post_tool_call" if tool == "terminal" => post_shell(pre()),
        "post_tool_call" => post_write(pre()),
        "pre_llm_call" => Event::TurnStart,
        "on_session_end" | "on_session_finalize" => Event::End,
        _ => Event::Ignore,
    };
    (event, text(&p["session_id"]))
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

fn patch_event(patch: Option<&str>) -> Event {
    let paths = patch.map(patch_paths).unwrap_or_default();
    if paths.is_empty() {
        Event::Ignore
    } else {
        Event::PreEdit(paths)
    }
}

fn shell_event(command: Option<&str>) -> Event {
    match command {
        Some(c) if c.contains("*** Begin Patch") => patch_event(Some(c)),
        Some(c) => Event::PreShell(c.into()),
        None => Event::Ignore,
    }
}

/// Paths named by `*** Add/Update/Delete File:` and move headers of an apply_patch envelope.
pub fn patch_paths(patch: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in patch.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("*** ") else {
            continue;
        };
        for prefix in ["Add File: ", "Update File: ", "Delete File: ", "Move to: "] {
            if let Some(path) = rest.strip_prefix(prefix) {
                out.push(path.trim().to_string());
            }
        }
        if let Some((from, to)) = rest
            .strip_prefix("Move File: ")
            .and_then(|r| r.split_once(" -> "))
        {
            out.push(from.trim().to_string());
            out.push(to.trim().to_string());
        }
    }
    out
}

/// Best-effort list of files a shell command writes: redirections, `tee`, `sed -i`/`perl -i`,
/// and `rm`/`mv`/`cp`/`touch` targets. Catches the usual ways around an edit-tool block.
pub fn shell_writes(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let spaced = crate::git::skeleton(command)
        .replace(">>", " >> ")
        .replace('>', " > ")
        .replace(" >  > ", " >> ");
    let separated = spaced.replace("&&", ";").replace("||", ";");
    for segment in separated.split([';', '|', '\n', '(', ')']) {
        let tokens: Vec<&str> = segment
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '"' || c == '\''))
            .collect();
        let mut i = 0;
        while i < tokens.len() {
            if matches!(tokens[i], ">" | ">>") {
                if let Some(target) = tokens.get(i + 1) {
                    let fd_dup = tokens[i + 1].starts_with('&')
                        || tokens
                            .get(i.wrapping_sub(1))
                            .is_some_and(|t| t.ends_with('2') && t.len() == 1);
                    if !fd_dup && !target.starts_with("/dev/") && !target.is_empty() {
                        out.push(target.to_string());
                    }
                }
                i += 2;
                continue;
            }
            i += 1;
        }
        let words: Vec<&str> = tokens
            .iter()
            .take_while(|t| !matches!(**t, ">" | ">>" | "<" | "<<"))
            .copied()
            .collect();
        let Some(cmd) = words.first().map(|c| c.rsplit('/').next().unwrap_or(c)) else {
            continue;
        };
        let positional: Vec<&str> = words[1..]
            .iter()
            .filter(|w| !w.starts_with('-'))
            .copied()
            .collect();
        let in_place = words
            .iter()
            .any(|w| w.starts_with("-i") || *w == "--in-place" || w.starts_with("-pi"));
        match cmd {
            "tee" | "rm" | "touch" | "truncate" => {
                out.extend(positional.iter().map(|s| s.to_string()))
            }
            "mv" => out.extend(positional.iter().map(|s| s.to_string())),
            "cp" | "install" => out.extend(positional.last().map(|s| s.to_string())),
            "sed" | "gsed" if in_place => {
                out.extend(positional.iter().skip(1).map(|s| s.to_string()))
            }
            "perl" if in_place => out.extend(positional.iter().skip(1).map(|s| s.to_string())),
            _ => {}
        }
    }
    out.retain(|p| !p.is_empty() && !p.contains(['$', '*', '<', '`', '{']));
    out.sort();
    out.dedup();
    out
}

/// `{"event": "pre_edit"|"pre_shell"|"end", "cwd", "paths": [..], "command", "sub"}`, for small
/// harness shims that do their own payload mapping.
fn generic(p: &Value) -> (Event, Option<String>) {
    let sub = text(&p["sub"]);
    let event = match p["event"].as_str().unwrap_or_default() {
        "pre_edit" => {
            let paths: Vec<&Value> = p["paths"]
                .as_array()
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            strings(&paths)
        }
        "pre_shell" => p["command"]
            .as_str()
            .map_or(Event::Ignore, |c| Event::PreShell(c.into())),
        "end" => Event::End,
        _ => Event::Ignore,
    };
    (event, sub)
}

fn strings(values: &[&Value]) -> Event {
    let paths: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_str())
        .map(String::from)
        .collect();
    if paths.is_empty() {
        Event::Ignore
    } else {
        Event::PreEdit(paths)
    }
}

/// Best-effort list of files a shell command reads (`cat`, `sed -n`, `head`, `rg pattern file`, ...).
/// Non-file tokens are dropped later, when paths are checked against the working tree.
pub fn shell_reads(command: &str) -> Vec<String> {
    const READERS: &[&str] = &[
        "cat", "head", "tail", "less", "more", "bat", "nl", "sed", "grep", "rg", "awk", "wc",
        "diff",
    ];
    let mut out = Vec::new();
    let separated = crate::git::skeleton(command)
        .replace("&&", ";")
        .replace("||", ";");
    for segment in separated.split([';', '|', '\n', '(', ')']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .take_while(|t| !t.starts_with('>') && !t.starts_with('<'))
            .map(|t| t.trim_matches(|c| c == '"' || c == '\''))
            .collect();
        let Some(cmd) = words.first().map(|c| c.rsplit('/').next().unwrap_or(c)) else {
            continue;
        };
        let in_place = words
            .iter()
            .any(|w| w.starts_with("-i") || *w == "--in-place");
        if READERS.contains(&cmd) && !in_place {
            out.extend(
                words[1..]
                    .iter()
                    .filter(|w| !w.is_empty() && !w.starts_with('-'))
                    .map(|w| w.to_string()),
            );
        }
    }
    out.retain(|p| !p.contains(['$', '*', '`', '{']));
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_patch_headers() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: b.txt\n+hi\n*** Update File: c.rs\n*** Move to: d.rs\n*** Delete File: e.rs\n*** End Patch";
        assert_eq!(
            patch_paths(patch),
            vec!["src/a.rs", "b.txt", "c.rs", "d.rs", "e.rs"]
        );
    }

    #[test]
    fn finds_shell_writes() {
        assert_eq!(shell_writes("echo hi > a.txt 2>&1"), vec!["a.txt"]);
        assert_eq!(
            shell_writes("sed -i '' 's/a/b/' src/x.rs src/y.rs"),
            vec!["src/x.rs", "src/y.rs"]
        );
        assert_eq!(
            shell_writes("cat <<'EOF' > gen.rs\nfn f() -> Result<()> { a >> b }\nEOF"),
            vec!["gen.rs"]
        );
        assert_eq!(shell_writes("perl -pi -e 's/x/y/' f.pl"), vec!["f.pl"]);
        assert_eq!(
            shell_writes("cargo test 2>/dev/null | tee log.txt"),
            vec!["log.txt"]
        );
        assert_eq!(
            shell_writes("mv a.rs b.rs && rm -f c.rs"),
            vec!["a.rs", "b.rs", "c.rs"]
        );
        assert!(shell_writes("git commit -m 'fix -> thing' && ls -la").is_empty());
        assert!(shell_writes("grep -r foo src").is_empty());
    }

    #[test]
    fn finds_shell_reads() {
        assert_eq!(
            shell_reads("sed -n '1,80p' src/a.rs && cat b.rs | head -n 5"),
            vec!["5", "b.rs", "src/a.rs"]
        );
        assert_eq!(
            shell_reads("rg -n foo src/lib.rs"),
            vec!["foo", "src/lib.rs"]
        );
        assert!(shell_reads("sed -i 's/a/b/' x.rs").is_empty());
        assert!(shell_reads("cargo test").is_empty());
    }

    #[test]
    fn maps_harness_payloads() {
        let claude = json!({"hook_event_name": "PreToolUse", "tool_name": "Edit", "tool_input": {"file_path": "/r/a.rs"}, "agent_id": "sub1"});
        assert_eq!(
            parse("claude", &claude).unwrap(),
            (Event::PreEdit(vec!["/r/a.rs".into()]), Some("sub1".into()))
        );
        let codex = json!({"hook_event_name": "PreToolUse", "tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch"}});
        assert_eq!(
            parse("codex", &codex).unwrap(),
            (Event::PreEdit(vec!["a.rs".into()]), None)
        );
        let codex_shell = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: n.rs\n*** End Patch\nEOF"}});
        assert_eq!(
            parse("codex", &codex_shell).unwrap().0,
            Event::PreEdit(vec!["n.rs".into()])
        );
        let oc = json!({"event": "tool.execute.before", "tool": "write", "sessionID": "ses_1", "args": {"filePath": "x.ts"}});
        assert_eq!(
            parse("opencode", &oc).unwrap(),
            (Event::PreEdit(vec!["x.ts".into()]), Some("ses_1".into()))
        );
        let hermes = json!({"hook_event_name": "pre_tool_call", "tool_name": "patch", "tool_input": {"path": "h.py"}, "session_id": "s"});
        assert_eq!(
            parse("hermes", &hermes).unwrap().0,
            Event::PreEdit(vec!["h.py".into()])
        );
        let stop = json!({"hook_event_name": "Stop"});
        assert_eq!(parse("claude", &stop).unwrap().0, Event::End);
        let read = json!({"hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {"file_path": "a"}});
        assert_eq!(parse("claude", &read).unwrap().0, Event::Ignore);
    }
}
