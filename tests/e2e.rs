use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_six-ten");

fn repo() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "six-ten-e2e-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "dev"]);
    fs::write(dir.join("src/a.rs"), "a\n").unwrap();
    fs::write(dir.join("src/b.rs"), "b\n").unwrap();
    git(&dir, &["add", "."]);
    git(
        &dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    dir
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn six(dir: &Path, agent: &str, args: &[&str]) -> Output {
    Command::new(BIN)
        .current_dir(dir)
        .env("SIX_TEN_AGENT", agent)
        .args(args)
        .output()
        .unwrap()
}

fn hook(dir: &Path, agent: &str, harness: &str, payload: Value) -> Output {
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .env("SIX_TEN_AGENT", agent)
        .args(["hook", harness])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn edit(dir: &Path, file: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Edit", "cwd": dir, "tool_input": {"file_path": dir.join(file)}})
}

fn bash(dir: &Path, command: &str) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "cwd": dir, "tool_input": {"command": command}})
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn second_agent_is_blocked_until_release() {
    let dir = repo();
    assert_eq!(
        six(&dir, "alice", &["claim", "src/a.rs"]).status.code(),
        Some(0)
    );
    let blocked = six(&dir, "bob", &["claim", "src/a.rs"]);
    assert_eq!(blocked.status.code(), Some(2));
    assert!(
        stderr(&blocked).contains("`src/a.rs` is being edited by alice"),
        "{}",
        stderr(&blocked)
    );
    assert_eq!(
        six(&dir, "bob", &["claim", "src/b.rs"]).status.code(),
        Some(0),
        "other files stay available"
    );
    assert_eq!(
        six(&dir, "alice", &["claim", "./src/../src/a.rs"])
            .status
            .code(),
        Some(0),
        "re-claim refreshes"
    );
    six(&dir, "alice", &["release"]);
    assert_eq!(
        six(&dir, "bob", &["claim", "src/a.rs"]).status.code(),
        Some(0)
    );
    let status = String::from_utf8(six(&dir, "carol", &["status"]).stdout).unwrap();
    assert!(
        status.contains("src/a.rs  bob") && status.contains("src/b.rs  bob"),
        "{status}"
    );
}

#[test]
fn claim_reason_is_shown_to_blocked_agents() {
    let dir = repo();
    assert_eq!(
        six(&dir, "alice", &["claim", "src/a.rs", "-m", "JWT migration"])
            .status
            .code(),
        Some(0)
    );
    let blocked = six(&dir, "bob", &["claim", "src/a.rs"]);
    assert_eq!(blocked.status.code(), Some(2));
    assert!(
        stderr(&blocked).contains("JWT migration"),
        "{}",
        stderr(&blocked)
    );
    let status = String::from_utf8(six(&dir, "carol", &["status"]).stdout).unwrap();
    assert!(status.contains("JWT migration"), "{status}");
    // Claims without a reason still work and old leases stay readable.
    assert_eq!(
        six(&dir, "alice", &["claim", "src/b.rs"]).status.code(),
        Some(0)
    );
    let blocked = six(&dir, "bob", &["claim", "src/b.rs"]);
    assert_eq!(blocked.status.code(), Some(2));
    assert!(
        stderr(&blocked).contains("`src/b.rs` is being edited by alice"),
        "{}",
        stderr(&blocked)
    );
}

#[test]
fn decide_records_an_event_visible_in_status() {
    let dir = repo();
    let out = six(
        &dir,
        "alice",
        &["decide", "Chose", "JWT", "over", "sessions"],
    );
    assert_eq!(out.status.code(), Some(0));
    let status = String::from_utf8(six(&dir, "bob", &["status"]).stdout).unwrap();
    assert!(status.contains("Chose JWT over sessions"), "{status}");
    assert_eq!(
        six(&dir, "alice", &["decide"]).status.code(),
        Some(1),
        "empty decisions are rejected"
    );
}

