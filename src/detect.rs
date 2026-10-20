//! A check for a repository whose low.toml names none, worked out from the files at its root: a Makefile's check or
//! test target, then the first of Cargo.toml, package.json, a pytest setup, go.mod and mix.exs, each with its build and
//! tests first and its build alone after. A new session runs them in that order on the tree as it finds it, before its
//! system prompt is written, and adopts the first that passes: tests that already fail can't hold its verdicts, and a
//! repository whose tests need a database it doesn't have still gets its build checked. One that settled for less moves
//! up to a fuller command once that passes on a turn's final tree, unless it ran out of time here: in 23 bench sessions
//! (2026-10-20) whose tests failed as found, every verdict rested on `cargo build -q` alone. check = "" in low.toml
//! turns this off. In the session logs (2026-10-19), 508 of 709 real turns ended NO CHECK, 502 of them in one
//! repository without a low.toml, where the model ran its type checker itself 34 times.
//!
//! Each result is kept for an hour in ~/.low/checks, by tree and command, behind a lock: lows started together in one
//! tree, as --tasks batches are, wait for one baseline and reuse it instead of each running their own.
use crate::agent::{tree_at, trial};
use crate::{compact, ui};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long a baseline may run, and a fuller command a session moves up to: a check slower than this would be slow at
/// every turn's end too, so the build alone, next in line, is tried instead.
pub const TIMEOUT: Duration = Duration::from_mins(5);
/// Whether a command ran out of time, from what trial() gives in place of its output then.
pub fn timed_out(out: &str) -> bool {
    out.starts_with("timeout after ")
}
/// How long another low's baseline on the same tree is reused.
const FRESH_MS: u64 = 3_600_000;
/// Baselines kept per repository.
const KEPT: usize = 20;

/// What a rule makes of the repository: the file it read, and the commands to try, fullest first.
type Rule = Option<(&'static str, Vec<String>)>;

/// A command to try, and the file it was worked out from.
#[derive(Debug, PartialEq)]
pub struct Candidate {
    pub command: String,
    pub from: &'static str,
}

/// The commands to try, fullest first: the Makefile's target, then the first manifest's.
pub fn candidates(root: &Path) -> Vec<Candidate> {
    let mut found: Vec<Candidate> =
        make(root).into_iter().map(|command| Candidate { command, from: "Makefile" }).collect();
    let rules: [fn(&Path) -> Rule; 5] = [cargo, node, pytest, go, mix];
    if let Some((from, commands)) = rules.iter().find_map(|rule| rule(root)) {
        found.extend(commands.into_iter().map(|command| Candidate { command, from }));
    }
    found
}

/// `make check`, else `make test`, when the Makefile has that target.
fn make(root: &Path) -> Option<String> {
    let text =
        ["GNUmakefile", "makefile", "Makefile"].iter().find_map(|f| std::fs::read_to_string(root.join(f)).ok())?;
    let targets: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with(|c: char| c.is_whitespace() || c == '#'))
        .filter_map(|line| line.split_once(':').filter(|(_, rest)| !rest.starts_with('=')).map(|(names, _)| names))
        .flat_map(str::split_whitespace)
        .collect();
    ["check", "test"].into_iter().find(|t| targets.contains(t)).map(|t| format!("make {t}"))
}

/// In a workspace whose root is a package too, plain cargo builds and tests that package alone, so a workspace that
/// sets no default-members gets --workspace, which takes in its members. default-members is the repository's own
/// choice of what plain cargo runs.
fn cargo(root: &Path) -> Rule {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let manifest: toml::Table = toml::from_str(&text).unwrap_or_default();
    let workspace = manifest.get("workspace").and_then(toml::Value::as_table);
    let all = if workspace.is_some_and(|w| !w.contains_key("default-members")) { " --workspace" } else { "" };
    Some(("Cargo.toml", vec![format!("cargo build -q{all} && cargo test -q{all}"), format!("cargo build -q{all}")]))
}

