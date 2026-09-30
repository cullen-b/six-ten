use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::policy::{self, Decision};
use crate::store::Store;

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
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let reply = match respond(&cwd, &agent, &msg) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
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
                Ok((text, ok)) => {
                    json!({"content": [{"type": "text", "text": text}], "isError": !ok})
                }
                Err(e) => {
                    json!({"content": [{"type": "text", "text": format!("six-ten error: {e:#}")}], "isError": true})
                }
            })
        }
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

fn call(cwd: &Path, agent: &Agent, name: &str, args: &Value) -> Result<(String, bool)> {
    let store = Store::open(cwd)?;
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
            policy::pre_edit(&store, agent, cwd, &paths)?,
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
    ];
    if !hooked {
        tools.insert(0, json!({
            "name": "six_ten_claim",
            "description": "Claim files right before editing them. Fails, naming the holder, if another agent is editing any of them; then work on other files.",
            "inputSchema": {"type": "object", "properties": {"paths": paths}, "required": ["paths"]},
        }));
    }
    Value::Array(tools)
}