#[test]
fn concurrent_claims_have_exactly_one_winner() {
    let dir = repo();
    let children: Vec<_> = (0..16)
        .map(|i| {
            Command::new(BIN)
                .current_dir(&dir)
                .env("SIX_TEN_AGENT", format!("agent{i}"))
                .args(["claim", "src/a.rs", "src/new.rs"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let winners = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .filter(|o| o.status.success())
        .count();
    assert_eq!(winners, 1);
}

#[test]
fn expired_lease_can_be_taken() {
    let dir = repo();
    let out = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "alice")
        .env("SIX_TEN_TTL", "1")
        .args(["claim", "src/a.rs"])
        .output()
        .unwrap();
    assert!(out.status.success());
    std::thread::sleep(std::time::Duration::from_millis(2100));
    assert_eq!(
        six(&dir, "bob", &["claim", "src/a.rs"]).status.code(),
        Some(0)
    );
}

#[test]
fn wait_returns_once_holder_releases() {
    let dir = repo();
    six(&dir, "alice", &["claim", "src/a.rs"]);
    let waiter = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "bob")
        .args(["wait", "src/a.rs", "--timeout", "10"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    six(&dir, "alice", &["release", "src/a.rs"]);
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        six(&dir, "alice", &["claim", "src/a.rs"]).status.code(),
        Some(2),
        "bob now holds it"
    );
}

#[test]
fn claude_hook_blocks_edits_and_shell_workarounds() {
    let dir = repo();
    assert_eq!(
        hook(&dir, "alice", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(0)
    );
    let blocked = hook(&dir, "bob", "claude", edit(&dir, "src/a.rs"));
    assert_eq!(blocked.status.code(), Some(2));
    assert!(stderr(&blocked).contains("Work on other files"));
    assert_eq!(
        hook(&dir, "bob", "claude", edit(&dir, "src/b.rs"))
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        hook(
            &dir,
            "bob",
            "claude",
            bash(&dir, "sed -i '' 's/a/z/' src/a.rs")
        )
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        hook(&dir, "bob", "claude", bash(&dir, "echo z > src/a.rs"))
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        hook(&dir, "bob", "claude", bash(&dir, "cargo build 2>&1 | tail"))
            .status
            .code(),
        Some(0)
    );
    // A subagent of alice may edit what alice holds; bob's subagent may not.
    let mut sub = edit(&dir, "src/a.rs");
    sub["agent_id"] = json!("helper");
    assert_eq!(
        hook(&dir, "alice", "claude", sub.clone()).status.code(),
        Some(0)
    );
    assert_eq!(hook(&dir, "bob", "claude", sub).status.code(), Some(2));
    // End of alice's turn frees the file.
    hook(
        &dir,
        "alice",
        "claude",
        json!({"hook_event_name": "Stop", "cwd": dir}),
    );
    assert_eq!(
        hook(&dir, "bob", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn git_guard_protects_other_agents_uncommitted_work() {
    let dir = repo();
    hook(&dir, "alice", "claude", edit(&dir, "src/a.rs"));
    fs::write(dir.join("src/a.rs"), "alice's work\n").unwrap();
    hook(
        &dir,
        "alice",
        "claude",
        json!({"hook_event_name": "Stop", "cwd": dir}),
    );
    for cmd in [
        "git stash",
        "git checkout -- src/a.rs",
        "git reset --hard",
        "git restore src",
        "git clean -fd",
    ] {
        let out = hook(&dir, "bob", "claude", bash(&dir, cmd));
        assert_eq!(out.status.code(), Some(2), "{cmd} should be blocked");
        assert!(
            stderr(&out).contains("src/a.rs (alice, uncommitted)"),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(
        hook(
            &dir,
            "bob",
            "claude",
            bash(&dir, "git checkout -- src/b.rs")
        )
        .status
        .code(),
        Some(0)
    );
    assert_eq!(
        hook(&dir, "alice", "claude", bash(&dir, "git stash"))
            .status
            .code(),
        Some(0),
        "own work is fine"
    );
}

#[test]
fn precommit_rejects_other_agents_files() {
    let dir = repo();
    hook(&dir, "alice", "claude", edit(&dir, "src/a.rs"));
    fs::write(dir.join("src/a.rs"), "alice\n").unwrap();
    hook(&dir, "bob", "claude", edit(&dir, "src/b.rs"));
    fs::write(dir.join("src/b.rs"), "bob\n").unwrap();
    git(&dir, &["add", "-A"]);
    let out = six(&dir, "bob", &["precommit"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("src/a.rs (being edited by alice)"),
        "{}",
        stderr(&out)
    );
    git(&dir, &["restore", "--staged", "src/a.rs"]);
    assert_eq!(six(&dir, "bob", &["precommit"]).status.code(), Some(0));
}

#[test]
fn codex_opencode_and_hermes_hooks_block() {
    let dir = repo();
    six(&dir, "alice", &["claim", "src/a.rs"]);
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch";
    let codex = json!({"hook_event_name": "PreToolUse", "tool_name": "apply_patch", "cwd": dir, "tool_input": {"command": patch}});
    assert_eq!(hook(&dir, "bob", "codex", codex).status.code(), Some(2));
    let oc = json!({"event": "tool.execute.before", "tool": "edit", "sessionID": "s1", "directory": dir, "args": {"filePath": dir.join("src/a.rs")}});
    assert_eq!(hook(&dir, "bob", "opencode", oc).status.code(), Some(2));
    let hermes = json!({"hook_event_name": "pre_tool_call", "tool_name": "write_file", "cwd": dir, "session_id": "h", "tool_input": {"path": "src/a.rs"}});
    let out = hook(&dir, "bob", "hermes", hermes);
    assert_eq!(out.status.code(), Some(0));
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reply["decision"], "block");
}

#[test]
fn hooks_allow_outside_git_and_on_bad_input() {
    let dir = std::env::temp_dir().join(format!("six-ten-nogit-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        hook(&dir, "a", "claude", edit(&dir, "x.rs")).status.code(),
        Some(0)
    );
    let mut child = Command::new(BIN)
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"not json").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn mcp_server_speaks_json_rpc() {
    let dir = repo();
    six(&dir, "alice", &["claim", "src/a.rs"]);
    let mut child = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "bob")
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "six_ten_claim", "arguments": {"paths": ["src/a.rs"]}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "six_ten_claim", "arguments": {"paths": ["src/b.rs"]}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "six_ten_status", "arguments": {}}}),
    ];
    let mut stdin = child.stdin.take().unwrap();
    for r in &requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), 5, "notification gets no reply");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "six-ten");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 5);
    assert_eq!(replies[2]["result"]["isError"], true);
    assert!(
        replies[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("alice")
    );
    assert_eq!(replies[3]["result"]["isError"], false);
    assert!(
        replies[4]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("src/b.rs  bob (you)")
    );
}

#[test]
fn install_is_idempotent() {
    let dir = repo();
    fs::write(dir.join("CLAUDE.md"), "# Project\n\nExisting notes.\n").unwrap();
    // Keep the real Hermes install out of this: temp home, and no `hermes` on PATH.
    let hermes_home = dir.join(".hermes-test");
    for _ in 0..2 {
        let out = Command::new(BIN)
            .current_dir(&dir)
            .env("HERMES_HOME", &hermes_home)
            .env("PATH", "/usr/bin:/bin")
            .args(["install", "all"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
    }
    assert!(hermes_home.join("plugins/six-ten/__init__.py").exists());
    let codex_config = fs::read_to_string(dir.join(".codex/config.toml")).unwrap();
    assert!(
        codex_config.starts_with("default_permissions = \"six-ten\"\n"),
        "{codex_config}"
    );
    assert_eq!(
        codex_config.matches("\".git/hooks/**\" = \"read\"").count(),
        1
    );
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    assert_eq!(
        settings["permissions"]["allow"],
        json!(["mcp__six-ten", "Bash(six-ten:*)"])
    );
    let claude_md = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
    assert!(claude_md.starts_with("# Project") && claude_md.matches("six-ten:begin").count() == 1);
    let codex = fs::read_to_string(dir.join(".codex/hooks.json")).unwrap();
    assert!(codex.contains("six-ten hook codex"));
    assert!(
        fs::read_to_string(dir.join(".codex/config.toml"))
            .unwrap()
            .matches("[mcp_servers.six-ten]")
            .count()
            == 1
    );
    assert!(dir.join(".opencode/plugins/six-ten.ts").exists());
    let opencode: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
    assert_eq!(
        opencode["mcp"]["six-ten"]["command"],
        json!(["six-ten", "mcp"])
    );
    let precommit = fs::read_to_string(dir.join(".git/hooks/pre-commit")).unwrap();
    assert_eq!(precommit.matches("six-ten precommit").count(), 1);
    assert_eq!(
        fs::read_to_string(dir.join("AGENTS.md"))
            .unwrap()
            .matches("six-ten:begin")
            .count(),
        1
    );
}

fn tool(dir: &Path, event: &str, tool: &str, file: &str) -> Value {
    json!({"hook_event_name": event, "tool_name": tool, "cwd": dir, "tool_input": {"file_path": dir.join(file)}})
}

/// A full edit by `agent` through the Claude hooks: pre-check, write, post-record, end of turn.
fn edit_as(dir: &Path, agent: &str, file: &str, content: &str) {
    assert_eq!(
        hook(dir, agent, "claude", tool(dir, "PreToolUse", "Edit", file))
            .status
            .code(),
        Some(0)
    );
    fs::write(dir.join(file), content).unwrap();
    hook(dir, agent, "claude", tool(dir, "PostToolUse", "Edit", file));
    stop_uncommitted(dir, agent);
}

/// Ends `agent`'s turn without committing: the first Stop is sent back once with a reminder.
fn stop_uncommitted(dir: &Path, agent: &str) {
    let stop = json!({"hook_event_name": "Stop", "cwd": dir});
    if hook(dir, agent, "claude", stop.clone()).status.code() == Some(2) {
        assert_eq!(hook(dir, agent, "claude", stop).status.code(), Some(0));
    }
}

fn context(o: &Output) -> String {
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn stale_target_is_refused_once_with_a_diff() {
    let dir = repo();
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "bob", "src/a.rs", "a\nbob was here\n");
    let first = hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Edit", "src/a.rs"),
    );
    assert_eq!(first.status.code(), Some(2));
    let msg = stderr(&first);
    assert!(
        msg.contains("you now hold `src/a.rs`")
            && msg.contains("+bob was here")
            && msg.contains("changed by bob"),
        "{msg}"
    );
    let retry = hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Edit", "src/a.rs"),
    );
    assert_eq!(retry.status.code(), Some(0));
    assert!(retry.stdout.is_empty());
}

#[test]
fn changes_to_files_read_earlier_arrive_as_a_note_once() {
    let dir = repo();
    fs::write(dir.join("src/api.rs"), "pub fn foo(a: u8) {}\n").unwrap();
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/api.rs"),
    );
    edit_as(&dir, "bob", "src/api.rs", "pub fn foo(a: u8, b: u8) {}\n");
    let out = hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Write", "src/main.rs"),
    );
    assert_eq!(out.status.code(), Some(0));
    let note = context(&out);
    assert!(
        note.contains("files you read earlier")
            && note.contains("+pub fn foo(a: u8, b: u8) {}")
            && note.contains("-pub fn foo(a: u8) {}"),
        "{note}"
    );
    let again = hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Write", "src/main.rs"),
    );
    assert!(again.stdout.is_empty(), "told only once");
}

