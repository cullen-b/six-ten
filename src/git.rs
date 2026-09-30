use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Result, bail};

/// Paths with uncommitted changes (staged, unstaged or untracked), optionally limited to `only`.
pub fn dirty(root: &Path, only: &[String]) -> Result<HashSet<String>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "-z", "--untracked-files=all", "--"]);
    cmd.args(only);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut set = HashSet::new();
    let mut entries = text.split('\0').filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (status, path) = entry.split_at(3);
        set.insert(path.to_string());
        if status.contains('R') || status.contains('C') {
            entries.next().map(|orig| set.insert(orig.to_string()));
        }
    }
    Ok(set)
}

const MAX_BLOB: u64 = 1 << 20;

/// Stores `rel`'s current content in the object DB and returns its blob id; None if it doesn't exist.
/// Files over 1MB get a size/mtime stand-in so they are tracked for changes but never diffed.
pub fn blob(root: &Path, rel: &str) -> Option<String> {
    let meta = std::fs::metadata(root.join(rel))
        .ok()
        .filter(|m| m.is_file())?;
    if meta.len() > MAX_BLOB {
        let mtime = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        return Some(format!("big:{}:{mtime}", meta.len()));
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "--"])
        .arg(rel)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Unified diff from `old` to `new` (None = absent), or a one-line summary when over `max_lines`.
pub fn diff_blobs(root: &Path, old: Option<&str>, new: Option<&str>, max_lines: usize) -> String {
    let (old, new) = match (old, new) {
        (_, None) => return "the file was deleted.".into(),
        (o, Some(n)) if o.is_some_and(|o| o.starts_with("big:")) || n.starts_with("big:") => {
            return "large file changed; re-read the parts you rely on.".into();
        }
        (o, Some(n)) => (
            o.map(String::from).unwrap_or_else(|| empty_blob(root)),
            n.to_string(),
        ),
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--no-color", "--no-ext-diff", "-U2", &old, &new])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return "changed (diff unavailable); re-read it.".into(),
    };
    let body: Vec<&str> = text.lines().skip_while(|l| !l.starts_with("@@")).collect();
    let added = body.iter().filter(|l| l.starts_with('+')).count();
    let removed = body.iter().filter(|l| l.starts_with('-')).count();
    if body.len() > max_lines {
        return format!("changed substantially (+{added}/-{removed} lines); re-read it.");
    }
    body.join("\n")
}

fn empty_blob(root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "-t", "blob", "--stdin"])
        .stdin(std::process::Stdio::null())
        .output();
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into())
}

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn current_branch(root: &Path) -> Option<String> {
    git_text(root, &["symbolic-ref", "--short", "-q", "HEAD"])
}

/// `main`, `master`, or whatever `origin/HEAD` points at.
pub fn is_default_branch(root: &Path, branch: &str) -> bool {
    let remote_default = git_text(
        root,
        &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
    );
    matches!(branch, "main" | "master")
        || remote_default.is_some_and(|r| r.strip_prefix("origin/") == Some(branch))
}

/// Session branches are on unless the repo sets `git config six-ten.sessionBranches false`.
pub fn session_branches_enabled(root: &Path) -> bool {
    git_text(root, &["config", "--type=bool", "six-ten.sessionBranches"])
        .is_none_or(|v| v != "false")
}

