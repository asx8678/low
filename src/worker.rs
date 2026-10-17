use anyhow::{Context, Result, anyhow, bail};
use rustix::process::{Pid, Signal, getpgrp, kill_process, kill_process_group};
use serde_json::{Value, json};
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const RUN_TIMEOUT: Duration = Duration::from_mins(10);
const HERMETIC: &str = "TZ=UTC LANG=C.UTF-8 LC_ALL=C.UTF-8 NO_COLOR=1 CLICOLOR=0 CARGO_TERM_COLOR=never CI=1 TERM=dumb \
                        PAGER=cat GIT_PAGER=cat GIT_TERMINAL_PROMPT=0 PYTHONHASHSEED=0 PYTHONUTF8=1 PYTHONDONTWRITEBYTECODE=1";
const CACHES: &str =
    "XDG_CACHE_HOME GOCACHE UV_CACHE_DIR PIP_CACHE_DIR PRE_COMMIT_HOME npm_config_cache PYTHONPYCACHEPREFIX";
const SECRETS: &str = ".low, .config/low, .ssh, .gnupg, .aws, .azure, .kube, .docker, .netrc, .pi, .config/gh, \
                       .config/gcloud, .config/op, .password-store, .git-credentials, .config/git/credentials, \
                       .npmrc, .pypirc, .cargo/credentials, .cargo/credentials.toml, .vault-token, .claude, \
                       .claude.json, .codex, .zsh_history, .zsh_sessions, .bash_history, .python_history, \
                       .node_repl_history, Library/Keychains, Library/Cookies, Library/Application Support/Google, \
                       Library/Application Support/Firefox, .terraform.d, .m2/settings.xml, .gradle/gradle.properties, \
                       .oci, .subversion/auth, .config/helm";
const ENV: &str = "PATH HOME USER LOGNAME SHELL LANG LC_* TERM TMPDIR TZ PWD CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN \
                   GOPATH GOROOT GOFLAGS VIRTUAL_ENV CONDA_PREFIX PYENV_ROOT NVM_DIR JAVA_HOME MIX_HOME HEX_HOME ERL_* \
                   ELIXIR_* DEVELOPER_DIR SDKROOT";

static EXTRA_ENV: OnceLock<Vec<String>> = OnceLock::new();

pub fn allow_env(names: Vec<String>) {
    let _ = EXTRA_ENV.set(names);
}

#[derive(Debug, thiserror::Error)]
#[error("the worker could not start")]
pub struct StartFailed;

pub struct Worker {
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    busy: bool,
    ready: bool,
    child: Child,
    started: std::time::Instant,
}