#[test]
fn own_and_family_edits_are_not_reported() {
    let dir = repo();
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "alice", "src/a.rs", "mine\n");
    let mut sub = tool(&dir, "PreToolUse", "Edit", "src/b.rs");
    sub["agent_id"] = json!("helper");
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/b.rs"),
    );
    assert_eq!(
        hook(&dir, "alice", "claude", sub.clone()).status.code(),
        Some(0)
    );
    fs::write(dir.join("src/b.rs"), "helper\n").unwrap();
    sub["hook_event_name"] = json!("PostToolUse");
    hook(&dir, "alice", "claude", sub);
    let sub_stop = json!({"hook_event_name": "SubagentStop", "cwd": dir, "agent_id": "helper"});
    hook(&dir, "alice", "claude", sub_stop);
    let out = hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Edit", "src/b.rs"),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn turn_start_and_wait_deliver_pending_changes() {
    let dir = repo();
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "bob", "src/a.rs", "a\nbob 1\n");
    let turn = hook(
        &dir,
        "alice",
        "claude",
        json!({"hook_event_name": "UserPromptSubmit", "cwd": dir, "prompt": "go"}),
    );
    let v: Value = serde_json::from_slice(&turn.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(context(&turn).contains("+bob 1"));

    // Bob holds the file and changes it while alice waits; the wait result carries the diff.
    assert_eq!(
        hook(
            &dir,
            "bob",
            "claude",
            tool(&dir, "PreToolUse", "Edit", "src/a.rs")
        )
        .status
        .code(),
        Some(0)
    );
    let waiter = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "alice")
        .args(["wait", "src/a.rs", "--timeout", "10"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    fs::write(dir.join("src/a.rs"), "a\nbob 1\nbob 2\n").unwrap();
    hook(
        &dir,
        "bob",
        "claude",
        tool(&dir, "PostToolUse", "Edit", "src/a.rs"),
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    hook(
        &dir,
        "bob",
        "claude",
        json!({"hook_event_name": "Stop", "cwd": dir}),
    );
    let out = waiter.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("claimed src/a.rs") && text.contains("+bob 2"),
        "{text}"
    );
    assert_eq!(
        hook(
            &dir,
            "alice",
            "claude",
            tool(&dir, "PreToolUse", "Edit", "src/a.rs")
        )
        .status
        .code(),
        Some(0),
        "wait already told alice"
    );
}

#[test]
fn unread_changes_are_listed_once_and_big_diffs_are_summarized() {
    let dir = repo();
    hook(
        &dir,
        "alice",
        "claude",
        json!({"hook_event_name": "UserPromptSubmit", "cwd": dir, "prompt": "go"}),
    );
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "bob", "src/b.rs", "b changed\n");
    let big: String = (0..300).map(|i| format!("line {i}\n")).collect();
    edit_as(&dir, "bob", "src/a.rs", &big);
    let note = context(&hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PreToolUse", "Write", "src/new.rs"),
    ));
    assert!(note.contains("during your turn: src/b.rs"), "{note}");
    assert!(
        note.contains("changed substantially (+300/-1 lines)"),
        "{note}"
    );
    assert!(
        hook(
            &dir,
            "alice",
            "claude",
            tool(&dir, "PreToolUse", "Write", "src/new2.rs")
        )
        .stdout
        .is_empty()
    );
}