pub fn branch_exists(root: &Path, branch: &str) -> bool {
    git_text(
        root,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_some()
}

/// Creates `branch` at HEAD and checks it out; the working tree and index are untouched.
pub fn create_branch(root: &Path, branch: &str) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["switch", "-q", "-c", branch])
        .output()?;
    if !out.status.success() {
        bail!(
            "git switch -c {branch} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Paths staged for the next commit.
pub fn staged(root: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--name-only", "-z"])
        .output()?;
    if !out.status.success() {
        bail!("git diff failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect())
}

/// What a shell command would clobber in a shared checkout.
#[derive(Debug, PartialEq)]
pub enum Clobber {
    /// Rewrites the whole working tree.
    Tree(String),
    /// Creates or switches branches: every agent in the checkout would start committing elsewhere.
    Branch(String),
    /// Overwrites or deletes specific paths (relative to the command's cwd).
    Paths(String, Vec<String>),
}

/// `command` with heredoc bodies dropped and quoted strings blanked (quotes kept as `""`), so
/// parsers only see the shell structure. Unquoted-token paths still survive.
pub fn skeleton(command: &str) -> String {
    let mut out = String::new();
    let mut heredoc: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &heredoc {
            if line.trim() == end {
                heredoc = None;
            }
            continue;
        }
        let mut quote: Option<char> = None;
        for c in line.chars() {
            match quote {
                Some(q) if c == q => {
                    quote = None;
                    out.push(c);
                }
                Some(_) => {}
                None if c == '\'' || c == '"' => {
                    quote = Some(c);
                    out.push(c);
                }
                None => out.push(c),
            }
        }
        if let Some(i) = line.find("<<") {
            let tag = line[i + 2..].trim_start_matches('-').trim_start();
            let tag: String = tag
                .chars()
                .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | ')'))
                .collect();
            if !tag.is_empty() {
                heredoc = Some(tag);
            }
        }
        out.push('\n');
    }
    out
}

/// Finds git invocations in `command` that discard or rewrite working-tree files.
pub fn clobbers(command: &str) -> Vec<Clobber> {
    let mut found = Vec::new();
    let separated = skeleton(command).replace("&&", ";").replace("||", ";");
    for segment in separated.split([';', '|', '&', '\n', '(', ')', '{', '}']) {
        let tokens: Vec<String> = segment
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '"' || c == '\'').to_string())
            .collect();
        let Some(git) = tokens
            .iter()
            .position(|t| t == "git" || t.ends_with("/git"))
        else {
            continue;
        };
        let mut rest = tokens[git + 1..].iter();
        let mut sub = None;
        while let Some(t) = rest.next() {
            match t.as_str() {
                "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                    rest.next();
                }
                t if t.starts_with('-') => {}
                t => {
                    sub = Some(t.to_string());
                    break;
                }
            }
        }
        let Some(sub) = sub else { continue };
        let args: Vec<&str> = rest.map(String::as_str).collect();
        let label = format!("git {sub}");
        let positional: Vec<String> = args
            .iter()
            .filter(|a| !a.starts_with('-'))
            .map(|a| a.to_string())
            .collect();
        let after_dashdash: Option<Vec<String>> = args
            .iter()
            .position(|a| *a == "--")
            .map(|i| args[i + 1..].iter().map(|a| a.to_string()).collect());
        let paths_or_tree = |paths: Vec<String>| {
            if paths.is_empty() || paths.iter().any(|p| p == "." || p == ":/" || p == "*") {
                Clobber::Tree(label.clone())
            } else {
                Clobber::Paths(label.clone(), paths)
            }
        };
        match sub.as_str() {
            "stash" => {
                let action = positional.first().map(String::as_str);
                if matches!(action, None | Some("push" | "save")) {
                    found.push(Clobber::Tree(label));
                }
            }
            "reset"
                if args
                    .iter()
                    .any(|a| matches!(*a, "--hard" | "--merge" | "--keep")) =>
            {
                found.push(Clobber::Tree(label))
            }
            "checkout" if !args.is_empty() => match after_dashdash {
                Some(paths) => found.push(paths_or_tree(paths)),
                None if positional.iter().any(|p| p == ".") => found.push(Clobber::Tree(label)),
                None => found.push(Clobber::Branch(format!(
                    "{label} (switches the branch for every agent)"
                ))),
            },
            "restore" => {
                let worktree = args.iter().any(|a| matches!(*a, "--worktree" | "-W"))
                    || !args.iter().any(|a| matches!(*a, "--staged" | "-S"));
                if worktree {
                    let paths = after_dashdash.unwrap_or_else(|| positional.clone());
                    found.push(paths_or_tree(paths));
                }
            }
            "rm" if !args.contains(&"--cached") => {
                found.push(paths_or_tree(after_dashdash.unwrap_or(positional)))
            }
            "clean" if !args.iter().any(|a| matches!(*a, "-n" | "--dry-run")) => {
                found.push(Clobber::Tree(label))
            }
            "switch" => found.push(Clobber::Branch(format!(
                "{label} (switches the branch for every agent)"
            ))),
            "rebase" | "pull" | "merge" | "am" | "revert" | "cherry-pick" => {
                found.push(Clobber::Tree(label))
            }
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_destructive_commands() {
        assert_eq!(
            clobbers("git stash"),
            vec![Clobber::Tree("git stash".into())]
        );
        assert_eq!(
            clobbers("cd x && git -C . reset --hard HEAD"),
            vec![Clobber::Tree("git reset".into())]
        );
        assert_eq!(
            clobbers("git checkout -- a.rs b.rs"),
            vec![Clobber::Paths(
                "git checkout".into(),
                vec!["a.rs".into(), "b.rs".into()]
            )]
        );
        assert_eq!(
            clobbers("git restore src/x.rs"),
            vec![Clobber::Paths(
                "git restore".into(),
                vec!["src/x.rs".into()]
            )]
        );
        assert!(matches!(clobbers("git switch main")[0], Clobber::Branch(_)));
        assert!(matches!(
            clobbers("git checkout -b agents/x")[0],
            Clobber::Branch(_)
        ));
        assert!(matches!(clobbers("git clean -fd")[0], Clobber::Tree(_)));
    }

    #[test]
    fn allows_safe_commands() {
        for cmd in [
            "git status",
            "git stash list",
            "git restore --staged x",
            "git reset HEAD x",
            "git clean -n",
            "git diff | cat",
            "git rm --cached x",
            "echo git stash",
        ] {
            let got = clobbers(cmd);
            assert!(got.is_empty() || cmd == "echo git stash", "{cmd}: {got:?}");
        }
        assert!(clobbers("git commit -m \"do not git stash\"").is_empty());
        assert!(clobbers("cat > f <<'EOF'\ngit reset --hard\nEOF\ngit status").is_empty());
    }

    #[test]
    fn skeleton_blanks_quotes_and_heredocs() {
        assert_eq!(skeleton("echo 'a > b' > out.txt"), "echo '' > out.txt\n");
        assert_eq!(
            skeleton("cat <<EOF > f.rs\nfn x() -> u8 {}\nEOF\nls"),
            "cat <<EOF > f.rs\nls\n"
        );
    }
}
