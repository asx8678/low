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
                       .claude.json, .codex, .local/share/opencode/auth.json, .zsh_history, .zsh_sessions, \
                       .bash_history, .python_history, .node_repl_history, Library/Keychains, Library/Cookies, \
                       .terraform.d, .terraformrc, .m2/settings.xml, .gradle/gradle.properties, .oci, \
                       .subversion/auth, .config/helm, .local/share/keyrings, \
                       .gemini, .qwen, .cursor, .continue, .config/github-copilot, Library/Application Support/Claude, \
                       .config/glab-cli, .config/hub, .config/stripe, .config/doctl, \
                       Library/Application Support/doctl, .config/rclone, .config/sops, .fly, .wrangler, \
                       .config/.wrangler, Library/Preferences/.wrangler, .s3cfg, .boto, .pgpass, .my.cnf, \
                       .gem/credentials, .huggingface/token, .cache/huggingface/token, \
                       Library/Application Support/Google, Library/Application Support/Firefox, \
                       Library/Application Support/BraveSoftware, Library/Application Support/Microsoft Edge, \
                       Library/Application Support/Chromium, Library/Application Support/Arc, Library/Safari, \
                       Library/Containers/com.apple.Safari, Library/Thunderbird, .mozilla, .thunderbird, \
                       .config/google-chrome, .config/chromium, .config/BraveSoftware, .config/microsoft-edge";
const ENV: &str = "PATH HOME USER LOGNAME SHELL LANG LC_* TERM TMPDIR TZ PWD CARGO_HOME RUSTUP_HOME RUSTUP_TOOLCHAIN \
                   GOPATH GOROOT GOFLAGS VIRTUAL_ENV CONDA_PREFIX PYENV_ROOT NVM_DIR JAVA_HOME MIX_HOME HEX_HOME ERL_* \
                   ELIXIR_* DEVELOPER_DIR SDKROOT";

/// What low itself reads besides the worker's variables: color, its HTTP client's certificates, where git finds its
/// repository and configuration, and the XDG folders. Not GIT_CONFIG_PARAMETERS or GIT_CONFIG_KEY_*/VALUE_*, which
/// can carry an http.extraHeader with a token, nor GIT_ASKPASS or GIT_SSH*: low's git never talks to a remote.
const OWN_ENV: &str = "NO_COLOR COLUMNS LINES RUST_BACKTRACE XDG_* GIT_DIR GIT_WORK_TREE GIT_CEILING_DIRECTORIES \
                       GIT_DISCOVERY_ACROSS_FILESYSTEM GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_NOSYSTEM \
                       GIT_EXEC_PATH GIT_AUTHOR_* GIT_COMMITTER_* GIT_TRACE* SSL_CERT_FILE SSL_CERT_DIR";
/// The proxies low's HTTP client reads. A proxy's URL can hold its password, so they come along through the pipe with
/// LOW_API_KEY, not in the environment, and the client is given them directly.
const PROXY_ENV: &str = "ALL_PROXY all_proxy HTTP_PROXY http_proxy HTTPS_PROXY https_proxy NO_PROXY no_proxy";
/// Set in the environment low starts over with: the descriptor of the pipe that holds LOW_API_KEY and the proxies, or
/// "-".
const SCRUBBED: &str = "LOW_SCRUBBED";

static EXTRA_ENV: OnceLock<Vec<String>> = OnceLock::new();
static API_KEY: OnceLock<Option<String>> = OnceLock::new();
/// The proxy variables low started with, once it has started over without them; unset when it has not.
static PROXIES: OnceLock<Vec<(String, String)>> = OnceLock::new();

pub fn allow_env(names: Vec<String>) {
    let _ = EXTRA_ENV.set(names);
}

/// Whether a variable's name is on a list, where an entry ending in * is a prefix.
fn listed(list: &[&str], name: &str) -> bool {
    list.iter().any(|p| p.strip_suffix('*').map_or(*p == name, |prefix| name.starts_with(prefix)))
}