#[test]
fn codex_shell_reads_and_other_harness_note_channels() {
    let dir = repo();
    let codex_read = json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "cwd": dir, "tool_input": {"command": "sed -n '1,40p' src/a.rs"}});
    hook(&dir, "cx", "codex", codex_read);
    let oc_read = json!({"event": "tool.execute.after", "tool": "read", "sessionID": "s", "directory": dir, "args": {"filePath": dir.join("src/a.rs")}});
    hook(&dir, "oc", "opencode", oc_read);
    let hermes_read = json!({"hook_event_name": "post_tool_call", "tool_name": "read_file", "cwd": dir, "session_id": "h", "tool_input": {"path": "src/a.rs"}});
    hook(&dir, "hm", "hermes", hermes_read);
    edit_as(&dir, "bob", "src/a.rs", "a\nfrom bob\n");

    let codex = hook(
        &dir,
        "cx",
        "codex",
        json!({"hook_event_name": "PreToolUse", "tool_name": "apply_patch", "cwd": dir,
        "tool_input": {"command": "*** Begin Patch\n*** Add File: src/c.rs\n+x\n*** End Patch"}}),
    );
    assert!(context(&codex).contains("+from bob"));
    let oc = hook(
        &dir,
        "oc",
        "opencode",
        json!({"event": "tool.execute.before", "tool": "write", "sessionID": "s", "directory": dir, "args": {"filePath": "src/d.rs"}}),
    );
    let v: Value = serde_json::from_slice(&oc.stdout).unwrap();
    assert!(v["note"].as_str().unwrap().contains("+from bob"));
    let hermes_write = json!({"hook_event_name": "pre_tool_call", "tool_name": "write_file", "cwd": dir, "session_id": "h", "tool_input": {"path": "src/e.rs"}});
    let first: Value =
        serde_json::from_slice(&hook(&dir, "hm", "hermes", hermes_write.clone()).stdout).unwrap();
    assert_eq!(first["decision"], "block");
    assert!(
        first["reason"]
            .as_str()
            .unwrap()
            .contains("Retry the same call now")
    );
    assert!(
        hook(&dir, "hm", "hermes", hermes_write).stdout.is_empty(),
        "retry passes"
    );
}

#[test]
fn news_arrives_after_any_tool_call() {
    let dir = repo();
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "bob", "src/a.rs", "a\nbob\n");
    let after_shell = hook(
        &dir,
        "alice",
        "claude",
        json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "cwd": dir, "tool_input": {"command": "cargo check"}}),
    );
    let v: Value = serde_json::from_slice(&after_shell.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(context(&after_shell).contains("+bob"));
    assert!(
        hook(
            &dir,
            "alice",
            "claude",
            tool(&dir, "PreToolUse", "Edit", "src/b.rs")
        )
        .stdout
        .is_empty(),
        "already told"
    );
    let hermes_after = json!({"hook_event_name": "post_tool_call", "tool_name": "terminal", "cwd": dir, "session_id": "h", "tool_input": {"command": "ls"}});
    assert!(hook(&dir, "hm", "hermes", hermes_after).stdout.is_empty());
}

#[test]
fn rewrite_after_external_revert_is_still_news() {
    let dir = repo();
    edit_as(&dir, "bob", "src/a.rs", "a\nnew\n");
    git(&dir, &["checkout", "-q", "--", "src/a.rs"]);
    hook(
        &dir,
        "alice",
        "claude",
        tool(&dir, "PostToolUse", "Read", "src/a.rs"),
    );
    edit_as(&dir, "bob", "src/a.rs", "a\nnew\n");
    assert!(
        context(&hook(
            &dir,
            "alice",
            "claude",
            tool(&dir, "PreToolUse", "Write", "src/z.rs")
        ))
        .contains("+new")
    );
}