impl Worker {
    pub fn start(root: &Path, yolo: bool, check: Option<&str>, fmt: Option<&str>) -> Result<Self> {
        let bwrap = || std::process::Command::new("bwrap").arg("--version").output().is_ok_and(|o| o.status.success());
        if !yolo && !cfg!(target_os = "macos") && !bwrap() {
            return Err(
                anyhow!("bwrap (bubblewrap) is not installed; install it or run with --yolo").context(StartFailed)
            );
        }
        let guarded = json!(protected(root).iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>()).to_string();
        let mut child = sandboxed(root, yolo, &python(), &["-I", "-B", "-u", "-c", include_str!("worker.py")])
            .env("LOW_CHECK", if check.is_some() { "1" } else { "" })
            .env("LOW_FMT", fmt.unwrap_or_default())
            .env("LOW_PROTECTED", guarded)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .context("starting the python3 worker")
            .context(StartFailed)?;
        let stdin = child.stdin.take().context("worker stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("worker stdout")?).lines();
        Ok(Self { stdin, stdout, busy: false, ready: false, child, started: std::time::Instant::now() })
    }

    pub async fn run(&mut self, code: &str, state: &Value) -> Result<Value> {
        if !self.ready {
            let line = tokio::time::timeout(Duration::from_secs(60), self.stdout.next_line()).await;
            let Ok(Ok(Some(line))) = line else {
                return Err(anyhow!("the worker exited before it was ready").context(StartFailed));
            };
            self.ready = true;
            let python = serde_json::from_str::<Value>(&line).unwrap_or_default()["python"].take();
            let ms = crate::ui::ms(self.started);
            let text = format!("python {} worker ready in {ms} ms", python.as_str().unwrap_or("?"));
            crate::ui::record("worker", &text, json!({"pid": self.child.id(), "python": python, "ms": ms}));
        }
        self.send(json!({"code": code, "state": state})).await
    }

    pub async fn resume(&mut self, ok: bool, out: &str) -> Result<Value> {
        self.send(json!({"ok": ok, "out": out})).await
    }

    async fn send(&mut self, request: Value) -> Result<Value> {
        self.busy = true;
        self.stdin.write_all(format!("{request}\n").as_bytes()).await?;
        let reply = tokio::time::timeout(RUN_TIMEOUT, self.stdout.next_line()).await.context("run() timed out")??;
        let reply: Value = serde_json::from_str(&reply.context("the worker exited")?)?;
        self.busy = reply["needs_check"] == true;
        Ok(reply)
    }

    pub async fn settle(&mut self) -> Result<Option<Value>> {
        if !self.busy {
            return Ok(None);
        }
        self.busy = false;
        if let Some(pid) = self.child.id().and_then(|pid| Pid::from_raw(pid.cast_signed())) {
            let _ = kill_process(pid, Signal::INT);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, self.stdout.next_line()).await {
                Ok(Ok(Some(line))) => {
                    let reply: Value = serde_json::from_str(&line)?;
                    if reply["needs_check"] != true {
                        return Ok(Some(reply));
                    }
                }
                _ => bail!("the worker did not report back"),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let Some(pid) = self.child.id().map(u32::cast_signed) else { return };
        kill_groups(descendants(pid).into_iter().chain([pid]));
    }
}

/// SIGKILLs these process groups, never low's own or groups 0 and 1.
pub fn kill_groups(groups: impl IntoIterator<Item = i32>) {
    let own = Pid::as_raw(Some(getpgrp()));
    for group in groups.into_iter().filter(|&g| g > 1 && g != own).filter_map(Pid::from_raw) {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

pub fn descendants(pid: i32) -> Vec<i32> {
    let ps = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,pgid="]).output();
    let table = ps.map(|o| String::from_utf8_lossy_owned(o.stdout)).unwrap_or_default();
    let rows: Vec<[i32; 3]> = table
        .lines()
        .filter_map(|l| l.split_whitespace().filter_map(|n| n.parse().ok()).collect::<Vec<i32>>().as_array().copied())
        .collect();
    let (mut tree, mut groups, mut i) = (vec![pid], vec![], 0);
    while i < tree.len() {
        let parent = tree[i];
        for &[child, _, group] in rows.iter().filter(|&&[child, ppid, _]| ppid == parent && child != parent) {
            tree.push(child);
            groups.push(group);
        }
        i += 1;
    }
    groups
}

pub fn sandboxed(root: &Path, yolo: bool, program: &str, args: &[&str]) -> Command {
    let writable = writable(root);
    let secrets = secrets();
    let protected = protected(root);
    let mut cmd = if yolo {
        Command::new(program)
    } else if cfg!(target_os = "macos") {
        let subpaths = |dirs: &[PathBuf]| -> String { dirs.iter().map(|d| format!(" (subpath {d:?})")).collect() };
        let (dirs, secrets, protected) = (subpaths(&writable), subpaths(&secrets), subpaths(&protected));
        let sockets = root.join("tmp");
        let profile = format!(
            "(version 1)(allow default)(deny network*)\
             (allow network-outbound (remote ip \"localhost:*\"))\
             (allow network-bind network-inbound (local ip \"localhost:*\"))\
             (allow network-bind network-outbound network-inbound \
              (local unix-socket (subpath {sockets:?})) (remote unix-socket (subpath {sockets:?})))\
             (deny file-write*)(allow file-write*{dirs} (subpath \"/dev\"))(deny file-write*{protected})\
             (deny file-write* (require-all (subpath {root:?}) (regex #\"/\\.[gG][iI][tT](/|$)\")))\
             (deny file-read-data{secrets})\
             (deny file-ioctl (regex #\"^/dev/tty\"))(deny file-write* (regex #\"^/dev/tty\"))(deny appleevent-send)\
             (deny process-exec (literal \"/usr/bin/osascript\") (literal \"/usr/bin/osacompile\") \
              (literal \"/usr/bin/open\") (literal \"/bin/launchctl\") (literal \"/usr/bin/automator\"))"
        );
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile).arg(program);
        cmd
    } else {
        let mut cmd = Command::new("bwrap");
        cmd.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/run"]);
        for dir in &writable {
            cmd.arg("--bind").arg(dir).arg(dir);
        }
        for path in secrets.iter().filter(|p| p.exists()) {
            if path.is_dir() {
                cmd.arg("--tmpfs").arg(path);
            } else {
                cmd.arg("--ro-bind").arg("/dev/null").arg(path);
            }
        }
        for dir in protected.iter().chain(&nested(root)).filter(|d| d.exists()) {
            cmd.arg("--ro-bind").arg(dir).arg(dir);
        }
        cmd.args(["--unshare-all", "--die-with-parent", "--new-session", "--chdir"]).arg(root).arg(program);
        cmd
    };
    let hash = BuildHasherDefault::<DefaultHasher>::new().hash_one(root);
    let cache = std::env::temp_dir().join(format!("low-cache/{hash:016x}"));
    let _ = std::fs::create_dir_all(&cache);
    let extra = EXTRA_ENV.get().into_iter().flatten().map(String::as_str);
    let allowed: Vec<&str> = ENV.split_whitespace().chain(extra).collect();
    let allowed =
        |name: &str| allowed.iter().any(|p| p.strip_suffix('*').map_or(*p == name, |prefix| name.starts_with(prefix)));
    cmd.args(args)
        .current_dir(root)
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(allowed)))
        .envs(HERMETIC.split_whitespace().filter_map(|pin| pin.split_once('=')))
        .envs(CACHES.split_whitespace().map(|name| (name, cache.as_os_str())))
        .kill_on_drop(true);
    cmd
}

/// The secret paths in the home directory, which the sandbox hides from the worker and the check. The sandbox matches
/// resolved paths, so each one also brings its real path and the targets of links inside it (a dotfiles repo's ~/.ssh
/// or ~/.aws/credentials).
fn secrets() -> Vec<PathBuf> {
    let home = home();
    let mut paths = vec![];
    for path in SECRETS.split(',').map(|s| home.join(s.trim())) {
        resolved(&path, &home, &mut paths);
        linked(&path, &home, 3, &mut paths);
        paths.push(path);
    }
    paths
}

/// The home directory $HOME names, resolved; empty when HOME is unset.
pub fn home() -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    home.canonicalize().unwrap_or(home)
}