/// The package.json's check or typecheck script and its test script, with the lockfile's package manager; CI=1 keeps
/// test runners such as vitest out of watch mode.
fn node(root: &Path) -> Rule {
    let text = std::fs::read_to_string(root.join("package.json")).ok()?;
    let package: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = package["scripts"].as_object()?;
    let real = |name: &str| {
        let script = scripts.get(name).and_then(|s| s.as_str()).map(str::trim).unwrap_or_default();
        // npm init's placeholder test: echo "Error: no test specified" && exit 1
        !script.is_empty() && !script.contains("no test specified")
    };
    let locks = [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("bun.lock", "bun"), ("bun.lockb", "bun")];
    let pm = locks.iter().find(|(lock, _)| root.join(lock).is_file()).map_or("npm", |(_, pm)| pm);
    // `<pm> run <script>`: yarn check, without run, is yarn's own command.
    let types = ["check", "typecheck"].into_iter().find(|s| real(s)).map(|s| format!("{pm} run {s}"));
    let test = real("test").then(|| format!("CI=1 {pm} run test"));
    let commands = match (types, test) {
        (Some(types), Some(test)) => vec![format!("{types} && {test}"), types],
        (types, test) => types.or(test).into_iter().collect(),
    };
    (!commands.is_empty()).then_some(("package.json", commands))
}

/// pytest, when a config file sets it up or the project's requirements name it: with the repository's .venv when it
/// has one, and without the bytecode and cache files a tree git doesn't ignore would count as changed.
fn pytest(root: &Path) -> Rule {
    let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap_or_default();
    let configs = [
        ("pytest.ini", ""),
        ("conftest.py", ""),
        ("pyproject.toml", "pytest"),
        ("setup.cfg", "pytest"),
        ("tox.ini", "[pytest]"),
        ("requirements-dev.txt", "pytest"),
        ("requirements.txt", "pytest"),
    ];
    let (from, _) = configs.into_iter().find(|(f, needle)| root.join(f).is_file() && read(f).contains(needle))?;
    let python = if root.join(".venv/bin/python").is_file() { ".venv/bin/python" } else { "python3" };
    Some((from, vec![format!("PYTHONDONTWRITEBYTECODE=1 {python} -m pytest -q -p no:cacheprovider")]))
}

fn go(root: &Path) -> Rule {
    root.join("go.mod")
        .is_file()
        .then(|| ("go.mod", vec!["go build ./... && go test ./...".into(), "go build ./...".into()]))
}

fn mix(root: &Path) -> Rule {
    let build = "mix compile --warnings-as-errors";
    root.join("mix.exs").is_file().then(|| ("mix.exs", vec![format!("{build} && mix test"), build.into()]))
}

/// One baseline: a command, the tree it ran on, and how it went.
#[derive(Serialize, Deserialize)]
struct Record {
    root: String,
    tree: String,
    check: String,
    ok: bool,
    out: String,
    at: u64,
}

/// What low worked out for a repository whose low.toml names no check, and how its baseline went.
#[derive(Clone, Default)]
pub struct Detected {
    /// The file the check was worked out from: the adopted one's, else the first tried.
    pub from: String,
    /// The first command that passed on the tree as found; None when every one failed.
    pub check: Option<String>,
    /// The commands that failed on the tree as found, fullest first.
    pub failed: Vec<String>,
    /// Those of them that ran out of time, as found or at a turn's end: too slow for every turn's end, so the check
    /// never moves up to them.
    pub slow: Vec<String>,
    /// The tree the adopted command passed on and its output, so a check() before any change reuses the pass.
    pub passed: Option<(String, String)>,
}

impl Detected {
    /// What a session that checks with `check` may move up to on a turn's final tree, fullest first: the commands
    /// fuller than it that failed on the tree as found, but not for time.
    pub fn fuller(&self, check: &str) -> Vec<String> {
        let fuller = self.failed.iter().take_while(|c| *c != check);
        fuller.filter(|c| !self.slow.contains(c)).cloned().collect()
    }
}

