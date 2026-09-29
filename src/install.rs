use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const BIN: &str = "six-ten";
const BEGIN: &str = "<!-- six-ten:begin -->";
const END: &str = "<!-- six-ten:end -->";

const PROTOCOL: &str = "## Working alongside other agents (six-ten)

Several agents may be editing this checkout at the same time. six-ten keeps you from colliding:

- Edits are claimed automatically. If an edit is blocked because another agent holds the file, work on
  other files first and retry later, or call `six_ten_wait` (CLI: `six-ten wait <path>`) to block
  until it is free. Never work around a block with shell redirection, `sed -i`, or similar.
- If you edit files through the shell, claim them first: `six-ten claim <paths>` (MCP: `six_ten_claim`).
- Never run `git stash`, `git reset --hard`, `git checkout`/`git switch`, `git restore`, `git clean` or
  `git pull` while other agents are active; they rewrite everyone's files.
- Commit only files you changed: `git add <your files>`, never `git add -A`/`git commit -a`.
- `six-ten status` (MCP: `six_ten_status`) shows who is editing what.";

pub fn run(harness: &str, repo: &Path) -> Result<()> {
    let root = git_out(repo, &["rev-parse", "--show-toplevel"])
        .context("--repo must be inside a git repository")?;
    let root = Path::new(root.trim());
    match harness {
        "claude" => claude(root)?,
        "codex" => codex(root)?,
        "opencode" => opencode(root)?,
        "hermes" => hermes(root)?,
        "git" => git_hook(root)?,
        "all" => {
            claude(root)?;
            codex(root)?;
            opencode(root)?;
            hermes(root)?;
            git_hook(root)?;
        }
        other => bail!(
            "unknown harness `{other}` (expected claude, codex, opencode, hermes, git or all)"
        ),
    }
    if Command::new(BIN).arg("--version").output().is_err() {
        eprintln!(
            "warning: `{BIN}` is not on PATH; harness hooks will not find it. Run `cargo install --path .` in the six-ten repo."
        );
    }
    Ok(())
}

fn claude(root: &Path) -> Result<()> {
    let settings = root.join(".claude/settings.json");
    let mut doc = read_json(&settings)?;
    let hooks = doc
        .as_object_mut()
        .context("settings.json is not an object")?
        .entry("hooks")
        .or_insert(json!({}));
    let hook = json!([{"type": "command", "command": format!("{BIN} hook claude")}]);
    let wanted = [
        (
            "PreToolUse",
            json!({"matcher": "^(Edit|Write|MultiEdit|NotebookEdit|Bash)$", "hooks": hook}),
        ),
        (
            "PostToolUse",
            json!({"matcher": "^(Read|Edit|Write|MultiEdit|NotebookEdit|Bash)$", "hooks": hook}),
        ),
        ("UserPromptSubmit", json!({"hooks": hook})),
        ("Stop", json!({"hooks": hook})),
        ("SubagentStop", json!({"hooks": hook})),
        ("SessionEnd", json!({"hooks": hook})),
    ];
    merge_hooks(hooks, wanted)?;
    // Agents must be able to call six-ten's own tools without a permission prompt to wait politely.
    let obj = doc
        .as_object_mut()
        .context("settings.json is not an object")?;
    add_unique(
        obj.entry("permissions").or_insert(json!({})),
        "allow",
        &["mcp__six-ten", "Bash(six-ten:*)"],
    )?;
    add_unique(&mut doc, "enabledMcpjsonServers", &[BIN])?;
    write_json(&settings, &doc)?;

    let mcp = root.join(".mcp.json");
    let mut doc = read_json(&mcp)?;
    let servers = doc
        .as_object_mut()
        .context(".mcp.json is not an object")?
        .entry("mcpServers")
        .or_insert(json!({}));
    servers[BIN] = json!({"command": BIN, "args": ["mcp"]});
    write_json(&mcp, &doc)?;

    protocol_block(&root.join("CLAUDE.md"))?;
    println!(
        "claude: hooks in .claude/settings.json, MCP server in .mcp.json, protocol in CLAUDE.md"
    );
    Ok(())
}

fn codex(root: &Path) -> Result<()> {
    let hooks_file = root.join(".codex/hooks.json");
    let mut doc = read_json(&hooks_file)?;
    let hooks = doc
        .as_object_mut()
        .context("hooks.json is not an object")?
        .entry("hooks")
        .or_insert(json!({}));
    let hook = |timeout: u64| json!([{"type": "command", "command": format!("{BIN} hook codex"), "timeout": timeout}]);
    let wanted = [
        (
            "PreToolUse",
            json!({"matcher": "apply_patch|Bash", "hooks": hook(30)}),
        ),
        (
            "PostToolUse",
            json!({"matcher": "apply_patch|Bash", "hooks": hook(30)}),
        ),
        ("UserPromptSubmit", json!({"hooks": hook(30)})),
        ("Stop", json!({"hooks": hook(30)})),
        ("SubagentStop", json!({"hooks": hook(30)})),
        ("SessionEnd", json!({"hooks": hook(3)})),
    ];
    merge_hooks(hooks, wanted)?;
    write_json(&hooks_file, &doc)?;

    let config = root.join(".codex/config.toml");
    let existing = fs::read_to_string(&config).unwrap_or_default();
    if !existing.contains("[mcp_servers.six-ten]") {
        let sep = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        fs::write(
            &config,
            format!(
                "{existing}{sep}\n[mcp_servers.six-ten]\ncommand = \"{BIN}\"\nargs = [\"mcp\"]\n"
            ),
        )?;
    }
    protocol_block(&root.join("AGENTS.md"))?;
    println!(
        "codex: hooks in .codex/hooks.json, MCP server in .codex/config.toml, protocol in AGENTS.md\n  \
         Codex loads project config only for trusted projects, and asks you to approve new hooks: run /hooks once."
    );
    Ok(())
}