/// Adds where `path` really is, if that differs and is a file or directory other than the home directory or one above
/// it: a history file linked to /dev/null must not take /dev/null from the worker.
fn resolved(path: &Path, home: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(real) = path.canonicalize()
        && real != path
        && real.metadata().is_ok_and(|m| m.is_file() || m.is_dir())
        && !home.starts_with(&real)
    {
        out.push(real);
    }
}

/// Adds the targets of the links in `dir`, down to `depth` levels of real directories.
fn linked(dir: &Path, home: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => resolved(&entry.path(), home, out),
            Ok(kind) if kind.is_dir() => linked(&entry.path(), home, depth - 1, out),
            _ => {}
        }
    }
}

/// Whether the sandbox keeps this canonical path from the worker.
pub fn secret(path: &Path) -> bool {
    secrets().iter().any(|s| path.starts_with(s))
}

fn protected(root: &Path) -> Vec<PathBuf> {
    let out = std::process::Command::new("git").arg("-C").arg(root).args(["config", "core.hooksPath"]).output();
    let hooks = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut dirs = vec![root.join(".git")];
    if !hooks.is_empty() {
        let home = std::env::var("HOME").unwrap_or_default();
        let hooks = root.join(hooks.strip_prefix("~/").map_or(hooks.clone(), |rest| format!("{home}/{rest}")));
        let hooks = hooks.canonicalize().unwrap_or(hooks);
        dirs.push(if hooks.ends_with("_") { hooks.parent().unwrap_or(&hooks).into() } else { hooks });
    }
    dirs
}

fn nested(root: &Path) -> Vec<PathBuf> {
    let mut git = std::process::Command::new("git");
    let out = git.args(["-c", "core.fsmonitor=false", "-C"]).arg(root).args(["ls-files", "--stage", "-z"]).output();
    let list = out.map(|o| String::from_utf8_lossy_owned(o.stdout)).unwrap_or_default();
    let mut found: Vec<PathBuf> = list
        .split('\0')
        .filter_map(|e| e.strip_prefix("160000 ")?.split_once('\t').map(|(_, path)| root.join(path).join(".git")))
        .collect();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                if dir != root {
                    found.push(path);
                }
            } else if depth < 3 && entry.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push((path, depth + 1));
            }
        }
    }
    found
}

/// Whether the sandbox keeps `root` read-only: the home directory or a folder above it, where writes could reach shell
/// startup files and launch agents that later run outside the sandbox.
pub fn read_only(root: &Path) -> bool {
    let home = home();
    !home.as_os_str().is_empty() && home.starts_with(root)
}

