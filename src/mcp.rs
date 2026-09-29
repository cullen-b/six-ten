use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::agent::Agent;
use crate::policy::{self, Decision};
use crate::store::Store;

const INSTRUCTIONS: &str = "six-ten coordinates file edits between several agents sharing this checkout. \
Before editing a file, claim it with six_ten_claim (edits may also be claimed automatically by hooks). If a file is \
held by another agent, work on other files and come back later, or call six_ten_wait to block until it is free. \
Never stash, reset, restore, clean, switch branches or pull while other agents are active.";

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
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
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

fn tools() -> Value {
    let paths = json!({"type": "array", "items": {"type": "string"}, "description": "File paths (absolute or relative to the repo)"});
    json!([
        {
            "name": "six_ten_claim",
            "description": "Claim files before editing them. Fails, naming the holder, if another agent is editing any of them; then work on other files or use six_ten_wait.",
            "inputSchema": {"type": "object", "properties": {"paths": paths}, "required": ["paths"]},
        },
        {
            "name": "six_ten_wait",
            "description": "Block until the given files are free, then claim them. Use only when you cannot make progress on other files.",
            "inputSchema": {"type": "object", "properties": {"paths": paths, "timeout_seconds": {"type": "integer", "description": "Max wait, default 300"}}, "required": ["paths"]},
        },
        {
            "name": "six_ten_release",
            "description": "Release your claims (all of them if paths is omitted) so other agents can edit those files.",
            "inputSchema": {"type": "object", "properties": {"paths": paths}},
        },
        {
            "name": "six_ten_status",
            "description": "Show which files each agent is editing or has left uncommitted.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ])
}