/// On macOS code running as the user can read the environment any of the user's processes started with, sandboxed or
/// not (sysctl KERN_PROCARGS2), unless the process is one of Apple's binaries, and no sandbox-exec rule stops it. The
/// model's code could find every secret exported in the shell that started low. So low starts over once, before
/// anything else, with only the variables it and the worker use (`extra` is config.toml's env, which stays readable),
/// and LOW_API_KEY and the proxies come along through an inherited pipe. Other processes stay readable: a Claude Code
/// that started low, for one. On Linux bwrap's --unshare-all gives the sandbox its own process namespace and /proc, so
/// no process outside it is visible.
pub fn scrub(extra: &[String]) {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    if let Ok(fd) = std::env::var(SCRUBBED) {
        let held = fd.parse::<i32>().ok().filter(|&fd| fd > 2).map(|fd| {
            // SAFETY: the read end of the pipe this process opened before it started over and left open for this, so
            // nothing else here owns it; reading it to the end and closing it is all that happens to it.
            let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut held = String::new();
            let _ = pipe.read_to_string(&mut held);
            serde_json::from_str::<Value>(&held).unwrap_or_default()
        });
        let held = held.unwrap_or_default();
        let _ = API_KEY.set(held["key"].as_str().filter(|k| !k.is_empty()).map(String::from));
        let proxies = held["proxies"].as_object().into_iter().flatten();
        let _ = PROXIES.set(proxies.filter_map(|(name, value)| Some((name.clone(), value.as_str()?.into()))).collect());
        return;
    }
    if !cfg!(target_os = "macos") {
        return;
    }
    let restart = || -> std::io::Result<std::io::Error> {
        let mut args = std::env::args_os();
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg0(args.next().unwrap_or_default()).args(args).env_clear();
        let kept: Vec<&str> =
            ENV.split_whitespace().chain(OWN_ENV.split_whitespace()).chain(extra.iter().map(String::as_str)).collect();
        cmd.envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(|name| listed(&kept, name))));
        let (mut fd, mut pipe) = ("-".to_string(), None);
        let key = std::env::var("LOW_API_KEY").ok().filter(|k| !k.is_empty());
        let proxies: serde_json::Map<String, Value> = PROXY_ENV
            .split_whitespace()
            .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?.into())))
            .collect();
        if key.is_some() || !proxies.is_empty() {
            let (reader, mut writer) = std::io::pipe()?;
            writer.write_all(json!({"key": key, "proxies": proxies}).to_string().as_bytes())?;
            drop(writer);
            rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::empty())?;
            fd = reader.as_raw_fd().to_string();
            pipe = Some(reader);
        }
        let failed = cmd.env(SCRUBBED, fd).exec();
        drop(pipe);
        Ok(failed)
    };
    let e = restart().unwrap_or_else(|e| e);
    eprintln!("low: could not start over without your environment's secrets ({e}); the model's code can read them");
}

/// Gives an HTTP client the proxies low started with, when it started over without them in its environment; otherwise
/// the client reads them from the environment itself, as it would, with the same names and order.
pub fn proxied(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(vars) = PROXIES.get() else {
        return builder;
    };
    let var = |names: &[&str]| {
        let set = |name: &&str| vars.iter().find(|(var, _)| var == name).map(|(_, value)| value.as_str());
        names.iter().find_map(set).filter(|value| !value.is_empty())
    };
    let no = var(&["NO_PROXY", "no_proxy"]).and_then(reqwest::NoProxy::from_string);
    let mut builder = builder.no_proxy();
    for (names, scheme) in [
        (["HTTPS_PROXY", "https_proxy"], "https"),
        (["HTTP_PROXY", "http_proxy"], "http"),
        (["ALL_PROXY", "all_proxy"], "all"),
    ] {
        let proxy = var(&names).map(|url| match scheme {
            "https" => reqwest::Proxy::https(url),
            "http" => reqwest::Proxy::http(url),
            _ => reqwest::Proxy::all(url),
        });
        if let Some(Ok(proxy)) = proxy {
            builder = builder.proxy(proxy.no_proxy(no.clone()));
        }
    }
    builder
}

/// The API key from LOW_API_KEY, which a low that started over has from its pipe.
pub fn api_key() -> Option<String> {
    API_KEY.get().cloned().flatten().or_else(|| std::env::var("LOW_API_KEY").ok())
}

static LOCKED: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Paths a repository's low.toml (`protect`) or `--protect` keeps the model's code and edits from changing.
pub fn protect(paths: Vec<PathBuf>) {
    let _ = LOCKED.set(paths);
}

fn locked() -> &'static [PathBuf] {
    LOCKED.get().map_or(&[], Vec::as_slice)
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
            .env("LOW_LOCKED", json!(locked().iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>()).to_string())
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
    let protected: Vec<PathBuf> = protected(root).into_iter().chain(locked().iter().cloned()).collect();
    let mut cmd = if yolo {
        Command::new(program)
    } else if cfg!(target_os = "macos") {
        let subpaths = |dirs: &[PathBuf]| -> String { dirs.iter().map(|d| format!(" (subpath {d:?})")).collect() };
        let (dirs, secrets, protected) = (subpaths(&writable), subpaths(secrets), subpaths(&protected));
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
    let allowed = |name: &str| listed(&allowed, name);
    cmd.args(args)
        .current_dir(root)
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(allowed)))
        .envs(HERMETIC.split_whitespace().filter_map(|pin| pin.split_once('=')))
        .envs(CACHES.split_whitespace().map(|name| (name, cache.as_os_str())))
        .kill_on_drop(true);
    cmd
}

