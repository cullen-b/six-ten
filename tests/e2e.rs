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
    git(&dir, &["init", "-q", "-b", "main"]);
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
        "git switch -c other",
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
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 4);
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
    for _ in 0..2 {
        assert!(six(&dir, "x", &["install", "all"]).status.success());
    }
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(dir.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
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
