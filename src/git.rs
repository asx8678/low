use crate::ui;
use anyhow::{Context, Result};
use std::hash::{Hash, Hasher};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::OnceLock;

const BIG: u64 = 16 << 20;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
static GIT_GUARD: OnceLock<(String, Vec<String>)> = OnceLock::new();
/// One git at a time on low's index: a snapshot abandoned by Ctrl-C keeps running on the blocking pool.
static INDEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// The ref that keeps the snapshots turns start from, so git gc can't take the trees the undo commands name.
const KEPT: &str = "refs/low/snapshots";
/// How long a snapshot stays kept: past it, the snapshot and the untracked files' contents it holds are garbage again.
const KEEP_FOR: u64 = 14 * 86_400;

pub fn guard_git(root: &Path) {
    // Once per process: a check detected at the start guards git before the agent does.
    if GIT_GUARD.get().is_some() {
        return;
    }
    let out = std::process::Command::new("git")
        .args(["config", "--show-scope", "-z", "--get-regexp", r"^(filter|diff)\."])
        .current_dir(root)
        .process_group(0)
        .output();
    let text = out.map(|o| String::from_utf8_lossy_owned(o.stdout)).unwrap_or_default();
    let fields: Vec<&str> = text.split('\0').collect();
    let (mut args, mut local) = (vec![], vec![]);
    for [scope, entry] in fields.as_chunks::<2>().0 {
        let key = entry.split('\n').next().unwrap_or_default();
        let Some((kind, name)) = key.split_once('.').and_then(|(kind, rest)| Some((kind, rest.rsplit_once('.')?.0)))
        else {
            continue;
        };
        if *scope == "local" && !local.contains(&name) {
            local.push(name);
        }
        if kind == "filter" && !name.contains(['=', '\n']) && !args.contains(&format!("filter.{name}.clean=")) {
            for var in ["clean=", "smudge=", "process=", "required=false"] {
                args.extend(["-c".to_string(), format!("filter.{name}.{var}")]);
            }
        }
    }
    if !local.is_empty() {
        ui::note(&format!(
            ".git/config defines filter or diff drivers ({}); low's own git calls skip them",
            local.join(", ")
        ));
    }
    let empty =
        std::process::Command::new("git").args(["hash-object", "-t", "tree", "/dev/null"]).current_dir(root).output();
    let empty = empty.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let _ = GIT_GUARD.set((if empty.is_empty() { EMPTY_TREE.into() } else { empty }, args));
}

pub fn git<S: AsRef<str>>(root: &Path, index: Option<&Path>, args: &[S]) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    let guard = GIT_GUARD.get();
    cmd.args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "core.attributesFile=/dev/null"])
        .args(["-c", "core.quotePath=false", "-c", "core.safecrlf=false"])
        .args(guard.into_iter().flat_map(|g| &g.1))
        .args(args.iter().map(AsRef::as_ref))
        .env("GIT_ATTR_SOURCE", guard.map_or(EMPTY_TREE, |g| g.0.as_str()))
        .current_dir(root)
        .process_group(0);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
        let text = format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        return Err(GitExit(out.status.code(), text).into());
    }
    Ok(String::from_utf8_lossy_owned(out.stdout))
}

/// A git command that failed: its exit code, and the command with what git printed.
#[derive(Debug, thiserror::Error)]
#[error("{1}")]
struct GitExit(Option<i32>, String);

/// The private index is a cache: a leftover lock is dropped, and an index git can no longer use is rebuilt, once.
pub fn snapshot(root: &Path, index: &Path) -> Result<String> {
    let _held = INDEX.lock().unwrap_or_else(|p| p.into_inner());
    let _ = std::fs::remove_file(index.with_added_extension("lock"));
    snapshot_with(root, index).or_else(|e| {
        let _ = std::fs::remove_file(index);
        let tree = snapshot_with(root, index)?;
        ui::note(&format!("rebuilt the change-detection index after: {e:#}"));
        Ok(tree)
    })
}

fn snapshot_with(root: &Path, index: &Path) -> Result<String> {
    if !index.exists() {
        let current = git(root, None, &["rev-parse", "--git-path", "index"]).unwrap_or_default();
        let _ = std::fs::copy(root.join(current.trim()), index);
    }
    let others = git(root, Some(index), &["ls-files", "-z", "-o", "--exclude-standard"])?;
    let mut args = ["add", "-A", "--ignore-errors", "--", "."].map(String::from).to_vec();
    let base = args.len();
    let mut hasher = std::hash::DefaultHasher::new();
    for path in others.split('\0').filter(|p| !p.is_empty()) {
        let Ok(meta) = std::fs::symlink_metadata(root.join(path)) else { continue };
        if meta.len() > BIG {
            (path, meta.len(), meta.modified().ok()).hash(&mut hasher);
            args.push(format!(":(exclude,literal){path}"));
        }
    }
    if let Err(e) = git(root, Some(index), &args) {
        // Unlike a file git cannot add (exit 1), a fatal error (128) writes nothing: write-tree would give a stale tree.
        if e.downcast_ref::<GitExit>().is_some_and(|g| g.0 == Some(128)) {
            return Err(e);
        }
        static SKIPPED: std::sync::Once = std::sync::Once::new();
        SKIPPED.call_once(|| ui::note(&format!("change detection skips what git cannot add: {e:#}")));
    }
    let tree = git(root, Some(index), &["write-tree"])?.trim().to_string();
    Ok(if args.len() > base { format!("{tree} {:016x}", hasher.finish()) } else { tree })
}

