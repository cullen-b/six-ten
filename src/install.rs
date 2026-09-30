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

Several agents may share this checkout; six-ten keeps you from colliding.

- Just edit. Edits are claimed automatically; claim files first only if `six_ten_claim` is in your
  tool list.
- If an edit is refused because another agent holds the file, work on other files and retry later.
  Call `six_ten_wait` only when nothing else is left. Never get around a refusal with the shell.
- Notes from six-ten are diffs of other agents' changes: account for them; re-read only when told to.
- Branches: all agents share the checked-out branch. Never create or switch branches while other
  agents are active. If you're on `main` and `six-ten status` shows no other agents, start a session
  branch first: `git switch -c agents/<YYYY-MM-DD>-<topic>`.
- Commit your own finished files as you go: `git add <paths>` and commit, never `git add -A` or
  `commit -a`. If a shared file is refused, commit the rest now and that file after the other
  agent's turn ends. `Agent:` / `Co-edited-by:` trailers are added for you.
- Tests may fail in files other agents are editing (see `six-ten status`); those aren't yours to fix.
- Never `stash`, `reset --hard`, `checkout`/`switch`, `restore`, `clean` or `pull` while other
  agents are active. To undo your own work, restore just your files by path.
- The session branch goes into `main` once, at the end, through a PR (unless the user says otherwise).";

/// Where a harness's configuration goes: one repository, or the user's global config.
enum Scope {
    Repo(PathBuf),
    Global(PathBuf),
}

impl Scope {
    /// Directory the harness keeps its config in (`.claude`, `.codex`, `.opencode`, ...).
    fn dir(&self, repo: &str, global: &str) -> PathBuf {
        match self {
            Scope::Repo(root) => root.join(repo),
            Scope::Global(home) => home.join(global),
        }
    }
}