/// Runs the candidates on the tree as found, fullest first, and adopts the first that passes. None when the repository
/// gives nothing to try. Ctrl-C skips it, and the session goes without a check.
pub async fn baseline(root: &Path, home: &Path, yolo: bool) -> Option<Detected> {
    let candidates = candidates(root);
    let first = candidates.first()?;
    let dir = home.join(".low/checks");
    // Its private index lives here too, so the folder comes first.
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    let index = dir.join(format!("{}.index", std::process::id()));
    let detected = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            ui::note("check: Ctrl-C skipped the baseline, so this session has no check");
            Detected { from: first.from.into(), ..Default::default() }
        }
        detected = run(root, &dir, &index, yolo, &candidates) => detected,
    };
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_file(index.with_added_extension("lock"));
    Some(detected)
}

async fn run(root: &Path, dir: &Path, index: &Path, yolo: bool, candidates: &[Candidate]) -> Detected {
    // The tree as this low found it, before waiting: a low that ran its baseline first may be changing it by now.
    let found = tree_at(root, index, true).await;
    let held = lock(root, dir).await;
    let mut records: Vec<Record> = held.as_ref().map(read).unwrap_or_default();
    let now = ui::now_ms();
    records.retain(|r| now.saturating_sub(r.at) < FRESH_MS);
    let key = root.to_string_lossy().into_owned();
    let mut detected = Detected { from: candidates[0].from.into(), ..Default::default() };
    for (i, Candidate { command, from }) in candidates.iter().enumerate() {
        let clock = Instant::now();
        let shown = ui::printable(command);
        let kept = found
            .as_ref()
            .ok()
            .and_then(|tree| records.iter().find(|r| r.root == key && r.tree == *tree && r.check == *command));
        let reused = kept.is_some();
        let (ok, out, tree) = match kept.map(|r| (r.ok, r.out.clone(), r.at)) {
            Some((ok, out, at)) => {
                let ago = match now.saturating_sub(at) / 60_000 {
                    0 => "under a minute".into(),
                    minutes => format!("{minutes} min"),
                };
                let how = if ok { "passed" } else { "failed" };
                ui::note(&format!("check: `{shown}` {how} on this tree {ago} ago, in another low; reusing that"));
                (ok, out, found.as_ref().ok().cloned())
            }
            None => {
                let text = if i == 0 {
                    format!("check: low.toml sets none; running `{shown}` (from {from}) on the tree as found")
                } else {
                    format!("check: trying `{shown}` (from {from}) instead")
                };
                ui::note(&text);
                let before = tree_at(root, index, true).await;
                let (ok, out, _) = trial(root, index, true, yolo, command, &before, TIMEOUT).await;
                let tree = before.ok();
                if let Some(tree) = &tree {
                    let (check, out) = (command.clone(), compact::tail(&out, 1500).to_string());
                    records.push(Record { root: key.clone(), tree: tree.clone(), check, ok, out, at: now });
                }
                (ok, out, tree)
            }
        };
        let ms = ui::ms(clock);
        let out_tail = compact::tail(&out, 1500);
        let entry = json!({"ok": ok, "baseline": true, "check": command, "from": from, "reused": reused,
                           "ms": ms, "out": out_tail});
        ui::log(json!({"check": entry}));
        if ok {
            if !reused {
                ui::note(&format!(
                    "check: `{shown}` passed in {}; this session checks with it (check = \"\" in low.toml turns that off)",
                    ui::minutes(ms / 1000)
                ));
            }
            detected.from = (*from).into();
            detected.check = Some(command.clone());
            detected.passed = tree.map(|tree| (tree, out));
            break;
        }
        if !reused {
            ui::note(&format!("check: `{shown}` failed on the tree as found: {}", reason(&out)));
        }
        detected.failed.push(command.clone());
        if timed_out(&out) {
            detected.slow.push(command.clone());
        }
    }
    if detected.check.is_none() {
        ui::note(
            "check: nothing low detected passes on the tree as found, so this session has none; set check in low.toml",
        );
    }
    if let Some(file) = held {
        records.drain(..records.len().saturating_sub(KEPT));
        write(file, &records);
    }
    detected
}