fn opencode(root: &Path) -> Result<()> {
    let plugin = root.join(".opencode/plugins/six-ten.ts");
    fs::create_dir_all(plugin.parent().context("plugin path has no parent")?)?;
    fs::write(&plugin, include_str!("../integrations/opencode/six-ten.ts"))?;
    let config = root.join("opencode.json");
    let mcp = json!({"type": "local", "command": [BIN, "mcp"], "enabled": true});
    if root.join("opencode.jsonc").exists() {
        println!("opencode: add to opencode.jsonc under \"mcp\": \"six-ten\": {mcp}");
    } else {
        let mut doc = read_json(&config)?;
        let obj = doc
            .as_object_mut()
            .context("opencode.json is not an object")?;
        obj.entry("$schema")
            .or_insert(json!("https://opencode.ai/config.json"));
        obj.entry("mcp").or_insert(json!({}))[BIN] = mcp;
        write_json(&config, &doc)?;
    }
    protocol_block(&root.join("AGENTS.md"))?;
    println!(
        "opencode: plugin in .opencode/plugins/six-ten.ts, MCP server in opencode.json, protocol in AGENTS.md"
    );
    Ok(())
}

/// Hermes only reads global config (~/.hermes/config.yaml), so print the snippet instead of editing it.
fn hermes(root: &Path) -> Result<()> {
    protocol_block(&root.join("AGENTS.md"))?;
    println!(
        "hermes: protocol in AGENTS.md. Hermes config is global; add this to ~/.hermes/config.yaml:\n\n\
         hooks:\n  pre_tool_call:\n    - matcher: \"write_file|patch|terminal\"\n      command: \"{BIN} hook hermes\"\n      timeout: 30\n  \
         post_tool_call:\n    - matcher: \"read_file|write_file|patch|terminal\"\n      command: \"{BIN} hook hermes\"\n  \
         pre_llm_call:\n    - command: \"{BIN} hook hermes\"\n  \
         on_session_end:\n    - command: \"{BIN} hook hermes\"\n  on_session_finalize:\n    - command: \"{BIN} hook hermes\"\n\
         mcp_servers:\n  six-ten:\n    command: \"{BIN}\"\n    args: [\"mcp\"]\n"
    );
    Ok(())
}

fn add_unique(obj: &mut Value, key: &str, items: &[&str]) -> Result<()> {
    let list = obj
        .as_object_mut()
        .context("expected a JSON object")?
        .entry(key)
        .or_insert(json!([]));
    let arr = list
        .as_array_mut()
        .with_context(|| format!("`{key}` is not an array"))?;
    for item in items {
        if !arr.iter().any(|v| v == item) {
            arr.push(json!(item));
        }
    }
    Ok(())
}

/// Replaces any existing six-ten hook groups with `wanted`, leaving the user's own hooks alone.
fn merge_hooks<const N: usize>(hooks: &mut Value, wanted: [(&str, Value); N]) -> Result<()> {
    for (event, group) in wanted {
        let list = hooks
            .as_object_mut()
            .context("hooks is not an object")?
            .entry(event)
            .or_insert(json!([]));
        let arr = list.as_array_mut().context("hook list is not an array")?;
        arr.retain(|g| !g.to_string().contains(&format!("{BIN} hook")));
        arr.push(group);
    }
    Ok(())
}

fn git_hook(root: &Path) -> Result<()> {
    let hook = git_out(root, &["rev-parse", "--git-path", "hooks/pre-commit"])?;
    let hook = root.join(hook.trim());
    let line = format!("{BIN} precommit || exit 1");
    let existing = fs::read_to_string(&hook).unwrap_or_default();
    let body = if existing.contains(&line) {
        existing
    } else if existing.is_empty() {
        format!("#!/bin/sh\n{line}\n")
    } else {
        match existing.split_once('\n') {
            Some((shebang, rest)) if shebang.starts_with("#!") => {
                format!("{shebang}\n{line}\n{rest}")
            }
            _ => format!("{line}\n{existing}"),
        }
    };
    fs::create_dir_all(hook.parent().context("hook path has no parent")?)?;
    fs::write(&hook, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    }
    println!("git: pre-commit check in {}", hook.display());
    Ok(())
}

/// Inserts or replaces the protocol section between six-ten markers.
fn protocol_block(file: &Path) -> Result<()> {
    let existing = fs::read_to_string(file).unwrap_or_default();
    let block = format!("{BEGIN}\n{PROTOCOL}\n{END}");
    let updated = match (existing.find(BEGIN), existing.find(END)) {
        (Some(b), Some(e)) if b < e => {
            format!("{}{block}{}", &existing[..b], &existing[e + END.len()..])
        }
        _ if existing.trim().is_empty() => format!("{block}\n"),
        _ => format!("{}\n\n{block}\n", existing.trim_end()),
    };
    fs::write(file, updated)?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))
        }
        _ => Ok(json!({})),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn git_out(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}