/// Keeps a turn's starting snapshot reachable from refs/low/snapshots for KEEP_FOR: the ref's one commit holds each kept
/// snapshot as a folder named by when it was taken, so `git log --all` shows one commit, not one per turn. A snapshot
/// already kept stays as it is; one whose update another low races past isn't kept, as before.
pub fn keep(root: &Path, snapshot: &str) {
    let Some(tree) = snapshot.split(' ').next().filter(|t| !t.is_empty()) else { return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let tip = git(root, None, &["rev-parse", "-q", "--verify", &format!("{KEPT}^{{commit}}")]).ok();
    let tip = tip.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let listed = tip.as_ref().and_then(|t| git(root, None, &["ls-tree", t]).ok()).unwrap_or_default();
    // ls-tree lines: "040000 tree <id>\t<seconds>-<id>".
    let young = |line: &&str| {
        let taken = line.split_once('\t').and_then(|(_, name)| name.split('-').next()?.parse::<u64>().ok());
        taken.is_some_and(|t| now.saturating_sub(t) < KEEP_FOR)
    };
    let mut entries: Vec<&str> = listed.lines().filter(young).collect();
    if entries.iter().any(|l| l.split_once('\t').is_some_and(|(meta, _)| meta.ends_with(tree))) {
        return;
    }
    let new = format!("040000 tree {tree}\t{now}-{tree}");
    entries.push(&new);
    let made = std::process::Command::new("git")
        .args(["mktree"])
        .current_dir(root)
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().map_or(Ok(()), |mut stdin| stdin.write_all((entries.join("\n") + "\n").as_bytes()))?;
            child.wait_with_output()
        });
    let Some(kept) =
        made.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    else {
        return;
    };
    let args = ["-c", "user.name=low", "-c", "user.email=low@localhost", "commit-tree", "--no-gpg-sign", &kept];
    let Ok(commit) = git(root, None, &[&args[..], &["-m", "low: snapshots that undo commands restore"]].concat())
    else {
        return;
    };
    // Against the tip read above, so a race loses this snapshot rather than another low's; and no reflog, which would
    // keep old commits, and the snapshots they hold, past KEEP_FOR.
    let update = ["-c", "core.logAllRefUpdates=false", "update-ref", KEPT, commit.trim(), tip.as_deref().unwrap_or("")];
    let _ = git(root, None, &update);
}

pub fn changes(root: &Path, snapshot: Option<(&str, &Path)>, since: std::time::SystemTime) -> String {
    let _held = INDEX.lock().unwrap_or_else(|p| p.into_inner());
    let (base, index) = snapshot.map_or(("HEAD", None), |(tree, index)| (tree, Some(index)));
    let diff =
        git(root, index, &["diff", "--numstat", "--summary", "--no-renames", "--no-ext-diff", "--no-textconv", base])
            .unwrap_or_default();
    let summary = |mode: &str| -> Vec<&str> {
        diff.lines().filter_map(|l| l.strip_prefix(mode)?.split_once(' ')).map(|(_, p)| p).collect()
    };
    let (created, deleted) = (summary(" create mode "), summary(" delete mode "));
    let mut out: Vec<String> = diff
        .lines()
        .filter_map(|l| match l.splitn(3, '\t').collect::<Vec<_>>()[..] {
            [_, _, path] if created.contains(&path) => Some(format!("{path} (new)")),
            [_, _, path] if deleted.contains(&path) => Some(format!("{path} (deleted)")),
            // numstat counts no lines in a binary file: "-\t-\tpath".
            ["-", "-", path] => Some(format!("{path} (binary)")),
            [added, removed, path] => Some(format!("{path} +{added} −{removed}")),
            _ => None,
        })
        .collect();
    let new = git(root, index, &["ls-files", "-o", "--exclude-standard"]).unwrap_or_default();
    let fresh = |f: &&str| {
        let meta = std::fs::symlink_metadata(root.join(f));
        meta.is_ok_and(|m| m.len() <= BIG || m.modified().is_ok_and(|t| t >= since))
    };
    out.extend(new.lines().filter(fresh).map(|f| format!("{f} (new)")));
    if out.is_empty() { "none".into() } else { out.join("\n") }
}