/// The secret paths in the home directories, which the sandbox hides from the worker and the check. The sandbox matches
/// resolved paths, so each one also brings its real path and the targets of links inside it (a dotfiles repo's ~/.ssh
/// or ~/.aws/credentials). They are found once per process, since that walks each folder three levels deep.
fn secrets() -> &'static [PathBuf] {
    static FOUND: OnceLock<Vec<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(|| {
        let homes = homes();
        let mut paths = vec![];
        for path in homes.iter().flat_map(|home| SECRETS.split(',').map(|s| home.join(s.trim()))) {
            resolved(&path, &homes, &mut paths);
            linked(&path, &homes, 3, &mut paths);
            paths.push(path);
        }
        paths
    })
}

/// The home directory $HOME names, resolved; empty when HOME is unset.
pub fn home() -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    home.canonicalize().unwrap_or(home)
}

/// The home directories the sandbox guards: $HOME's and the account's, which differ when HOME names another folder,
/// such as the scratch HOME a test or another tool runs low with.
fn homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> =
        [home()].into_iter().chain(account_home()).filter(|h| !h.as_os_str().is_empty()).collect();
    homes.dedup();
    homes
}

/// The account's home directory from the user database, resolved.
fn account_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let uid = rustix::process::getuid().as_raw();
    let mut buf = vec![0; 4096];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes the entry into `entry` and its strings into `buf`, both alive and sized as passed,
        // and points `found` at `entry` only when it found the account.
        let status = unsafe { libc::getpwuid_r(uid, entry.as_mut_ptr(), buf.as_mut_ptr(), buf.len(), &mut found) };
        if status == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if status != 0 || found.is_null() {
            return None;
        }
        // SAFETY: `found` points at the entry getpwuid_r filled, whose pw_dir is null or a C string inside `buf`.
        let dir = unsafe { (*found).pw_dir };
        if dir.is_null() {
            return None;
        }
        // SAFETY: as above, a NUL-terminated string in `buf`, which outlives this borrow.
        let dir = PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(dir) }.to_bytes()));
        return Some(dir.canonicalize().unwrap_or(dir));
    }
}

/// Adds where `path` really is, if that differs and is a file or directory other than a home directory or one above
/// it: a history file linked to /dev/null must not take /dev/null from the worker.
fn resolved(path: &Path, homes: &[PathBuf], out: &mut Vec<PathBuf>) {
    if let Ok(real) = path.canonicalize()
        && real != path
        && real.metadata().is_ok_and(|m| m.is_file() || m.is_dir())
        && !homes.iter().any(|home| home.starts_with(&real))
    {
        out.push(real);
    }
}