fn writable(root: &Path) -> Vec<PathBuf> {
    let root = Some(root.to_path_buf()).filter(|r| !read_only(r));
    let dirs = root.into_iter().chain([std::env::temp_dir(), "/tmp".into()]);
    dirs.filter_map(|d| d.canonicalize().ok()).collect()
}

/// The worker's interpreter: the newest python3.N on PATH, else python3, which on macOS is the system's 3.9.
fn python() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    newest_python(std::env::split_paths(&path)).map_or_else(|| "python3".into(), |p| p.to_string_lossy().into_owned())
}

/// The highest runnable python3.N in these directories; on a tie the earliest directory wins, as PATH would pick.
fn newest_python(dirs: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let entries = dirs.flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten());
    let minor = |e: &std::fs::DirEntry| e.file_name().to_str()?.strip_prefix("python3.")?.parse::<u32>().ok();
    let runnable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    let found = entries.filter_map(|e| Some((minor(&e)?, e.path()))).filter(|(_, p)| runnable(p));
    found.min_by_key(|(minor, _)| std::cmp::Reverse(*minor)).map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    /// Runs code the way the agent does, answering check() with a pass.
    async fn run(worker: &mut super::Worker, code: &str) -> Value {
        let mut reply = worker.run(code, &json!({})).await.unwrap();
        while reply["needs_check"] == true {
            reply = worker.resume(true, "[check passed: true]").await.unwrap();
        }
        reply
    }

    /// Drives the worker.py this build embeds through its helpers: low's own check runs no other code of it, and the
    /// worker serving a session is the installed low's copy, which never reloads.
    #[tokio::test]
    async fn worker_helpers() {
        let dir = std::env::temp_dir().join(format!("low-worker-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut worker = super::Worker::start(&dir, true, Some("true"), None).unwrap();
        let whole = [
            ("x = 41\nx + 1", "42\n"),
            ("print(sh('echo hi; exit 3'))", "hi\n[exit 3]\n"),
            (
                "write('a.py', 'def f():\\n    return 1\\n')\nshow('a.py')",
                "write a.py: ok\n[a.py: lines 1-2 of 2]\n1: def f():\n2:     return 1\n",
            ),
            ("edit('a.py', 'return 1', 'return 2')\ncheck()", "edit a.py: ok\n[check passed: true]\n"),
            ("def ratio(a, b):\n    return a / b", "(no output)"),
        ];
        for (code, out) in whole {
            assert_eq!(run(&mut worker, code).await["out"], out, "{code}");
        }
        let parts = [
            (
                "edit('a.py', [('return 2', 'return 3'), ('nope', 'x')])",
                "old text not found in a.py (pair 2 of 2; nothing was changed)",
            ),
            (
                "edit('a.py', ['return 2', 'return 3'])",
                "edit() takes (path, old, new) or (path, [(old, new), ...]) with string old and new",
            ),
            ("total = 1\nratio(total, 0)", "    return a / b"),
            ("show = 1", "[show: low's helpers, restored before the next run]"),
            (
                "edit('a.py', 'return 2', 'return 4')\n1 / 0",
                "[applied before the error: edit a.py; inspect before repeating them]",
            ),
        ];
        for (code, part) in parts {
            let reply = run(&mut worker, code).await;
            assert!(reply["out"].as_str().is_some_and(|out| out.contains(part)), "{code}: {reply}");
        }
        assert!(std::fs::read_to_string(dir.join("a.py")).unwrap().contains("return 4"));
        let file = "import os\\n\\n\\ndef first():\\n    x = 1\\n    return x\\n\\n\\nclass K:\\n    def method(self):\\n        y = 2\\n        return y\\n";
        for (code, out) in [
            (
                format!("write('b.py', '{file}')\nshow('b.py', r'return y')"),
                "write b.py: ok\n[b.py: the blocks with a line matching 'return y', lines 10-12 of 12]\n10:     def method(self):\n11:         y = 2\n12:         return y\n",
            ),
            (
                "show('b.py', r'x = 1|y = 2')".into(),
                "[b.py: the blocks with a line matching 'x = 1|y = 2', lines 4-6, 10-12 of 12]\n4: def first():\n5:     x = 1\n6:     return x\n…\n10:     def method(self):\n11:         y = 2\n12:         return y\n",
            ),
            ("show('b.py', 'zzz')".into(), "[b.py: no line matches 'zzz']\n"),
        ] {
            assert_eq!(run(&mut worker, &code).await["out"], out, "{code}");
        }
        drop(worker);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