/// The repository's baselines file, locked: one low at a time runs a baseline in a repository, and the rest wait to
/// reuse it. None when ~/.low can't hold it: the baseline then runs unshared.
async fn lock(root: &Path, dir: &Path) -> Option<std::fs::File> {
    let mut hasher = std::hash::DefaultHasher::new();
    root.hash(&mut hasher);
    let path = dir.join(format!("{:016x}.json", hasher.finish()));
    let file =
        std::fs::File::options().read(true).write(true).create(true).truncate(false).mode(0o600).open(path).ok()?;
    let mut told = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if !std::mem::replace(&mut told, true) {
                    ui::note("check: waiting for another low's baseline in this repository");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::Error(_)) => return None,
        }
    }
}

fn read(mut file: &std::fs::File) -> Vec<Record> {
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_default()
}

fn write(mut file: std::fs::File, records: &[Record]) {
    let Ok(text) = serde_json::to_string(records) else { return };
    let _ = file.set_len(0).and_then(|()| file.rewind()).and_then(|()| file.write_all(text.as_bytes()));
}

/// Why a baseline failed, in a line: the output's last line, or low's own verdict when that is more than the command.
fn reason(out: &str) -> String {
    let mut lines = out.lines().rev().map(str::trim).filter(|l| !l.is_empty());
    let verdict = lines.next().unwrap_or_default();
    let line = match lines.next() {
        Some(last) if verdict.starts_with("[check FAILED: ") && !verdict.contains("changed the working tree") => last,
        _ => verdict,
    };
    let line: String = line.chars().take(200).collect();
    ui::printable(&line)
}

#[cfg(test)]
mod tests {
    use super::{Candidate, Detected, candidates, reason};
    use std::path::{Path, PathBuf};