/// Adds the targets of the links in `dir`, down to `depth` levels of real directories.
fn linked(dir: &Path, homes: &[PathBuf], depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 || !dir.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => resolved(&entry.path(), homes, out),
            Ok(kind) if kind.is_dir() => linked(&entry.path(), homes, depth - 1, out),
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

/// Whether the sandbox keeps `root` read-only: a home directory or a folder above it, where writes could reach shell
/// startup files and launch agents that later run outside the sandbox.
pub fn read_only(root: &Path) -> bool {
    homes().iter().any(|home| home.starts_with(root))
}

fn writable(root: &Path) -> Vec<PathBuf> {
    let root = Some(root.to_path_buf()).filter(|r| !read_only(r));
    let dirs = root.into_iter().chain([std::env::temp_dir(), "/tmp".into()]);
    dirs.filter_map(|d| d.canonicalize().ok()).collect()
}

/// The worker's interpreter: an active virtualenv's, so run() imports what the project installed there; else the newest
/// python3.N on PATH, else python3, which on macOS is the system's 3.9.
fn python() -> String {
    let venv = std::env::var_os("VIRTUAL_ENV").map(|dir| PathBuf::from(dir).join("bin"));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let found = venv
        .and_then(|bin| newest_python(std::iter::once(bin)))
        .or_else(|| newest_python(std::env::split_paths(&path)));
    found.map_or_else(|| "python3".into(), |p| p.to_string_lossy().into_owned())
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

    /// The account's home stays guarded whatever HOME names: read-only along with the folders above it, and its
    /// secret paths hidden. Paths only; nothing in the home directory is read.
    #[test]
    fn account_home_guarded() {
        let Some(home) = super::account_home() else { return };
        assert!(super::read_only(&home) && super::read_only(std::path::Path::new("/")));
        assert!(!super::read_only(&home.join("projects")));
        assert!(super::secret(&home.join(".ssh/config")) && !super::secret(&home.join("projects")));
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
            // A failed edit says where old is, or what is nearest to it.
            (
                "write('n.py', 'def g():\\n    x = 1\\n    return x\\n\\nx = 1\\ns = \"a\"\\n')\n\
                 edit('n.py', 'def g():\\n  x = 1', 'pass')",
                "[n.py: nearest to old is lines 1-2, it differs only in whitespace]\n1: def g():\n2:     x = 1\n",
            ),
            ("edit('n.py', 'x = 1', 'x = 2')", "[n.py: old occurs at lines 2, 5; add context so it matches once]"),
            (
                "edit('n.py', 'def g():\\n    x = 2\\n    return x', 'pass')",
                "[n.py: nearest to old is lines 1-3, 2 of 3 lines alike]\n1: def g():\n2:     x = 1\n3:     return x\n",
            ),
            ("edit('n.py', 'retrun x', 'y')", "[n.py: nearest to old is line 3, the closest match]\n3:     return x\n"),
            (
                "edit('n.py', r's = \\\"a\\\"', 'y')",
                "[n.py: old matches once without the backslashes before its quotes",
            ),
            ("edit('n.py', 'zzzz', 'y')", "[n.py: no line of old is in the file; show() it before editing]"),
            (
                "edit('a.py', ['return 2', 'return 3'])",
                "edit() takes (path, old, new) or (path, [(old, new), ...]) with string old and new",
            ),
            ("total = 1\nratio(total, 0)", "    return a / b"),
            ("show = 1", "[show: low's helpers, restored before the next run]"),
            ("show('a.py', 10)", "[a.py: 2 lines; nothing at line 10]"),
            ("write('a\\u202eb.txt', 'x')", "has invisible or bidi characters in its name"),
            ("print(sh('rm -rf ./* ./.[!.]*'))", "[refused: rm -r ./* deletes the files git ignores"),
            ("print(sh('cd . && rm -fr .'))", "[refused: rm -r . deletes the files git ignores"),
            ("print(sh('rm -rf build* && echo kept'))", "kept"),
            ("import subprocess\nsubprocess.Popen(['sleep', '60'])", "[stopped 1 process this run left running"),
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
        assert!(!dir.join("a\u{202e}b.txt").exists());
        // The process a run leaves running is gone by the next run.
        let reply = run(&mut worker, "import subprocess\nlive = subprocess.Popen(['sleep', '60'])").await;
        assert!(reply["out"].as_str().is_some_and(|out| out.contains("[stopped 1 process")), "{reply}");
        let gone = "import os\ntry:\n    os.kill(live.pid, 0)\nexcept ProcessLookupError:\n    print('gone')";
        assert_eq!(run(&mut worker, gone).await["out"], "gone\n");
        let reply = run(&mut worker, "edit('a.py', 'return 4', 'return 5')\nassert check()\nfinish('done')").await;
        assert_eq!((&reply["finish"], &reply["own"], &reply["check"]), (&json!("done"), &json!(0), &json!(true)));
        assert_eq!(run(&mut worker, "print('x')\nfinish('done')").await["own"], 2);
        // write() replaces a regular file, keeping CRLF line ends, and refuses anything else that exists.
        let reply = run(&mut worker, "write('a.py', 'x = 1\\n')").await;
        assert_eq!((&reply["out"], &reply["actions"][0]["replaced"]), (&json!("write a.py: ok\n"), &json!(true)));
        std::fs::write(dir.join("w.txt"), "one\r\ntwo\r\n").unwrap();
        run(&mut worker, "write('w.txt', 'three\\n')").await;
        assert_eq!(std::fs::read_to_string(dir.join("w.txt")).unwrap(), "three\r\n");
        std::fs::create_dir(dir.join("d")).unwrap();
        let reply = run(&mut worker, "write('d', 'x')").await;
        assert!(reply["out"].as_str().is_some_and(|out| out.contains("d exists and is not a regular file")), "{reply}");
        drop(worker);
        // With no check configured, check() runs nothing and passes, so `assert check()` doesn't end the run, and its
        // line is the helper's own, not output finish() waits for the model to read.
        let mut worker = super::Worker::start(&dir, true, None, None).unwrap();
        let reply = run(&mut worker, "assert check()\nfinish('done')").await;
        assert_eq!((&reply["out"], &reply["own"]), (&json!("[no check configured: check() ran nothing]\n"), &json!(0)));
        assert_eq!((&reply["check"], &reply["error"]), (&Value::Null, &json!(false)));
        drop(worker);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
