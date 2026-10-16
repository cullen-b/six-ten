use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::policy::{self, Decision};
use crate::store::{Store, now};

const WAKE: &str = "six_ten_wake";

const HOOKED: &str = "six-ten coordinates edits between agents sharing this checkout. Your edits are claimed \
automatically, so do not call six-ten tools before ordinary edits. If an edit is refused because another agent holds \
the file, work on other files and retry later; call six_ten_wait only when nothing else is left to do. Notes from \
six-ten are diffs of other agents' changes: account for them, and re-read a file only when told to. Never stash, reset, \
restore, clean, switch branches or pull while other agents are active.";

const UNHOOKED: &str = "six-ten coordinates edits between agents sharing this checkout. Your harness has no six-ten \
hooks, so claim files with six_ten_claim right before you edit them, and release them with six_ten_release when you \
are done. If a file is held by another agent, work on other files and retry later; call six_ten_wait only when \
nothing else is left to do. Never stash, reset, restore, clean, switch branches or pull while other agents are active.";

/// Harnesses whose hooks already claim edits get fewer tools, so agents don't spend calls on claims.
fn hooked(cwd: &Path, agent: &Agent) -> bool {
    Store::open(cwd).is_ok_and(|s| s.is_hooked(crate::store::harness_of(&agent.id)))
}

/// Serves MCP over stdio (newline-delimited JSON-RPC) until stdin closes.
pub fn serve() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let agent = Agent::current();
    // A reader thread keeps stdin flowing while six_ten_wake sleeps, so it sees cancellations.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut deferred: VecDeque<Value> = VecDeque::new();
    loop {
        let msg = match deferred.pop_front() {
            Some(msg) => msg,
            None => match rx.recv() {
                Ok(line) => match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                },
                Err(_) => return Ok(()),
            },
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let reply = if msg["method"] == "tools/call" && msg["params"]["name"] == WAKE {
            match wake(&cwd, &agent, &msg, &rx, &mut deferred) {
                Ok(Some((text, ok))) => tool_result(text, ok),
                Ok(None) => continue,
                Err(e) => tool_result(format!("six-ten error: {e:#}"), false),
            }
        } else {
            match respond(&cwd, &agent, &msg) {
                Ok(result) => result,
                Err((code, message)) => {
                    send(
                        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
                    )?;
                    continue;
                }
            }
        };
        send(&json!({"jsonrpc": "2.0", "id": id, "result": reply}))?;
    }
}

fn send(msg: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{msg}")?;
    stdout.flush()?;
    Ok(())
}

fn tool_result(text: String, ok: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": !ok})
}

/// Sleeps until the planned time, answering pings and sending progress so the harness doesn't time
/// out. `None` when the call was cancelled (no reply is due) or the client went away.
fn wake(
    cwd: &Path,
    agent: &Agent,
    msg: &Value,
    rx: &Receiver<String>,
    deferred: &mut VecDeque<Value>,
) -> Result<Option<(String, bool)>> {
    let args = &msg["params"]["arguments"];
    let plan = match crate::wake::plan(
        crate::store::harness_of(&agent.id),
        args["at"].as_str(),
        args["minutes"].as_f64(),
        args["provider"].as_str(),
    ) {
        Ok(plan) => plan,
        Err(reason) => return Ok(Some((reason, false))),
    };
    let task = args["task"].as_str().unwrap_or_default();
    // Cursor ends every MCP call after 60s and ignores progress, so long waits go through its shell.
    let harness = crate::store::harness_of(&agent.id).to_ascii_lowercase();
    if harness.contains("cursor") && plan.until > now() + 50 {
        return Ok(Some((
            format!(
                "Cursor stops MCP calls after 60 seconds, so this wait can't be a tool call. Run this in the \
                 terminal and let it finish; it blocks until it's time, then prints what to do:\n\
                 six-ten wake --at {} '{}'",
                plan.until,
                task.replace('\'', "'\\''")
            ),
            false,
        )));
    }
    let store = Store::open(cwd)?;
    crate::policy::sleep(&store, agent, &plan, task)?;
    let start = now();
    let token = msg["params"]["_meta"]["progressToken"].clone();
    while now() < plan.until {
        // Progress every 20s: some clients (OpenCode) time a call out after 60s without one.
        let left = Duration::from_secs((plan.until - now()).min(20));
        match rx.recv_timeout(left) {
            Ok(line) => {
                let Ok(other) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if other["method"] == "notifications/cancelled"
                    && other["params"]["requestId"] == msg["id"]
                {
                    store.log(&agent.id, "woke", "sleep cancelled".into())?;
                    return Ok(None);
                } else if other["method"] == "ping" {
                    send(&json!({"jsonrpc": "2.0", "id": other["id"], "result": {}}))?;
                } else {
                    deferred.push_back(other);
                }
            }
            Err(RecvTimeoutError::Timeout) if !token.is_null() && now() < plan.until => {
                send(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": now() - start,
                        "total": plan.until.saturating_sub(start),
                        "message": format!("sleeping, {} left", crate::wake::span(plan.until.saturating_sub(now()))),
                    },
                }))?
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(None),
        }
    }
    Ok(Some((
        crate::policy::woke(&store, agent, &plan, task)?,
        true,
    )))
}