    fn repo(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("low-detect-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn commands(root: &Path) -> Vec<String> {
        let found = candidates(root);
        let _ = std::fs::remove_dir_all(root);
        found.into_iter().map(|c| c.command).collect()
    }

    #[test]
    fn makefile_first_then_one_manifest() {
        let root = repo(
            "make",
            &[("Makefile", "VAR := x\ncheck test: build\n\tcargo test\n"), ("Cargo.toml", ""), ("go.mod", "")],
        );
        let found = candidates(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found[0], Candidate { command: "make check".into(), from: "Makefile" });
        assert_eq!(found[1].from, "Cargo.toml");
        assert_eq!(found.len(), 3, "go.mod is not tried after Cargo.toml");
        let root = repo("make-test", &[("Makefile", "check := 1\ntest:\n\t./run\n")]);
        assert_eq!(commands(&root), ["make test"], "check := is a variable, not a target");
        assert!(commands(&repo("none", &[("README.md", "hi")])).is_empty());
    }

    #[test]
    fn ladders() {
        let go = commands(&repo("go", &[("go.mod", "module x")]));
        assert_eq!(go, ["go build ./... && go test ./...", "go build ./..."]);
        let mix = commands(&repo("mix", &[("mix.exs", "")]));
        assert_eq!(mix, ["mix compile --warnings-as-errors && mix test", "mix compile --warnings-as-errors"]);
        let cargo = commands(&repo("cargo", &[("Cargo.toml", "")]));
        assert_eq!(cargo, ["cargo build -q && cargo test -q", "cargo build -q"]);
        let rooted = "[package]\nname = \"app\"\n\n[workspace]\nmembers = [\"crates/core\"]\n";
        let workspace = commands(&repo("cargo-workspace", &[("Cargo.toml", rooted)]));
        let members = ["cargo build -q --workspace && cargo test -q --workspace", "cargo build -q --workspace"];
        assert_eq!(workspace, members);
        let chosen = format!("{rooted}default-members = [\"crates/core\"]\n");
        let defaults = commands(&repo("cargo-defaults", &[("Cargo.toml", &chosen)]));
        assert_eq!(defaults, cargo, "default-members says what plain cargo runs");
        let broken = commands(&repo("cargo-broken", &[("Cargo.toml", "[workspace")]));
        assert_eq!(broken, cargo, "a manifest that doesn't parse is still a Rust repository");
    }

    #[test]
    fn package_scripts() {
        let both = r#"{"scripts": {"check": "svelte-check", "test": "vitest"}}"#;
        let found = commands(&repo("node-both", &[("package.json", both), ("pnpm-lock.yaml", "")]));
        assert_eq!(found, ["pnpm run check && CI=1 pnpm run test", "pnpm run check"]);
        let placeholder = r#"{"scripts": {"typecheck": "tsc", "test": "echo \"Error: no test specified\" && exit 1"}}"#;
        let found = commands(&repo("node-placeholder", &[("package.json", placeholder), ("yarn.lock", "")]));
        assert_eq!(found, ["yarn run typecheck"]);
        let test = r#"{"scripts": {"test": "jest"}}"#;
        assert_eq!(commands(&repo("node-test", &[("package.json", test)])), ["CI=1 npm run test"]);
        let none = r#"{"scripts": {"dev": "vite"}}"#;
        assert!(commands(&repo("node-none", &[("package.json", none)])).is_empty());
        assert!(commands(&repo("node-bad", &[("package.json", "{")])).is_empty());
    }

    #[test]
    fn pytest_setups() {
        let pytest = "PYTHONDONTWRITEBYTECODE=1 python3 -m pytest -q -p no:cacheprovider";
        assert_eq!(commands(&repo("py-ini", &[("pytest.ini", "")])), [pytest]);
        let found =
            commands(&repo("py-venv", &[("pyproject.toml", "[tool.pytest.ini_options]\n"), (".venv/bin/python", "")]));
        assert_eq!(found, [pytest.replace("python3", ".venv/bin/python")]);
        assert!(commands(&repo("py-plain", &[("pyproject.toml", "[project]\nname = 'x'\n")])).is_empty());
        assert_eq!(commands(&repo("py-req", &[("requirements-dev.txt", "pytest==8\n")])), [pytest]);
    }

    /// The session header keeps the check redacted, and a resumed session adopts it from there: low's own commands
    /// must come through whole.
    #[test]
    fn commands_survive_redaction() {
        let package = r#"{"scripts": {"typecheck": "tsc", "test": "vitest"}}"#;
        let repos = [
            repo("redact-make", &[("Makefile", "check:\n\ttrue\n"), ("Cargo.toml", "")]),
            repo("redact-cargo", &[("Cargo.toml", "[workspace]\nmembers = []\n")]),
            repo("redact-node", &[("package.json", package), ("bun.lock", "")]),
            repo("redact-py", &[("pytest.ini", ""), (".venv/bin/python", "")]),
            repo("redact-go", &[("go.mod", "")]),
            repo("redact-mix", &[("mix.exs", "")]),
        ];
        for root in repos {
            for command in commands(&root) {
                assert_eq!(crate::redact::redact(&command).0, command);
            }
        }
    }

    /// A session moves up only to a command fuller than the one it checks with, and never to one that ran out of time.
    #[test]
    fn moving_up() {
        let failed = ["make check", "cargo build -q && cargo test -q"].map(String::from).to_vec();
        let detected = Detected { failed: failed.clone(), ..Default::default() };
        assert_eq!(detected.fuller("cargo build -q"), failed);
        assert_eq!(detected.fuller("cargo build -q && cargo test -q"), ["make check"]);
        assert!(detected.fuller("make check").is_empty());
        let slow = Detected { slow: vec!["make check".into()], ..detected };
        assert_eq!(slow.fuller("cargo build -q"), ["cargo build -q && cargo test -q"]);
    }

    #[test]
    fn reasons() {
        assert_eq!(reason("compiling\nerror[E0425]: x\n[check FAILED: cargo build -q]"), "error[E0425]: x");
        let changed = "ok\n[check FAILED: the check changed the working tree]";
        assert_eq!(reason(changed), "[check FAILED: the check changed the working tree]");
        assert_eq!(reason("[check FAILED: make test]"), "[check FAILED: make test]");
    }
}