#[test]
fn hermes_plugin_mode_blocks_by_exit_code_and_returns_notes() {
    let dir = repo();
    six(&dir, "alice", &["claim", "src/a.rs"]);
    let write = |path: &str| json!({"hook_event_name": "pre_tool_call", "tool_name": "write_file", "cwd": dir, "session_id": "h", "tool_input": {"path": path}});
    let blocked = hook(&dir, "hm", "hermes-plugin", write("src/a.rs"));
    assert_eq!(blocked.status.code(), Some(2));
    assert!(stderr(&blocked).contains("being edited by alice"));
    six(&dir, "alice", &["release"]);
    hook(
        &dir,
        "hm",
        "hermes-plugin",
        json!({"hook_event_name": "post_tool_call", "tool_name": "read_file", "cwd": dir, "session_id": "h", "tool_input": {"path": "src/b.rs"}}),
    );
    edit_as(&dir, "bob", "src/b.rs", "b\nbob\n");
    let after = hook(
        &dir,
        "hm",
        "hermes-plugin",
        json!({"hook_event_name": "post_tool_call", "tool_name": "terminal", "cwd": dir, "session_id": "h", "tool_input": {"command": "ls"}}),
    );
    let v: Value = serde_json::from_slice(&after.stdout).unwrap();
    assert!(
        v["note"].as_str().unwrap().contains("+bob"),
        "post-tool news is delivered, not blocked"
    );
}

