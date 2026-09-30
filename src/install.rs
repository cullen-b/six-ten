use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const BIN: &str = "six-ten";
const BEGIN: &str = "<!-- six-ten:begin -->";
const END: &str = "<!-- six-ten:end -->";

const GIT_RULES: &str = "[permissions.six-ten.filesystem.\":workspace_roots\"]\n\".git/**\" = \"write\"\n\".git/config\" = \"read\"\n\".git/hooks/**\" = \"read\"\n";

const PROTOCOL: &str = "## Working alongside other agents (six-ten)

Several agents may be editing this checkout at the same time; six-ten keeps you from colliding.

- Just edit. With six-ten hooks installed your edits are claimed automatically: don't call
  `six_ten_claim` or `six_ten_status` before ordinary edits. (Only if `six_ten_claim` is in your
  tool list does your harness lack hooks; then claim files right before editing them.)
- If an edit is refused because another agent holds the file, work on other files and retry later.
  Call `six_ten_wait` only when nothing else is left. Never get around a refusal with the shell.
- Notes from six-ten are diffs of other agents' changes: account for them; re-read only when told to.
- Never run `git stash`, `reset --hard`, `checkout`/`switch`, `restore`, `clean` or `pull` while
  other agents are active. Commit only your files: `git add <paths>`, never `git add -A`/`commit -a`.";

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
        "generic" => {
            protocol_block(&root.join("AGENTS.md"))?;
            git_hook(root)?;
            println!(
                "generic: protocol in AGENTS.md. Register `{BIN} mcp` as a stdio MCP server in your harness.\n  \
                 For enforcement, forward its tool hooks to `{BIN} hook generic` (README: \"Adding a harness\")."
            );
        }
        "all" => {
            claude(root)?;
            codex(root)?;
            opencode(root)?;
            hermes(root)?;
            git_hook(root)?;
        }
        other => bail!(
            "unknown harness `{other}` (expected claude, codex, opencode, hermes, generic, git or all)"
        ),
    }
    // The MCP server hides six_ten_claim from harnesses whose hooks already claim edits.
    let hooked: Vec<&str> = match harness {
        "all" => vec!["claude", "codex", "opencode", "hermes"],
        "git" | "generic" => vec![],
        one => vec![one],
    };
    let store = crate::store::Store::open(root)?;
    for h in hooked {
        store.mark_hooked(h)?;
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
    let mut text = fs::read_to_string(&config).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.contains("[mcp_servers.six-ten]") {
        text.push_str(&format!(
            "\n[mcp_servers.six-ten]\ncommand = \"{BIN}\"\nargs = [\"mcp\"]\n"
        ));
    }
    // Codex's sandbox keeps .git read-only, so agents can't commit. This profile opens .git but
    // keeps hooks and config read-only: writing either would run code outside the sandbox.
    if !text.contains("[permissions.six-ten]") {
        text.push_str(&format!(
            "\n[permissions.six-ten]\nextends = \":workspace\"\n\n{GIT_RULES}"
        ));
    }
    let own_profile = text
        .lines()
        .any(|l| l.trim_start().starts_with("default_permissions") && !l.contains("\"six-ten\""));
    if own_profile {
        println!(
            "codex: you already set default_permissions; to let agents commit, add to that profile:\n{}",
            GIT_RULES.replacen("six-ten", "<your profile>", 1)
        );
    } else if !text.contains("default_permissions = \"six-ten\"") {
        // Top-level keys must come before the first table header.
        text = format!("default_permissions = \"six-ten\"\n{text}");
    }
    fs::create_dir_all(config.parent().context("config path has no parent")?)?;
    fs::write(&config, text)?;
    protocol_block(&root.join("AGENTS.md"))?;
    println!(
        "codex: hooks in .codex/hooks.json, MCP server in .codex/config.toml, protocol in AGENTS.md\n  \
         Codex loads project config only for trusted projects, and asks you to approve new hooks: run /hooks once.\n  \
         Sandbox profile `six-ten` lets agents commit (.git writable; .git/hooks and .git/config stay read-only)."
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

/// Hermes config is global: install the plugin into its home and let Hermes's own CLI enable it
/// and register the MCP server, rather than editing config.yaml ourselves.
fn hermes(root: &Path) -> Result<()> {
    protocol_block(&root.join("AGENTS.md"))?;
    let home = std::env::var("HERMES_HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| Path::new(&h).join(".hermes"))
        })
        .context("cannot locate the Hermes home (set HERMES_HOME)")?;
    let dir = home.join("plugins/six-ten");
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("plugin.yaml"),
        include_str!("../integrations/hermes/six-ten/plugin.yaml"),
    )?;
    fs::write(
        dir.join("__init__.py"),
        include_str!("../integrations/hermes/six-ten/__init__.py"),
    )?;
    println!("hermes: plugin in {}, protocol in AGENTS.md", dir.display());

    // Answers `mcp add`'s "Enable all tools? [Y/n]" prompt; it exits 0 even when cancelled.
    let hermes = |args: &[&str]| {
        let mut child = Command::new("hermes")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(b"y\n");
        }
        child.wait_with_output()
    };
    let listed =
        || hermes(&["mcp", "list"]).is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(BIN));
    let enabled = hermes(&["plugins", "enable", "six-ten"]).is_ok_and(|o| o.status.success());
    let mcp = listed()
        || (hermes(&["mcp", "add", BIN, "--command", BIN, "--args", "mcp"]).is_ok() && listed());
    if enabled && mcp {
        println!("  enabled the plugin and registered the MCP server with the hermes CLI");
    } else {
        println!(
            "  finish with:{}{}",
            if enabled {
                ""
            } else {
                "\n    hermes plugins enable six-ten"
            },
            if mcp {
                ""
            } else {
                "\n    hermes mcp add six-ten --command six-ten --args mcp"
            }
        );
    }
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