fn respond(cwd: &Path, agent: &Agent, msg: &Value) -> Result<Value, (i64, String)> {
    let params = &msg["params"];
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "six-ten", "version": env!("CARGO_PKG_VERSION")},
            "instructions": if hooked(cwd, agent) { HOOKED } else { UNHOOKED },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools(hooked(cwd, agent))})),
        "tools/call" => {
            let args = &params["arguments"];
            let result = call(
                cwd,
                agent,
                params["name"].as_str().unwrap_or_default(),
                args,
            );
            Ok(match result {
                Ok((text, ok)) => tool_result(text, ok),
                Err(e) => tool_result(format!("six-ten error: {e:#}"), false),
            })
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

fn call(cwd: &Path, agent: &Agent, name: &str, args: &Value) -> Result<(String, bool)> {
    let store = Store::open(cwd)?;
    store.mark_present(agent)?;
    let paths: Vec<String> = args["paths"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(match name {
        "six_ten_status" => (crate::status_text(&store, agent)?, true),
        "six_ten_claim" => reply(
            policy::pre_edit_with_reason(&store, agent, cwd, &paths, args["reason"].as_str())?,
            format!("claimed: {}", paths.join(", ")),
        ),
        "six_ten_wait" => {
            let secs = args["timeout_seconds"].as_u64().unwrap_or(300).min(1800);
            reply(
                policy::wait(&store, agent, cwd, &paths, Duration::from_secs(secs))?,
                format!("free and claimed for you: {}", paths.join(", ")),
            )
        }
        "six_ten_release" => {
            let only = (!paths.is_empty()).then(|| {
                paths
                    .iter()
                    .filter_map(|p| store.normalize(cwd, p))
                    .collect::<Vec<_>>()
            });
            let released = store.release(&agent.id, only.as_deref())?;
            (
                format!(
                    "released {} lease(s): {}",
                    released.len(),
                    released.join(", ")
                ),
                true,
            )
        }
        "six_ten_decide" => {
            let text = args["text"].as_str().unwrap_or_default().trim().to_string();
            if text.is_empty() {
                ("give the decision to record".to_string(), false)
            } else {
                match store.log(&agent.id, "decision", text.clone()) {
                    Ok(()) => (format!("recorded decision: {text}"), true),
                    Err(e) => (format!("six-ten error: {e:#}"), false),
                }
            }
        }
        other => (format!("unknown tool {other}"), false),
    })
}

fn reply(decision: Decision, ok: String) -> (String, bool) {
    match decision.granted_text(ok) {
        Ok(text) => (text, true),
        Err(reason) => (reason, false),
    }
}

fn tools(hooked: bool) -> Value {
    let paths = json!({"type": "array", "items": {"type": "string"}, "description": "File paths (absolute or relative to the repo)"});
    let mut tools = vec![
        json!({
            "name": "six_ten_wait",
            "description": "Block until the given files are free, then claim them. Only for files you were refused and when nothing else is left to do.",
            "inputSchema": {"type": "object", "properties": {"paths": paths, "timeout_seconds": {"type": "integer", "description": "Max wait, default 300"}}, "required": ["paths"]},
        }),
        json!({
            "name": "six_ten_release",
            "description": "Release your claims early (all of them if paths is omitted). They are released automatically when your turn ends.",
            "inputSchema": {"type": "object", "properties": {"paths": paths}},
        }),
        json!({
            "name": "six_ten_status",
            "description": "Who is editing what. Useful for choosing what to work on; never needed before an edit.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
        json!({
            "name": WAKE,
            "description": "Sleep, then get told to start working. Use when the user wants work to start later, e.g. when their usage limit resets. With no time given it wakes when your Claude Code or Codex usage window resets; if six-ten has no usage data, ask the user when their limit resets and pass `at`. Commit first: your file claims are released while you sleep. Don't retry if it times out; tell the user.",
            "inputSchema": {"type": "object", "properties": {
                "at": {"type": "string", "description": "Local time (\"2:30am\", \"14:05\"), RFC 3339, or Unix seconds"},
                "minutes": {"type": "number", "description": "Wake after this many minutes instead"},
                "provider": {"type": "string", "enum": ["claude", "codex"], "description": "Whose usage reset to wait for; default: your harness"},
                "task": {"type": "string", "description": "What to work on after waking; repeated back to you"},
            }},
        }),
        json!({
            "name": "six_ten_decide",
            "description": "Record a decision (e.g. why approach X beat Y) so it outlives the chat session. Shown in status and watch.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string", "description": "The decision to record"}}, "required": ["text"]},
        }),
    ];
    if !hooked {
        tools.insert(0, json!({
            "name": "six_ten_claim",
            "description": "Claim files right before editing them. Fails, naming the holder, if another agent is editing any of them; then work on other files.",
            "inputSchema": {"type": "object", "properties": {"paths": paths, "reason": {"type": "string", "description": "Why you are editing; shown to agents you block"}}, "required": ["paths"]},
        }));
    }
    Value::Array(tools)
}