fn events(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join(".git/six-ten/events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn events_explain_blocks_and_how_they_were_resolved() {
    let dir = repo();
    six(&dir, "alice", &["claim", "src/a.rs"]);
    assert_eq!(
        hook(&dir, "bob", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        hook(&dir, "bob", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(2),
        "retries are not re-logged"
    );
    assert_eq!(
        hook(&dir, "bob", "claude", edit(&dir, "src/b.rs"))
            .status
            .code(),
        Some(0)
    );
    six(&dir, "alice", &["release"]);
    let out = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "bob")
        .args(["wait", "src/a.rs", "--timeout", "5"])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Carol is blocked and ends her turn without getting the file.
    assert_eq!(
        hook(&dir, "carol", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(2)
    );
    hook(
        &dir,
        "carol",
        "claude",
        json!({"hook_event_name": "Stop", "cwd": dir}),
    );
    fs::write(dir.join("src/b.rs"), "bob's work\n").unwrap();
    assert_eq!(
        hook(&dir, "carol", "claude", bash(&dir, "git stash"))
            .status
            .code(),
        Some(2)
    );

    let log = events(&dir);
    let kinds: Vec<&str> = log.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["blocked", "resolved", "blocked", "unresolved", "refused"],
        "{log:#?}"
    );
    let resolved = log[1]["text"].as_str().unwrap();
    assert!(
        resolved.contains("got src/a.rs")
            && resolved.contains("edited 1 other file meanwhile")
            && resolved.contains("(alice held it)"),
        "{resolved}"
    );
    assert!(
        log[3]["text"]
            .as_str()
            .unwrap()
            .contains("without editing src/a.rs (held by bob)")
    );
    assert!(log[4]["text"].as_str().unwrap().contains("`git stash`"));
    let status = String::from_utf8(six(&dir, "dave", &["status"]).stdout).unwrap();
    assert!(
        status.contains("recent:") && status.contains("✅ bob"),
        "{status}"
    );
    assert!(
        String::from_utf8(six(&dir, "x", &["notify", "on"]).stdout)
            .unwrap()
            .contains("on")
    );
    assert!(dir.join(".git/six-ten/notify").exists());
    six(&dir, "x", &["notify", "off"]);
    assert!(!dir.join(".git/six-ten/notify").exists());
}

#[test]
fn an_agent_may_commit_and_stash_its_own_subagents_work() {
    let dir = repo();
    let mut sub = edit(&dir, "src/a.rs");
    sub["agent_id"] = json!("helper");
    assert_eq!(hook(&dir, "alice", "claude", sub).status.code(), Some(0));
    fs::write(dir.join("src/a.rs"), "helper's work\n").unwrap();
    git(&dir, &["add", "src/a.rs"]);
    assert_eq!(six(&dir, "alice", &["precommit"]).status.code(), Some(0));
    assert_eq!(
        hook(&dir, "alice", "claude", bash(&dir, "git stash"))
            .status
            .code(),
        Some(0)
    );
    let refused = six(&dir, "bob", &["precommit"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        !stderr(&refused).contains("SIX_TEN_ALLOW_COMMIT"),
        "the override is for humans, not advertised to agents"
    );
    let forced = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "bob")
        .env("SIX_TEN_ALLOW_COMMIT", "1")
        .arg("precommit")
        .output()
        .unwrap();
    assert!(forced.status.success());
    assert_eq!(events(&dir).last().unwrap()["kind"], "override");
}

fn mcp_session(dir: &Path, agent: &str) -> Vec<Value> {
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .env("SIX_TEN_AGENT", agent)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    )
    .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn mcp_drops_claim_for_harnesses_whose_hooks_claim_edits() {
    let dir = repo();
    let names = |r: &Value| -> Vec<String> {
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let before = mcp_session(&dir, "cursor:1");
    assert!(names(&before[1]).contains(&"six_ten_claim".to_string()));
    assert!(
        before[0]["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("claim files with six_ten_claim")
    );
    // The first hook run from this harness marks it as hooked.
    hook(
        &dir,
        "cursor:1",
        "generic",
        json!({"event": "pre_edit", "cwd": dir, "paths": ["src/a.rs"]}),
    );
    let after = mcp_session(&dir, "cursor:2");
    assert_eq!(
        names(&after[1]),
        [
            "six_ten_wait",
            "six_ten_release",
            "six_ten_status",
            "six_ten_decide"
        ]
    );
    assert!(
        after[0]["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("claimed automatically")
    );
}

#[test]
fn generic_contract_covers_the_full_lifecycle() {
    let dir = repo();
    let g = |agent: &str, v: Value| hook(&dir, agent, "generic", v);
    g("new", json!({"event": "turn_start", "cwd": dir}));
    g(
        "new",
        json!({"event": "post_read", "cwd": dir, "paths": ["src/a.rs"]}),
    );
    assert_eq!(
        g(
            "bob",
            json!({"event": "pre_edit", "cwd": dir, "paths": ["src/a.rs"]})
        )
        .status
        .code(),
        Some(0)
    );
    assert_eq!(
        g(
            "new",
            json!({"event": "pre_edit", "cwd": dir, "paths": ["src/a.rs"]})
        )
        .status
        .code(),
        Some(2)
    );
    fs::write(dir.join("src/a.rs"), "a\nbob\n").unwrap();
    g(
        "bob",
        json!({"event": "post_write", "cwd": dir, "paths": ["src/a.rs"]}),
    );
    g("bob", json!({"event": "end", "cwd": dir}));
    let note = g(
        "new",
        json!({"event": "post_shell", "cwd": dir, "command": "ls"}),
    );
    let v: Value = serde_json::from_slice(&note.stdout).unwrap();
    assert!(v["note"].as_str().unwrap().contains("+bob"));
    assert_eq!(
        g(
            "new",
            json!({"event": "pre_shell", "cwd": dir, "command": "echo x > src/a.rs"})
        )
        .status
        .code(),
        Some(0)
    );
}

#[test]
fn global_install_keeps_existing_user_config() {
    let home = std::env::temp_dir().join(format!("six-ten-home-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let cfg = home.join(".config");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(cfg.join("opencode")).unwrap();
    fs::write(home.join(".claude/settings.json"), r#"{"model": "x", "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "mine"}]}]}}"#).unwrap();
    fs::write(
        home.join(".codex/config.toml"),
        "sandbox_mode = \"workspace-write\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
    )
    .unwrap();
    fs::write(cfg.join("opencode/opencode.jsonc"), "{\n  // my comment\n  \"mcp\": {\n    \"other\": {\"type\": \"local\", \"command\": [\"x\"]}\n  }\n}\n").unwrap();
    let run = || {
        Command::new(BIN)
            .args(["install", "all", "--global"])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &cfg)
            .env("CODEX_HOME", home.join(".codex"))
            .env("HERMES_HOME", home.join(".hermes"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap()
    };
    for _ in 0..2 {
        let out = run();
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["model"], "x");
    let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "user's own hook kept, ours added once");
    assert!(pre[0].to_string().contains("mine"));
    assert!(
        fs::read_to_string(home.join(".claude/CLAUDE.md"))
            .unwrap()
            .contains("six-ten:begin")
    );
    let codex = fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(
        codex.starts_with("default_permissions = \"six-ten\"\n")
            && codex.contains("[mcp_servers.other]")
    );
    assert_eq!(codex.matches("[mcp_servers.six-ten]").count(), 1);
    assert!(
        fs::read_to_string(home.join(".codex/hooks.json"))
            .unwrap()
            .contains("six-ten hook codex")
    );
    let jsonc = fs::read_to_string(cfg.join("opencode/opencode.jsonc")).unwrap();
    assert!(
        jsonc.contains("// my comment") && jsonc.matches("\"six-ten\":").count() == 1,
        "{jsonc}"
    );
    let stripped: String = jsonc
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed: Value =
        serde_json::from_str(&stripped).expect("still valid JSON once comments are removed");
    assert_eq!(
        parsed["mcp"]["six-ten"]["command"],
        json!(["six-ten", "mcp"])
    );
    assert!(cfg.join("opencode/plugins/six-ten.ts").exists());
    assert!(home.join(".hermes/plugins/six-ten/__init__.py").exists());
    assert!(cfg.join("six-ten/hooked/codex").exists());

    // A repo with no per-repo install still gets the lean MCP tool list for a globally hooked harness.
    let dir = repo();
    let out = Command::new(BIN)
        .current_dir(&dir)
        .env("SIX_TEN_AGENT", "codex:1")
        .env("XDG_CONFIG_HOME", &cfg)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = out;
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    )
    .unwrap();
    let reply: Value = serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
    assert_eq!(reply["result"]["tools"].as_array().unwrap().len(), 4);
}

#[test]
fn commit_guard_installs_itself_without_setup() {
    let dir = repo();
    let hook_file = dir.join(".git/hooks/pre-commit");
    fs::write(&hook_file, "#!/bin/sh\necho existing-hook\n").unwrap();
    assert_eq!(
        hook(&dir, "alice", "claude", edit(&dir, "src/a.rs"))
            .status
            .code(),
        Some(0)
    );
    let body = fs::read_to_string(&hook_file).unwrap();
    assert!(
        body.contains("six-ten precommit || exit 1") && body.contains("echo existing-hook"),
        "{body}"
    );
    hook(&dir, "alice", "claude", edit(&dir, "src/a.rs"));
    assert_eq!(
        fs::read_to_string(&hook_file)
            .unwrap()
            .matches("six-ten precommit")
            .count(),
        1
    );

    // Hooks kept in tracked files (Husky-style core.hooksPath) are the project's: leave them alone.
    let husky = repo();
    fs::create_dir_all(husky.join(".husky")).unwrap();
    git(&husky, &["config", "core.hooksPath", ".husky"]);
    hook(&husky, "alice", "claude", edit(&husky, "src/a.rs"));
    assert!(!husky.join(".husky/pre-commit").exists());
}

fn commit(dir: &Path, agent: &str, msg: &str) -> Output {
    Command::new("git")
        .current_dir(dir)
        .env("SIX_TEN_AGENT", agent)
        .env(
            "PATH",
            format!(
                "{}:{}",
                Path::new(BIN).parent().unwrap().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            msg,
        ])
        .output()
        .unwrap()
}

#[test]
fn shared_files_wait_for_the_co_editor_and_carry_trailers() {
    let dir = repo();
    edit_as(&dir, "alice", "src/a.rs", "a\nalice\n");
    // Bob edits the same file and is still mid-turn.
    hook(
        &dir,
        "bob",
        "claude",
        tool(&dir, "PreToolUse", "Edit", "src/a.rs"),
    );
    fs::write(dir.join("src/a.rs"), "a\nalice\nbob\n").unwrap();
    hook(
        &dir,
        "bob",
        "claude",
        tool(&dir, "PostToolUse", "Edit", "src/a.rs"),
    );
    git(&dir, &["add", "src/a.rs"]);
    let refused = commit(&dir, "alice", "alice: a");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("you both edited it; bob is still working"),
        "{}",
        stderr(&refused)
    );

    stop_uncommitted(&dir, "bob");
    let ok = commit(&dir, "alice", "alice: a");
    assert!(ok.status.success(), "{}", stderr(&ok));
    let msg = String::from_utf8(
        Command::new("git")
            .current_dir(&dir)
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        msg.contains("Agent: alice") && msg.contains("Co-edited-by: bob"),
        "{msg}"
    );
}

#[test]
fn agents_never_switch_branches_or_add_worktrees() {
    let dir = repo();
    for cmd in [
        "git switch -c agents/2026-09-29-a",
        "git checkout main",
        "git worktree add -b x ../x",
    ] {
        let out = hook(&dir, "alice", "claude", bash(&dir, cmd));
        assert_eq!(
            out.status.code(),
            Some(2),
            "{cmd} should be refused, even alone"
        );
        assert!(stderr(&out).contains("six-ten session"), "{}", stderr(&out));
    }
    let tool = json!({"hook_event_name": "PreToolUse", "tool_name": "EnterWorktree", "cwd": dir, "tool_input": {}});
    let out = hook(&dir, "alice", "claude", tool);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("don't create worktrees"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn without_session_branches_switches_wait_for_agents_that_have_not_edited_yet() {
    let dir = repo();
    git(&dir, &["config", "six-ten.sessionBranches", "false"]);
    assert_eq!(
        hook(
            &dir,
            "alice",
            "claude",
            bash(&dir, "git switch -c agents/2026-09-29-a")
        )
        .status
        .code(),
        Some(0),
        "alone: allowed"
    );
    // Bob has only started his turn: no leases, no edits.
    hook(
        &dir,
        "bob",
        "claude",
        json!({"hook_event_name": "UserPromptSubmit", "cwd": dir, "prompt": "go"}),
    );
    let out = hook(
        &dir,
        "alice",
        "claude",
        bash(&dir, "git switch -c agents/2026-09-29-b"),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("bob (active"), "{}", stderr(&out));
    assert_eq!(
        hook(&dir, "alice", "claude", bash(&dir, "git status"))
            .status
            .code(),
        Some(0)
    );
    let status = String::from_utf8(six(&dir, "alice", &["status"]).stdout).unwrap();
    assert!(status.contains("other agents active: bob"), "{status}");
}

#[test]
fn agents_commit_on_one_session_branch_started_by_whoever_is_first() {
    let dir = repo();
    git(&dir, &["switch", "-q", "-c", "main"]);
    edit_as(&dir, "alice", "src/a.rs", "a\nalice\n");
    git(&dir, &["add", "src/a.rs"]);
    let refused = commit(&dir, "alice", "alice: a");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("six-ten session <topic>"),
        "{}",
        stderr(&refused)
    );

    // Both agents ask for a session at once: one branch, shared.
    let racers: Vec<_> = ["alice", "bob"]
        .iter()
        .map(|a| {
            Command::new(BIN)
                .current_dir(&dir)
                .env("SIX_TEN_AGENT", a)
                .args(["session", "Auth Flow!"])
                .output()
        })
        .collect();
    let texts: Vec<String> = racers
        .into_iter()
        .map(|o| String::from_utf8(o.unwrap().stdout).unwrap())
        .collect();
    assert_eq!(
        texts.iter().filter(|t| t.starts_with("started")).count(),
        1,
        "{texts:?}"
    );
    let branch = String::from_utf8(
        Command::new("git")
            .current_dir(&dir)
            .args(["branch", "--show-current"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        branch.trim().starts_with("agents/") && branch.trim().ends_with("-auth-flow"),
        "{branch}"
    );
    assert_eq!(
        fs::read_to_string(dir.join("src/a.rs")).unwrap(),
        "a\nalice\n",
        "work untouched"
    );
    assert!(commit(&dir, "alice", "alice: a").status.success());
    // Humans may still commit on main.
    git(&dir, &["switch", "-q", "main"]);
    fs::write(dir.join("src/b.rs"), "human\n").unwrap();
    git(&dir, &["add", "src/b.rs"]);
    assert!(commit(&dir, "someone", "human commit").status.success());
}

fn out_text(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn stop(dir: &Path, agent: &str) -> Output {
    hook(
        dir,
        agent,
        "claude",
        json!({"hook_event_name": "Stop", "cwd": dir}),
    )
}

fn edit_now(dir: &Path, agent: &str, file: &str, content: &str) {
    assert_eq!(
        hook(dir, agent, "claude", tool(dir, "PreToolUse", "Edit", file))
            .status
            .code(),
        Some(0)
    );
    fs::write(dir.join(file), content).unwrap();
    hook(dir, agent, "claude", tool(dir, "PostToolUse", "Edit", file));
}

#[test]
fn turn_end_asks_for_commits_and_the_last_agent_finishes_on_main() {
    let dir = repo();
    let origin = dir.with_extension("origin.git");
    let _ = fs::remove_dir_all(&origin);
    git(&dir, &["switch", "-q", "-c", "main"]);
    git(&dir, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&dir, &["push", "-q", "origin", "main"]);
    six(&dir, "alice", &["session", "data"]);
    edit_now(&dir, "alice", "src/a.rs", "a\nalice\n");
    edit_now(&dir, "bob", "src/b.rs", "b\nbob\n");

    let reminded = stop(&dir, "alice");
    assert_eq!(
        reminded.status.code(),
        Some(2),
        "uncommitted work sends alice back"
    );
    let msg = stderr(&reminded);
    assert!(
        msg.contains("commit your work") && msg.contains("src/a.rs"),
        "{msg}"
    );
    assert!(!msg.contains("src/b.rs"), "bob's file is bob's: {msg}");

    git(&dir, &["add", "src/a.rs"]);
    assert!(commit(&dir, "alice", "alice: a").status.success());
    assert_eq!(
        stop(&dir, "alice").status.code(),
        Some(0),
        "bob still works: alice is done"
    );

    git(&dir, &["add", "src/b.rs"]);
    assert!(commit(&dir, "bob", "bob: b").status.success());
    let last = stop(&dir, "bob");
    assert_eq!(last.status.code(), Some(2));
    assert!(
        stderr(&last).contains("you're the last agent working"),
        "{}",
        stderr(&last)
    );
    assert_eq!(
        stop(&dir, "bob").status.code(),
        Some(0),
        "the reminder comes once"
    );

    let finished = six(&dir, "bob", &["finish"]);
    assert_eq!(finished.status.code(), Some(0), "{}", stderr(&finished));
    let text = String::from_utf8_lossy(&finished.stdout);
    assert!(
        text.contains("merged `agents/") && text.contains("pushed `main`"),
        "{text}"
    );
    assert_eq!(out_text(&dir, &["branch", "--show-current"]), "main");
    assert_eq!(
        out_text(&dir, &["rev-parse", "main"]),
        out_text(&origin, &["rev-parse", "main"])
    );
    assert!(out_text(&dir, &["log", "-1", "--format=%s"]).starts_with("Merge agents/"));
    assert_eq!(
        fs::read_to_string(dir.join("src/b.rs")).unwrap(),
        "b\nbob\n"
    );

    // The day's next session continues the first one's name.
    let next =
        String::from_utf8(six(&dir, "carol", &["session", "something else"]).stdout).unwrap();
    assert!(next.contains("-data.2"), "{next}");
}

#[test]
fn finish_waits_for_working_agents_and_leftovers_are_anyones_to_commit() {
    let dir = repo();
    git(&dir, &["switch", "-q", "-c", "main"]);
    six(&dir, "bob", &["session", "x"]);
    edit_as(&dir, "alice", "src/a.rs", "a\nalice\n");

    let refused = six(&dir, "bob", &["finish"]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        stderr(&refused).contains("src/a.rs (left by alice"),
        "{}",
        stderr(&refused)
    );

    // Alice's turn is over, so bob may commit her work; she is credited.
    git(&dir, &["add", "src/a.rs"]);
    assert!(commit(&dir, "bob", "alice's leftover").status.success());
    assert!(out_text(&dir, &["log", "-1", "--format=%B"]).contains("Co-edited-by: alice"));

    hook(
        &dir,
        "carol",
        "claude",
        json!({"hook_event_name": "UserPromptSubmit", "cwd": dir, "prompt": "go"}),
    );
    let waiting = six(&dir, "bob", &["finish"]);
    assert_eq!(waiting.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&waiting.stdout).contains("carol still working"));
    assert!(out_text(&dir, &["branch", "--show-current"]).starts_with("agents/"));
}

#[test]
fn a_worktree_commit_ignores_main_checkout_work_and_is_warned_once() {
    let dir = repo();
    let wt = dir.with_extension("wt");
    let _ = fs::remove_dir_all(&wt);
    git(
        &dir,
        &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
    );
    edit_now(&dir, "alice", "src/b.rs", "b\nalice\n");
    fs::write(wt.join("src/b.rs"), "b\nbob\n").unwrap();

    let read = tool(&wt, "PostToolUse", "Read", "src/a.rs");
    let note = context(&hook(&wt, "bob", "claude", read.clone()));
    assert!(note.contains("separate worktree"), "{note}");
    assert!(!context(&hook(&wt, "bob", "claude", read)).contains("separate worktree"));

    git(&wt, &["add", "src/b.rs"]);
    let ok = commit(&wt, "bob", "bob in worktree");
    assert!(ok.status.success(), "{}", stderr(&ok));
}