fn home() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `$XDG_CONFIG_HOME` or `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| Path::new(&h).join(".config"))
        })
}

pub fn run(harness: &str, repo: &Path, global: bool) -> Result<()> {
    let scope = if global {
        Scope::Global(home()?)
    } else {
        let root = git_out(repo, &["rev-parse", "--show-toplevel"])
            .context("--repo must be inside a git repository (or pass --global)")?;
        Scope::Repo(PathBuf::from(root.trim()))
    };
    let all = ["claude", "codex", "opencode", "hermes", "git"];
    let names: Vec<&str> = if harness == "all" {
        all.to_vec()
    } else {
        vec![harness]
    };
    for name in &names {
        match *name {
            "claude" => claude(&scope)?,
            "codex" => codex(&scope)?,
            "opencode" => opencode(&scope)?,
            "hermes" => hermes(&scope)?,
            "git" if global => println!(
                "git: automatic; each repo gets the pre-commit check the first time an agent works in it"
            ),
            "generic" if global => println!(
                "generic: per-repository only; run `{BIN} install generic` inside the repo"
            ),
            "git" => git_hook(repo_root(&scope))?,
            "generic" => {
                protocol_block(&repo_root(&scope).join("AGENTS.md"))?;
                git_hook(repo_root(&scope))?;
                println!(
                    "generic: protocol in AGENTS.md. Register `{BIN} mcp` as a stdio MCP server in your harness.\n  \
                     For enforcement, forward its tool hooks to `{BIN} hook generic` (README: \"Adding a harness\")."
                );
            }
            other => bail!(
                "unknown harness `{other}` (expected claude, codex, opencode, hermes, generic, git or all)"
            ),
        }
    }
    // The MCP server hides six_ten_claim from harnesses whose hooks already claim edits.
    for name in names.iter().filter(|n| !matches!(**n, "git" | "generic")) {
        match &scope {
            Scope::Repo(root) => crate::store::Store::open(root)?.mark_hooked(name)?,
            Scope::Global(_) => crate::store::mark_hooked_globally(name)?,
        }
    }
    if Command::new(BIN).arg("--version").output().is_err() {
        eprintln!(
            "warning: `{BIN}` is not on PATH; harness hooks will not find it. Run `cargo install --path .` in the six-ten repo."
        );
    }
    Ok(())
}

fn repo_root(scope: &Scope) -> &Path {
    match scope {
        Scope::Repo(root) | Scope::Global(root) => root,
    }
}

fn claude(scope: &Scope) -> Result<()> {
    let settings = scope.dir(".claude", ".claude").join("settings.json");
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
    match scope {
        Scope::Repo(root) => {
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
        }
        Scope::Global(home) => {
            write_json(&settings, &doc)?;
            protocol_block(&home.join(".claude/CLAUDE.md"))?;
            // User-scope MCP servers live in ~/.claude.json, which Claude Code owns: use its CLI.
            let registered = Command::new("claude")
                .args(["mcp", "get", BIN])
                .output()
                .is_ok_and(|o| o.status.success())
                || Command::new("claude")
                    .args(["mcp", "add", "--scope", "user", BIN, "--", BIN, "mcp"])
                    .output()
                    .is_ok_and(|o| o.status.success());
            println!(
                "claude: hooks and permissions in ~/.claude/settings.json, protocol in ~/.claude/CLAUDE.md"
            );
            if registered {
                println!("  MCP server registered for all projects (claude mcp, user scope)");
            } else {
                println!("  finish with: claude mcp add --scope user {BIN} -- {BIN} mcp");
            }
        }
    }
    Ok(())
}

fn codex(scope: &Scope) -> Result<()> {
    let dir = match scope {
        Scope::Global(home) => std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".codex")),
        Scope::Repo(root) => root.join(".codex"),
    };
    let hooks_file = dir.join("hooks.json");
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

    let config = dir.join("config.toml");
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
    let (agents, shown) = match scope {
        Scope::Repo(root) => (root.join("AGENTS.md"), ".codex/".to_string()),
        Scope::Global(_) => (dir.join("AGENTS.md"), format!("{}/", dir.display())),
    };
    protocol_block(&agents)?;
    println!(
        "codex: hooks in {shown}hooks.json, MCP server and sandbox profile in {shown}config.toml, protocol in {}\n  \
         Codex asks you to approve new or changed hooks: run /hooks once{}.\n  \
         Sandbox profile `six-ten` lets agents commit (.git writable; .git/hooks and .git/config stay read-only).",
        agents.display(),
        if matches!(scope, Scope::Repo(_)) {
            " (and trust the project)"
        } else {
            ""
        }
    );
    Ok(())
}

fn opencode(scope: &Scope) -> Result<()> {
    let (dir, config_dir) = match scope {
        Scope::Repo(root) => (root.join(".opencode"), root.clone()),
        Scope::Global(_) => {
            let d = config_home()
                .context("cannot locate ~/.config")?
                .join("opencode");
            (d.clone(), d)
        }
    };
    let plugin = dir.join("plugins/six-ten.ts");
    fs::create_dir_all(plugin.parent().context("plugin path has no parent")?)?;
    fs::write(&plugin, include_str!("../integrations/opencode/six-ten.ts"))?;
    let mcp = json!({"type": "local", "command": [BIN, "mcp"], "enabled": true});
    let jsonc = config_dir.join("opencode.jsonc");
    let config = if jsonc.exists() {
        jsonc_add_mcp(&jsonc, &mcp)?;
        jsonc
    } else {
        let config = config_dir.join("opencode.json");
        let mut doc = read_json(&config)?;
        let obj = doc
            .as_object_mut()
            .context("opencode.json is not an object")?;
        obj.entry("$schema")
            .or_insert(json!("https://opencode.ai/config.json"));
        obj.entry("mcp").or_insert(json!({}))[BIN] = mcp;
        write_json(&config, &doc)?;
        config
    };
    let agents = config_dir.join("AGENTS.md");
    protocol_block(&agents)?;
    println!(
        "opencode: plugin in {}, MCP server in {}, protocol in {}",
        plugin.display(),
        config.display(),
        agents.display()
    );
    Ok(())
}

/// Adds the MCP server to an `opencode.jsonc` by text insertion, keeping the user's comments.
fn jsonc_add_mcp(path: &Path, mcp: &Value) -> Result<()> {
    let text = fs::read_to_string(path)?;
    if text.contains(&format!("\"{BIN}\"")) {
        return Ok(());
    }
    let entry = format!("\"{BIN}\": {mcp}");
    let (at, insert) = match text
        .find("\"mcp\"")
        .and_then(|k| text[k..].find('{').map(|b| k + b + 1))
    {
        Some(at) => {
            let rest = text[at..].trim_start();
            let comma = if rest.starts_with('}') { "" } else { "," };
            (at, format!("\n    {entry}{comma}"))
        }
        None => {
            let at = text
                .find('{')
                .context("opencode.jsonc has no top-level object")?
                + 1;
            let rest = text[at..].trim_start();
            let comma = if rest.starts_with('}') { "" } else { "," };
            (at, format!("\n  \"mcp\": {{\n    {entry}\n  }}{comma}"))
        }
    };
    fs::copy(path, path.with_extension("jsonc.six-ten-backup"))?;
    fs::write(path, format!("{}{insert}{}", &text[..at], &text[at..]))?;
    Ok(())
}

/// Hermes config is global: install the plugin into its home and let Hermes's own CLI enable it
/// and register the MCP server, rather than editing config.yaml ourselves.
fn hermes(scope: &Scope) -> Result<()> {
    if let Scope::Repo(root) = scope {
        protocol_block(&root.join("AGENTS.md"))?;
    }
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
    if let Scope::Global(_) = scope {
        // Hermes's own global instructions, if the user keeps them.
        let agents = home.join("AGENTS.md");
        if agents.exists() {
            protocol_block(&agents)?;
        }
    }
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
    println!(
        "hermes: plugin in {} (Hermes plugins are always global)",
        dir.display()
    );

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

/// Git hooks six-ten adds a line to: (hook name, line). The trailer hook never blocks a commit.
const GIT_HOOKS: [(&str, &str); 2] = [
    ("pre-commit", "six-ten precommit || exit 1"),
    ("commit-msg", "six-ten commit-msg \"$1\" || true"),
];

fn git_hook(root: &Path) -> Result<()> {
    for (name, line) in GIT_HOOKS {
        let hook = write_git_hook(&hook_path(root, name)?, line)?;
        println!("git: {name} hook in {}", hook.display());
    }
    Ok(())
}

fn hook_path(root: &Path, name: &str) -> Result<PathBuf> {
    let hook = git_out(root, &["rev-parse", "--git-path", &format!("hooks/{name}")])?;
    Ok(root.join(hook.trim()))
}

/// Runs on every harness hook: gives the repo the commit guard and trailers without any setup.
/// Hooks kept in the working tree (`core.hooksPath`, e.g. Husky) are the project's own files, so
/// those are left alone.
pub fn ensure_git_hook(store: &crate::store::Store) -> Result<()> {
    let flag = store.dir.join("git-hooks");
    if let Ok(recorded) = fs::read_to_string(&flag) {
        let intact = recorded
            .lines()
            .zip(GIT_HOOKS)
            .all(|(path, (_, line))| fs::read_to_string(path).is_ok_and(|b| b.contains(line)));
        if recorded.trim() == "skip" || (recorded.lines().count() == GIT_HOOKS.len() && intact) {
            return Ok(());
        }
    }
    let root = store.root();
    let git_dir = PathBuf::from(
        git_out(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?
        .trim(),
    );
    let mut written = Vec::new();
    for (name, line) in GIT_HOOKS {
        let hook = hook_path(root, name)?;
        let hook_dir = hook.parent().context("hook path has no parent")?;
        fs::create_dir_all(hook_dir).ok();
        let inside_git = hook_dir
            .canonicalize()
            .is_ok_and(|d| git_dir.canonicalize().is_ok_and(|g| d.starts_with(g)));
        if !inside_git {
            return fs::write(&flag, "skip").map_err(Into::into);
        }
        written.push(write_git_hook(&hook, line)?.to_string_lossy().into_owned());
    }
    fs::write(&flag, written.join("\n"))?;
    Ok(())
}

fn write_git_hook(hook: &Path, line: &str) -> Result<PathBuf> {
    let hook = hook.to_path_buf();
    let existing = fs::read_to_string(&hook).unwrap_or_default();
    let body = if existing.contains(line) {
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
    Ok(hook)
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
