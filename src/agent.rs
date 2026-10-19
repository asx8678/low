use crate::detect::Detected;
use crate::fetch::{self, Fetched};
use crate::git::{changes, git, guard_git, snapshot};
use crate::llm::{self, Llm};
use crate::providers::Providers;
use crate::redact::redact;
use crate::session::{Session, prompt_tokens};
use crate::worker::{self, Worker};
use crate::{Config, compact, ui};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

const COMPACT_AT: u64 = 100_000;
/// Seconds an idle --serve session waits before re-sending its prompt to keep it cached; keep_warm in a model's
/// profile overrides it. OpenCode Go, Zro and open design all still had a 10k-token prompt cached after 20 untouched
/// minutes (2026-10-17), so ten minutes keeps a margin.
const WARM_EVERY: u64 = 600;
const MAX_REQUESTS: usize = 200;
const FILES: usize = 200;
const LISTING: usize = 8 * 1024;
const TREE: usize = 10 * 1024;
const AGENTS_MAX: usize = 32 * 1024;
const RESTARTS: usize = 2;
const CHECK_TIMEOUT: Duration = Duration::from_mins(30);
const CHECK_OUT: u64 = 1 << 20;
/// A turn's budget in compact_at units (turn_budget in a model's profile), counting cached input at a quarter.
const TURN_BUDGET: u64 = 60;
/// Runs in a row with the same code and the same result that end a turn: the model is stuck.
const REPEATS: usize = 5;
/// How long a session log stays a candidate for another low working in this repository.
const DAY: Duration = Duration::from_secs(86_400);
/// Margin around a run's recorded start and end when matching file times to it.
const SLACK_MS: u64 = 250;
/// Files a report row names before it says how many more: a port turn changed 12,725.
const LISTED: usize = 10;
const GATE: &str = "The project check failed on the current tree. Fix what fails, run check(), then finish again. \
                    If it cannot pass, say why in one line. The check output ended with:";
const RETRY: &str = "Send one run() now with the next step, or a shorter answer.";
/// Told to the model once a turn, when it has spent three quarters of its budget (turn_budget's tokens or
/// MAX_REQUESTS). It asks for focus, never for the turn to end: in a repository with no check an early reply is done,
/// and a --tasks task would lose the turns a limit gives one that is still changing files. In the session logs
/// (2026-10-19), 81 of 709 real turns hit the token budget, at a median of 101 runs, most of them still reading files.
const WARN: &str = "This turn has used three quarters of its budget";
const WARN_END: &str = "Spend them on the change itself: read only what the next edit needs, put several steps in each \
                        run, and verify what you changed.";
/// Asked once a turn-scoped limit stops a turn, so its last reply says where the work stands: the next turn, a --tasks
/// continuation or the user's, starts from it instead of from what the cut left.
const HANDOFF: &str = "This turn has reached its limit, so low runs no more code in it. Reply now in prose, without \
                       run(): what you changed and how far you verified it, what is left to do, and the next step, so \
                       the next turn can pick up from here.";
const NOT_RUN: &str = "not run: this turn had reached its limit";
const LOOPED: &str = "stopped: the model's reasoning kept repeating itself, the third unusable reply in a row";
/// The files a repository gives the model its instructions in, the first that can be read going in the system prompt:
/// CLAUDE.md is what Claude Code reads, for a repository that has no AGENTS.md.
const INSTRUCTIONS: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
const STRAY: &str = "This turn changed files without edit() or write():";
const STRAY_END: &str = "Delete the ones that are scratch output from running code, keep any that are part of your answer, \
                         then finish again.";
const PLANTED: &str = "This turn made a git repository inside the tree:";
const PLANTED_END: &str = "The user's git would run hooks and config from there, outside the sandbox. Delete it if it is \
                           scratch, or say in your final message why it belongs, then finish again.";
/// A second look's instructions, after the turn's own system prompt, which the provider still has cached. The e2e mock
/// (tests/e2e/mock.py) tells a second look's requests by its opening words.
const LOOK: &str = "You are taking a second look at a turn low just finished: another conversation did the task below, \
                    and you check its final report against the code. You get the task, its criteria, what its runs \
                    edited and ran, its diff and its report, not that conversation. Nothing here changes files: edit() \
                    and write() refuse, the repository is read-only, and check(), search() and fetch() are off.\n\n\
                    Pick the 3 to 5 claims in the report that matter most and are likeliest to be wrong: what the change \
                    does, a case it handles, something it leaves unchanged, a command said to pass. Check each by \
                    reading the code with show(), open(), re and sh(\"git grep ...\"), in as few runs as you can; don't \
                    build, test or run the project.\n\n\
                    Then end with finish(), which is always taken here, and one line per claim:\n\
                    held: <claim> — <file:line that shows it>\n\
                    contradicted: <claim> — <file:line, and what is there instead>\n\
                    uncertain: <claim> — <why reading can't settle it>\n\
                    Call a claim contradicted only when the code shows it false. A command said to pass holds when it \
                    passed in a run later than the last edit to what it runs, and is uncertain otherwise.";
/// The requests one second look may make before it settles nothing.
const LOOK_REQUESTS: usize = 6;
/// The diff a second look gets; it reads the files for the rest.
const LOOK_DIFF: usize = 24 * 1024;
/// The turn's runs a second look is told about: the last ones.
const LOOK_RUNS: usize = 30;
/// What a second look's check(), search() and fetch() get.
const LOOK_OFF: &str = "not available in a second look: read the code instead";
const REFUTED: &str = "A second look at your report, by a reader that had only the task, the criteria, the diff and the \
                       report, found claims the code contradicts:";
const REFUTED_END: &str = "Fix the code or correct the report, then finish again. If the reader is wrong, say why in one \
                           line.";
/// DeepSeek's own tool-call markup, which leaks into a run()'s code at times: what follows it is no Python.
const MARKUP: &[&str] = &["<｜end▁of▁thinking｜>", "｜DSML｜"];
const HELPERS: &[&str] = &[
    "sh", "show", "accept", "edit", "write", "undo", "check", "plan", "done", "finish", "ask", "search", "fetch",
    "recall",
];
/// The undo() line, in the prompt only inside a git repository, where a turn has a snapshot to restore from.
const UNDO: &str = "undo(*paths)                  # put files back as they were when this turn began (default: all this \
                    turn changed); files write() created are removed\n";
/// The helpers that reach the web, in the prompt only when config.toml's search names a provider.
const WEB: &str = "search(query, count=10) -> list  # web search: [{title, url, summary, date, site_name}]; site:, \"phrase\", \
                   after:2025 work; query up to 128 chars, count up to 50\nfetch(url) -> str             # a web page \
                   as markdown; JSON, code and other text files as their text, as served on the open hosts named here and \
                   less some blank lines elsewhere; a URL a search() returned or the task gives, or any URL on \
                   registry.npmjs.org, pypi.org, crates.io, docs.rs, api.github.com, raw.githubusercontent.com, \
                   cdn.jsdelivr.net, data.jsdelivr.com, unpkg.com, cdnjs.cloudflare.com, doc.rust-lang.org, \
                   docs.python.org, developer.mozilla.org\nsearch([query, ...]), fetch([url, ...]) -> list  # up to \
                   50 at once, 10 at a time: the answers in order, a RuntimeError in place of each that failed\n";
/// WEB's fetch() without a search provider: low reads the open hosts itself, and nothing else.
const WEB_OPEN: &str = "fetch(url) -> str             # a URL's text as its host serves it: JSON, code and other text \
                        files, not HTML pages; any URL on HOSTS\nfetch([url, ...]) -> list  # up to 50 at once, 10 \
                        at a time: the answers in order, a RuntimeError in place of each that failed\n";
/// How many requests in a run's list of searches or fetches go out at once: DeepSeek takes fetches at 25 a second.
const WEB_AT_ONCE: usize = 10;
/// The most requests one list may hold.
const WEB_LIST: usize = 50;
/// What a request the tools API would make gets without a search provider: a search(), and a fetch() off the open hosts.
const UNSET: [&str; 2] = [
    "web search is not set up: config.toml's search names no provider",
    "without a search provider in config.toml, fetch() reads only URLs on the open hosts its help lists",
];
/// Hosts where fetch() takes any URL, not only one a search() returned: package registries, CDNs and docs that show no
/// one the URLs asked of them, so a URL a run made up there can't carry what it read to someone who would see it. The
/// nearest thing is jsDelivr's public hit counts per file, which tell a package's author which of its files was fetched.
/// Not github.com, whose traffic insights show a repository's owner the paths visited; api.github.com serves the same.
const OPEN: &[&str] = &[
    "registry.npmjs.org",
    "pypi.org",
    "crates.io",
    "docs.rs",
    "api.github.com",
    "raw.githubusercontent.com",
    "cdn.jsdelivr.net",
    "data.jsdelivr.com",
    "unpkg.com",
    "cdnjs.cloudflare.com",
    "doc.rust-lang.org",
    "docs.python.org",
    "developer.mozilla.org",
];
const REGISTRIES: &str = " Package managers can't reach their registries; fetch() reads them: \
                          json.loads(fetch(\"https://registry.npmjs.org/three/latest\"))[\"version\"] is three's latest \
                          version.";
const SANDBOX: &str = "\n\n## Sandbox\nYour code runs in a sandbox. Writes outside the repo and temp dirs fail, as do \
                       writes into any .git directory and reads of credentials and private data in the home directory \
                       (ssh and cloud keys, shell history, browser data, other tools' configs). Only a fixed set of \
                       environment variables is passed through (env = [\"NAME\"] in ~/.config/low/config.toml adds more).";
const MACOS: &str = " Internet and DNS are off (localhost works); nested sandbox-exec fails, and Unix sockets work only \
                     under the repo's tmp/ directory.";
const LINUX: &str = " The network is fully isolated: Internet, DNS and the host's localhost services are off; only \
                     servers you start inside the sandbox are reachable on localhost.";
const PERMANENT: &str = " These failures (Operation not permitted, EPERM, Read-only file system, DNS lookup errors) are \
                         permanent. Don't retry them and don't search the machine for another copy of what was \
                         blocked: skip that step, finish the rest, and say in your final message what you could not run.";

pub struct Agent {
    root: PathBuf,
    config: Config,
    yolo: bool,
    llm: Llm,
    provider: String,
    session: Session,
    worker: Option<Worker>,
    compact_at: u64,
    pending: Option<Value>,
    current: Option<(usize, Instant)>,
    discarded: Option<(usize, String)>,
    restarts: usize,
    broken: Option<String>,
    passed: Option<(String, String)>,
    /// The pass of a check detected at the session's start, on the tree as found: the first turn starts with it, so a
    /// check() before anything changes doesn't run it again.
    baseline: Option<(String, String)>,
    /// Whether the root is a git repository; outside one there are no snapshots and no undo.
    repo: bool,
    /// The last turn's result: its verdict, report rows and final answer.
    last: Option<Outcome>,
    /// The question an ask() ended the turn with, for its report.
    question: Option<String>,
    /// What the turn's second look made of its report's claims, for its report.
    claims: Option<Claims>,
    /// A file low itself rewrites between turns (a --tasks checklist), relative to the root: not news to the model.
    quiet: Option<String>,
    /// The limit that stopped the current turn, for its result: a --tasks run continues such a task.
    limit: Option<String>,
}

impl Agent {
    pub fn new(
        root: &Path,
        home: &Path,
        mut config: Config,
        resume: bool,
        id: Option<&str>,
        yolo: bool,
        repo: bool,
    ) -> Result<Self> {
        guard_git(root);
        let session = if resume || id.is_some() {
            Session::resume(home, root, id, |header| {
                adopt(&mut config, header);
                context(root, &config, yolo)
            })?
        } else {
            let model = config
                .model
                .clone()
                .or_else(|| config.providers.values().find_map(|p| p.models.first().cloned()))
                .context(
                    "no model: pass -m, set model in ~/.config/low/config.toml, or list one under a provider in \
                     ~/.config/low/providers.toml",
                )?;
            let (named, model) = match qualified(&config.providers, &model) {
                Some((provider, id)) => (Some(provider), id.to_string()),
                None => (None, model.clone()),
            };
            let only = config.providers.keys().next().filter(|_| config.providers.len() == 1);
            let Some(provider) = named.as_ref().or(holding(&config.providers, &model)).or(only) else {
                if config.providers.is_empty() {
                    anyhow::bail!("no provider for {model}: {}", crate::providers::example(&model));
                }
                let names: Vec<&str> = config.providers.keys().map(String::as_str).collect();
                anyhow::bail!(
                    "no provider lists {model}: list it under one of [{}] in ~/.config/low/providers.toml",
                    names.join(", ")
                );
            };
            web(provider, &config.providers[provider].base_url)?;
            let profile = config.models.get(&model).cloned().unwrap_or_default();
            let system = system_prompt(root, yolo, &config, repo, config.search.is_some());
            Session::create(home, root, provider, &model, &system, profile, context(root, &config, yolo))?
        };
        let baseline = config.detected.as_ref().and_then(|d| d.passed.clone());
        let (provider, llm) = connect(&config.providers, &session)?;
        let mut since = session.entries.iter().rev().take_while(|e| e["switch"].is_null());
        let learned = since.find_map(|e| e["compact"]["at"].as_u64());
        let compact_at = learned.or(session.header()["compact_at"].as_u64()).unwrap_or(COMPACT_AT);
        let mut agent = Self {
            root: root.into(),
            config,
            yolo,
            llm,
            provider,
            session,
            worker: None,
            compact_at,
            pending: None,
            current: None,
            discarded: None,
            restarts: 0,
            broken: None,
            passed: None,
            baseline,
            repo,
            last: None,
            question: None,
            claims: None,
            quiet: None,
            limit: None,
        };
        agent.record()?;
        Ok(agent)
    }

    fn record(&mut self) -> Result<()> {
        if self.session.header()["provider"].as_str().is_some_and(|p| p != self.provider) {
            let (provider, model) = (self.provider.clone(), self.model().to_string());
            self.switch(&provider, &model)?;
        }
        Ok(())
    }

    pub fn resume_with(&mut self, model: &str) -> Result<()> {
        let (provider, model) = self.resolve(model);
        if provider == self.provider && model == self.model() {
            return Ok(());
        }
        self.switch(&provider, &model)
    }

    /// The provider and model id a typed model names: `provider:model`, else the provider that lists it, else the
    /// current one.
    pub fn resolve(&self, model: &str) -> (String, String) {
        match qualified(&self.config.providers, model) {
            Some((provider, id)) => (provider, id.to_string()),
            None => {
                let provider = holding(&self.config.providers, model).cloned();
                (provider.unwrap_or_else(|| self.provider.clone()), model.to_string())
            }
        }
    }

    /// The model's profile in config.toml, if it has one.
    pub fn profile(&self, model: &str) -> Option<&serde_json::Map<String, Value>> {
        self.config.models.get(model)
    }

    /// Runs one task and returns its result as `low --json`, `low --send` and `--tasks` give it.
    pub async fn task(&mut self, task: &str) -> Outcome {
        match self.run(task).await {
            Ok(_) => {
                self.last.clone().unwrap_or_else(|| failed(None, &anyhow::anyhow!("stopped before the turn began")))
            }
            Err(e) => failed(self.last.as_ref(), &e),
        }
    }

    /// How often an idle --serve session re-sends its prompt: keep_warm seconds from the model's profile, where 0 turns
    /// it off, else WARM_EVERY.
    pub fn keep_warm(&self) -> Option<Duration> {
        let every = self.session.header()["keep_warm"].as_u64().unwrap_or(WARM_EVERY);
        (every > 0).then(|| Duration::from_secs(every))
    }

    /// Re-sends the conversation so far, cut at one output token, so the provider keeps it cached for the next task.
    /// Nothing is sent before the first reply or right after a compaction. The request is logged as a warm event with
    /// its usage, so /cost counts it.
    pub async fn warm(&mut self) -> Result<()> {
        if self.session.last_prompt_tokens() == 0 {
            return Ok(());
        }
        let meta = self.llm.warm(self.session.messages(), &tools()).await?;
        let (prompt, cached) =
            (meta["usage"]["prompt_tokens"].as_u64().unwrap_or(0), ui::cached(&meta["usage"]).unwrap_or(0));
        ui::record(
            "warm",
            &format!("kept the prompt cache warm: {cached} of {prompt} tokens cached"),
            json!({"meta": meta}),
        );
        self.session.flush()
    }

    pub fn path(&self) -> &Path {
        &self.session.path
    }

    pub fn model(&self) -> &str {
        self.session.header()["model"].as_str().unwrap_or_default()
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn set_providers(&mut self, providers: Providers) -> Result<()> {
        self.config.providers = providers;
        match connect(&self.config.providers, &self.session) {
            Ok((provider, llm)) => (self.provider, self.llm) = (provider, llm),
            Err(e) => {
                ui::note(&format!("{e:#}; still connected as before until a provider is added"));
                return Ok(());
            }
        }
        self.record()
    }

    pub async fn models(&self) -> Vec<(String, String, bool)> {
        let mut known: Vec<(String, String)> = self
            .config
            .providers
            .iter()
            .flat_map(|(name, p)| p.models.iter().map(move |m| (name.clone(), m.clone())))
            .collect();
        // A provider that lists its models shows just those; /providers lists the rest of what it serves.
        if self.config.providers.get(&self.provider).is_none_or(|p| p.models.is_empty()) {
            match self.llm.models().await {
                Ok(listed) => known.extend(listed.into_iter().map(|m| (self.provider.clone(), m))),
                Err(e) => ui::note(&format!("could not list {}'s models: {e:#}", self.provider)),
            }
        }
        let mut seen = HashSet::new();
        known.retain(|pair| seen.insert(pair.clone()));
        known
            .into_iter()
            .map(|(p, m)| {
                let profiled = self.config.models.contains_key(&m);
                (p, m, profiled)
            })
            .collect()
    }

    /// The reasoning effort the session's requests carry, if they carry one.
    pub fn effort(&self) -> Option<&str> {
        self.session.header()["params"]["reasoning_effort"].as_str()
    }

    /// Sends reasoning_effort = `effort` with the session's requests from now on: the same model, logged as a switch
    /// so a resumed session keeps it. Switching the model with /model goes back to its profile.
    pub fn set_effort(&mut self, effort: &str) -> Result<()> {
        anyhow::ensure!(EFFORTS.contains(&effort), "the effort is one of: {}", EFFORTS.join(", "));
        let header = self.session.header();
        let (model, system) = (self.model().to_string(), header["system"].as_str().unwrap_or_default().to_string());
        let mut profile = header["params"].as_object().cloned().unwrap_or_default();
        for key in ["compact_at", "keep_warm", "turn_budget", "session_budget", "fallback"] {
            if !header[key].is_null() {
                profile.insert(key.into(), header[key].clone());
            }
        }
        profile.insert("reasoning_effort".into(), effort.into());
        // A profile that sends no temperature (temperature = false) keeps sending none.
        profile.entry("temperature").or_insert(false.into());
        let provider = self.provider.clone();
        self.session.switch(&provider, &model, &system, profile)?;
        (self.provider, self.llm) = connect(&self.config.providers, &self.session)?;
        let replied = self.session.entries.iter().any(|e| e["role"] == "assistant");
        let uncached = if replied { "; DeepSeek caches per level, so its next request is uncached" } else { "" };
        ui::note(&format!("reasoning effort {effort} for {model}{uncached}"));
        Ok(())
    }

    pub fn switch(&mut self, provider: &str, model: &str) -> Result<()> {
        anyhow::ensure!(self.config.providers.contains_key(provider), "no provider named {provider}");
        web(provider, &self.config.providers[provider].base_url)?;
        let profile = self.config.models.get(model).cloned().unwrap_or_default();
        let web = self.config.search.is_some();
        let system = system_prompt(&self.root, self.yolo, &self.config, self.repo, web);
        self.session.switch(provider, model, &system, profile)?;
        (self.provider, self.llm) = connect(&self.config.providers, &self.session)?;
        self.compact_at = self.session.header()["compact_at"].as_u64().unwrap_or(COMPACT_AT);
        ui::note(&format!("now using {model} on {provider}"));
        Ok(())
    }

    pub async fn run(&mut self, task: &str) -> Result<bool> {
        self.last = None;
        let bar = ui::Turn::start();
        let finished = tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => None,
            result = self.turn(task) => Some(result),
        };
        ui::settle();
        if let Some(result) = finished {
            if let Err(e) = &result {
                let text = error_text(e);
                self.session.push(json!({"event": "error", "text": text, "at": ui::now_ms()}))?;
                if let Err(e) = self.unfinished(true).await {
                    ui::note(&format!("could not write the turn report: {e:#}"));
                }
            }
            return result;
        }
        let settled = match self.worker.as_mut() {
            Some(worker) => tokio::select! {
                settled = worker.settle() => settled,
                _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("a second Ctrl-C stopped it")),
            },
            None => Ok(None),
        };
        match settled {
            Ok(Some(reply)) => {
                let (out, mut meta) = finish(reply);
                meta["interrupted"] = true.into();
                ui::ran(&meta, &out);
                if let Some(id) = self.pending.take() {
                    let content = format!("{out}\n[interrupted by the user]");
                    self.session.push(json!({"role": "tool", "tool_call_id": id, "content": content, "meta": meta}))?;
                }
            }
            Ok(None) => {}
            Err(e) => {
                self.worker = None;
                if let Some(id) = self.pending.take() {
                    let content = format!(
                        "[interrupted by the user] the run did not stop ({e:#}); the worker was restarted and its \
                         variables are gone"
                    );
                    let meta = json!({"tool": "run", "error": true, "interrupted": true});
                    self.session.push(json!({"role": "tool", "tool_call_id": id, "content": content, "meta": meta}))?;
                }
            }
        }
        ui::event("ctrl_c", "interrupted", Value::Null);
        let unfinished = self.unfinished(false).await;
        drop(bar);
        ui::unsend();
        unfinished.map(|()| false)
    }

    /// Ends a turn that an error or a Ctrl-C cut short with its report. An error before the first request gets none: no
    /// run could have changed a file, and a verdict would only dress up the error.
    async fn unfinished(&mut self, errored: bool) -> Result<()> {
        let requests = |began: usize| crate::stats::totals(&self.session.entries[began..])["requests"].as_u64();
        match self.current.take() {
            Some((began, _)) if errored && requests(began).unwrap_or(0) == 0 => self.session.flush(),
            Some((began, clock)) => self.report(None, false, true, began, clock).await.map(drop),
            None => self.session.flush(),
        }
    }

    async fn turn(&mut self, task: &str) -> Result<bool> {
        let baseline = self.baseline.take();
        (self.pending, self.restarts, self.broken, self.passed, self.claims) = (None, 0, None, baseline, None);
        self.session.flush()?;
        let clock = Instant::now();
        let why = "low's change detection (a private git index, not your staging area) could not snapshot the tree";
        let start = self.tree().await.context(why)?;
        let snapshot_ms = ui::ms(clock);
        // So the undo the report prints outlives git gc.
        let (root, tree) = (self.root.clone(), start.clone());
        tokio::task::spawn_blocking(move || crate::git::keep(&root, &tree)).await?;
        let mut meta = json!({"snapshot": start, "snapshot_ms": snapshot_ms});
        if let Some(date) = today() {
            meta["date"] = date.into();
        }
        self.session.push(json!({"role": "user", "content": task, "meta": meta}))?;
        // The push first writes what the snapshot logged, so the turn begins at the task itself.
        let began = self.session.entries.len() - 1;
        self.current = Some((began, clock));
        // A skill the task asks for by name comes with it: the model needs it, and reading it would take a request.
        for skill in crate::skills::named(task, &self.root) {
            let Some((text, size)) = file_text(&skill.real, AGENTS_MAX) else { continue };
            let (name, path) = (&skill.name, &skill.path);
            let mut content = format!(
                "[The task asks for the {name} skill. Its SKILL.md ({path}) follows; do as it says, and find the files it \
                 names in its folder.]\n\n{}",
                text.trim_end()
            );
            if size > AGENTS_MAX as u64 {
                let (kept, all) = (AGENTS_MAX / 1024, size.div_ceil(1024));
                content +=
                    &format!("\n\n[SKILL.md clipped to its first {kept} KB of {all} KB; read the file for the rest]");
            }
            ui::event("skill", &format!("the {name} skill goes with the task: {path}"), Value::Null);
            self.session.push(json!({"role": "user", "content": content, "meta": {"host": true, "skill": name}}))?;
        }
        if let Some(files) = self.moved_since(&start).await {
            let more = files.len().saturating_sub(20);
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            let shown = files[..files.len().min(20)].join(", ");
            let moved = format!("Files changed since your last turn, outside this conversation: {shown}{more}");
            // The screen gets one line of it, the log and the model all of it.
            let s = if files.len() == 1 { "" } else { "s" };
            let line =
                format!("{} file{s} changed since the last turn, outside this conversation: {shown}", files.len());
            ui::show(&ui::line(&line));
            ui::record("moved", &moved, Value::Null);
            self.session.push(json!({"role": "user", "content": format!("[{moved}]"), "meta": {"host": true}}))?;
        }
        let mut state = TurnState::default();
        let budget = self.session.header()["turn_budget"].as_u64().unwrap_or(TURN_BUDGET);
        let ceiling = self.compact_at * budget;
        let rule = "cached input counts a quarter";
        let mut limit = format!("stopped after {MAX_REQUESTS} model requests in one turn");
        // Whether the limit that stops the turn is the turn's own, which the model is asked to hand off from. Not the
        // session's budget, a cap on spending, nor a model that is stuck or keeps sending unusable replies.
        let mut handoff = true;
        for made in 1..=MAX_REQUESTS {
            if state.spent >= ceiling {
                limit =
                    format!("stopped after {} in one turn ({budget} × compact_at; {rule})", ui::tokens(state.spent));
                break;
            }
            if let Some(total) = self.over_budget() {
                let spent = ui::tokens(self.spent());
                limit = format!("stopped: the session has spent {spent} (session_budget {total} × compact_at; {rule})");
                handoff = false;
                break;
            }
            // What the user sent from the REPL during the turn goes in between requests, where every run has its result.
            if let Some(steer) = ui::steers() {
                ui::steered(&steer);
                state.repeats = 0;
                self.session.push(json!({"role": "user", "content": steer, "meta": {"steer": true}}))?;
            }
            // Twice what the last compaction left, too: a compact_at that the summary and the kept tail fill on their
            // own would compact before every task, each time a cache miss.
            if self.upcoming() >= self.compact_at.max(2 * self.compacted()) {
                let done = self.compact_to(self.compact_at as usize / 5, false)?;
                ui::record("note", &done, Value::Null);
            }
            let before = state.spent;
            let Some(message) = self.request(&mut state).await? else {
                if state.stalled {
                    (limit, handoff) = (LOOPED.into(), false);
                    break;
                }
                continue;
            };
            let cost = state.spent - before;
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            self.session.push(message)?;
            let (finished, asked) = self.act(&mut state, began, &calls).await?;
            // The model waits for an answer, which comes as the next task: no nudges or check gate before it.
            if let Some(question) = asked {
                self.reply(&question, "ask()")?;
                self.question = Some(question);
                let changed = self.changed(&start, began).await;
                self.current = None;
                return self.report(Some(changed), false, false, began, clock).await;
            }
            if finished.is_none() && state.repeats >= REPEATS {
                limit = format!("stopped: the model ran the same code to the same result {REPEATS} times in a row");
                handoff = false;
                break;
            }
            if let Some(report) = finished {
                self.reply(&report, "finish()")?;
            } else if !calls.is_empty() {
                self.warn(&mut state, ceiling, made, cost)?;
                ui::activity("waiting for the model");
                continue;
            }
            let Some((changed, checked)) = self.gates(&mut state, &start, began).await? else { continue };
            self.current = None;
            return self.report(Some(changed), checked, state.stalled && !changed, began, clock).await;
        }
        ui::event("limit", &limit, Value::Null);
        // The limit is set first, so a Ctrl-C during the handoff still reports it.
        self.limit = Some(limit);
        if handoff {
            self.handoff(&mut state).await?;
        }
        self.current = None;
        let changed = self.changed(&start, began).await;
        self.report(Some(changed), false, true, began, clock).await
    }

    /// Asks the model for its next reply. What can be retried is handled here: a context overflow (compact and ask
    /// again), a provider out of credit (go on with the profile's fallback, once a turn), a lost stream, reasoning cut
    /// off in a loop, and an empty or cut-off reply (ask again, twice in a row at most). None when the turn asks again,
    /// or when a third loop in a row stalls it; otherwise the reply, counted against the turn's budget.
    async fn request(&mut self, state: &mut TurnState) -> Result<Option<Value>> {
        let result = self.llm.complete(self.session.messages(), &tools()).await;
        if let Some(lost) = result.as_ref().err().and_then(llm::lost) {
            self.discard("the provider cut the stream", &lost.thinking);
        }
        if let Some(llm::Looping(lost)) = result.as_ref().err().and_then(|e| e.downcast_ref()) {
            self.discard("the reasoning kept repeating itself", &lost.thinking);
        }
        let mut message = match result {
            Err(e) if e.is::<llm::ContextOverflow>() && !state.overflowed => {
                state.overflowed = true;
                self.overflow(e.downcast_ref::<llm::ContextOverflow>().and_then(|o| o.window))?;
                return Ok(None);
            }
            Err(e) if e.is::<llm::ContextOverflow>() => {
                let system = self.session.header()["system"].as_str().map_or(0, str::len);
                let found = INSTRUCTIONS.into_iter().find_map(|n| repo_text(&self.root, n, 0).map(|(_, s)| (n, s)));
                let (name, size) = found.map_or((INSTRUCTIONS[0], 0), |(n, s)| (n, s as usize));
                anyhow::bail!(
                    "{e}; still over after compacting: the request is {} KB, of which the system prompt is {} KB \
                     (including {} KB of {name})",
                    self.sent() * 4 / 1024,
                    system / 1024,
                    size.min(AGENTS_MAX) / 1024
                );
            }
            // Out of credit or quota on this provider: once a turn, go on with the model profile's fallback.
            Err(e) if e.is::<llm::Exhausted>() && !state.failed_over && self.fallback().is_some() => {
                state.failed_over = true;
                let (provider, model) = self.fallback().unwrap_or_default();
                ui::event("failover", &format!("{e:#}; going on with {provider}:{model}"), Value::Null);
                self.switch(&provider, &model)?;
                return Ok(None);
            }
            Err(e) if state.retries < 2 && llm::lost(&e).is_some_and(|l| !l.stalled) => {
                (state.spent, state.retries) = (state.spent + self.sent(), state.retries + 1);
                self.retry("was lost: the provider cut the stream")?;
                return Ok(None);
            }
            // Its usage never came, so the event carries an estimate for /cost: what was sent, cached as far as the last
            // request went, and the reasoning's bytes / 4 out. Like an empty reply it is dropped and the model asked
            // to act.
            Err(ref e) if let Some(llm::Looping(lost)) = e.downcast_ref() => {
                let (sent, out) = (self.sent(), lost.thinking.len() as u64 / 4);
                let cached = self.session.last_prompt_tokens().min(sent);
                let usage = json!({"prompt_tokens": sent, "prompt_tokens_details": {"cached_tokens": cached},
                                   "completion_tokens": out, "estimated": true});
                state.spent += weighted(&json!({"usage": usage}));
                ui::usage(&usage);
                state.stalled = state.retries >= 2;
                let then = if state.stalled { "" } else { "; asking it to act" };
                let kb = lost.thinking.len() / 1024;
                let text = format!("{e}; cut it off at {kb} KB (~{}){then}", ui::tokens(out));
                let mut extra = lost.extra();
                extra["meta"] = json!({"usage": usage});
                ui::event("loop", &text, extra);
                if !state.stalled {
                    state.retries += 1;
                    self.retry("was cut off: its reasoning kept repeating itself")?;
                }
                return Ok(None);
            }
            result => result?,
        };
        state.overflowed = false;
        if !message["meta"]["usage"]["prompt_tokens"].is_u64() {
            let usage = &mut message["meta"]["usage"];
            usage["prompt_tokens"] = self.sent().into();
            usage["estimated"] = true.into();
        }
        state.spent += weighted(&message["meta"]);
        let usage = &message["meta"]["usage"];
        ui::usage(usage);
        self.cache_miss(usage);
        let calls = message["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
        let empty = !calls && message["content"].as_str().is_none_or(|c| c.trim().is_empty());
        let cut = !calls && !empty && message["meta"]["finish"] == "length";
        if (empty || cut) && state.retries < 2 {
            state.retries += 1;
            if empty {
                // The reply is dropped, so its usage stays on the event.
                let meta = &message["meta"];
                let extra =
                    json!({"meta": {"usage": meta["usage"], "timing": meta["timing"]}, "finish": meta["finish"]});
                ui::event("empty", "the model sent an empty reply; asking it to act", extra);
                let thinking = message["reasoning_content"].as_str().unwrap_or_default();
                self.discard("the reply was empty", thinking);
                self.retry("was empty")?;
            } else {
                ui::event("length", "the reply hit the output limit; asking the model to continue", Value::Null);
                self.session.push(message)?;
                self.retry("hit the output limit and was cut off; continue from where it stopped")?;
            }
            return Ok(None);
        }
        // Retries count in a row: a reply that is kept starts them over, and one that is still unusable after them ends
        // the turn as stopped.
        if empty || cut {
            let (kind, what) = if empty { ("empty", "an empty reply") } else { ("length", "a cut-off reply") };
            ui::event(kind, &format!("the model sent {what} again; finishing the turn"), Value::Null);
            state.stalled = true;
        } else {
            state.retries = 0;
        }
        Ok(Some(message))
    }

    /// Runs a reply's tool calls in order and logs each result. Returns the report a finish() ends the turn with, if
    /// low takes it, and the question an ask() ends it with.
    async fn act(
        &mut self,
        state: &mut TurnState,
        began: usize,
        calls: &[Value],
    ) -> Result<(Option<String>, Option<String>)> {
        let (mut finished, mut asked) = (None, None);
        for (i, call) in calls.iter().enumerate() {
            self.pending = Some(call["id"].clone());
            let (mut content, mut meta) = self.call(call).await;
            asked = meta["ask"].as_str().map(String::from).or(asked);
            if let Some(report) = meta["finish"].as_str() {
                match self.finishing(began, &meta, i + 1 == calls.len()) {
                    Ok(()) => finished = Some(report.to_string()),
                    Err(why) => {
                        content += &format!("\n[finish() ignored: {why}; reply when you are ready]");
                        meta["finish_ignored"] = why.into();
                    }
                }
            }
            state.ran(call["function"]["arguments"].as_str().unwrap_or_default(), &content);
            self.session.push(json!({"role": "tool", "tool_call_id": call["id"], "content": content, "meta": meta}))?;
            self.pending = None;
        }
        if let Some(why) = self.broken.take() {
            anyhow::bail!(why);
        }
        Ok((finished, asked))
    }

    /// Shows a reply that a run's finish() or ask() gave, and logs it as the model's message.
    fn reply(&mut self, text: &str, via: &str) -> Result<()> {
        ui::prose();
        let mut markdown = crate::render::Markdown::default();
        markdown.push(text);
        markdown.finish();
        self.session.push(json!({"role": "assistant", "content": text, "meta": {"via": via}}))
    }

    /// The end of a reply that ran no code. A turn that changed files without edit() or write() is asked once to clean
    /// up after itself, and one whose check fails on the final tree is asked once to fix it. None when the model was
    /// asked something; otherwise whether the turn changed files and whether the check passed.
    async fn gates(&mut self, state: &mut TurnState, start: &str, began: usize) -> Result<Option<(bool, bool)>> {
        let changed = self.changed(start, began).await;
        // A turn that changed files without edit() or write() most likely left output from running code behind. With
        // another low in this tree its files can land inside our runs' windows too, so it is not asked about then. A
        // deleted file is no leftover: deleting is what a shell command is for here.
        if changed && !state.strayed && !self.session.entries[began..].iter().any(|e| edited(&e["meta"])) {
            state.strayed = true;
            let own = self.own_diff(began).await?.1;
            let paths: Vec<String> =
                own.lines().filter(|l| !l.ends_with(" (deleted)")).filter_map(changed_path).collect();
            if !paths.is_empty() && !self.shared() {
                state.stalled = false;
                ui::event("nudge", "files changed outside edit()/write(): asking the model about them", Value::Null);
                self.host(format!("{STRAY} {}. {STRAY_END}", paths.join(", ")))?;
                return Ok(None);
            }
        }
        // A git repository made inside the tree runs its hooks and config outside the sandbox once the user's git visits
        // it. Under --yolo nothing is sandboxed, so nothing is asked.
        if changed && !self.yolo && !state.planted {
            state.planted = true;
            let paths: Vec<String> = self.own_diff(began).await?.1.lines().filter_map(changed_path).collect();
            let repos = repos(&self.root, &paths);
            if !repos.is_empty() {
                state.stalled = false;
                ui::event("nudge", "a git repository was made inside the tree: asking the model about it", Value::Null);
                self.host(format!("{PLANTED} {}. {PLANTED_END}", repos.join(", ")))?;
                return Ok(None);
            }
        }
        let mut checked = false;
        if changed && let Some(check) = &self.config.check {
            let (ok, out) = self.verify(&check.clone(), true).await;
            if !ok && !state.nudged {
                (state.nudged, state.stalled) = (true, false);
                ui::event("nudge", "the check failed on the final tree: asking the model to fix it", Value::Null);
                self.host(format!("{GATE}\n{}", compact::tail(&out, 1500)))?;
                return Ok(None);
            }
            checked = ok;
        }
        // Nothing has run the report's claims, so a second look reads the code for them. What it finds contradicted goes
        // back to the model once, and the answer gets a second look of its own.
        if changed && !checked && !state.stalled {
            self.claims = self.look(state, start, began).await?;
            let refuted = self.claims.as_ref().filter(|c| !c.contradicted.is_empty() && !state.refuted);
            if let Some(lines) = refuted.map(|c| c.contradicted.iter().map(|c| format!("- {c}")).collect::<Vec<_>>()) {
                (state.refuted, state.stalled) = (true, false);
                let text = "the second look found claims the code contradicts: asking the model about them";
                ui::event("nudge", text, Value::Null);
                self.host(format!("{REFUTED}\n{}\n{REFUTED_END}", lines.join("\n")))?;
                return Ok(None);
            }
        }
        Ok(Some((changed, checked)))
    }

    /// A second look at the report of a turn that changed files with no check passing. The same model, in a conversation
    /// of its own that starts with the turn's system prompt and the brief but has none of the turn's messages, checks
    /// the report's riskiest claims by reading the code in a worker that can't change files. Its requests are logged as
    /// aside events: the turn's cost and /cost count them, the turn's conversation never sees them. None when it settled
    /// nothing: no report, an error, or no verdict within LOOK_REQUESTS.
    async fn look(&mut self, state: &mut TurnState, start: &str, began: usize) -> Result<Option<Claims>> {
        let turn = &self.session.entries[began..];
        let report = turn.iter().rev().find(|e| e["role"] == "assistant").and_then(|e| e["content"].as_str());
        let Some(report) = report.filter(|r| !r.trim().is_empty()).map(String::from) else { return Ok(None) };
        let brief = self.brief(start, began, &report).await;
        ui::show(
            "second look: checking the report's claims against the code, in a conversation that can't change files",
        );
        let mut messages = vec![
            json!({"role": "system", "content": self.session.header()["system"]}),
            json!({"role": "user", "content": format!("{LOOK}\n\n{brief}")}),
        ];
        let mut worker = None;
        for n in 1..=LOOK_REQUESTS {
            let mut message = match self.llm.complete(messages.clone(), &tools()).await {
                Ok(message) => message,
                Err(e) => {
                    let text = format!("second look: stopped by an error, so it settled nothing: {}", error_text(&e));
                    ui::event("claims", &text, Value::Null);
                    return Ok(None);
                }
            };
            state.spent += weighted(&message["meta"]);
            ui::usage(&message["meta"]["usage"]);
            messages.push(compact::wire(&message));
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            let mut verdict = calls.is_empty().then(|| message["content"].as_str().unwrap_or_default().to_string());
            let mut runs = vec![];
            for call in &calls {
                let args = call["function"]["arguments"].as_str().unwrap_or_default();
                let args: Value = serde_json::from_str(args).unwrap_or_default();
                let (out, meta) = match args["code"].as_str() {
                    Some(code) => look_run(&mut worker, &self.root, self.yolo, code).await,
                    None => ("error: the only tool is run(code)".into(), json!({"error": true})),
                };
                verdict = meta["finish"].as_str().map(String::from).or(verdict);
                runs.push(json!({"code": args["code"], "out": out}));
                messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": out}));
            }
            // Logged without a role, so the turn's conversation never carries it.
            if let Some(fields) = message.as_object_mut() {
                fields.remove("role");
            }
            (message["meta"]["aside"], message["runs"]) = (true.into(), runs.into());
            ui::record("aside", &format!("second look, request {n}"), message);
            if let Some(verdict) = verdict {
                let claims = Claims::of(&verdict);
                let text =
                    claims.as_ref().map_or("its answer named no claims, so it settled nothing".into(), Claims::summary);
                ui::event("claims", &format!("second look: {text}"), json!({"claims": claims}));
                return Ok(claims);
            }
        }
        let text = format!("second look: no verdict in {LOOK_REQUESTS} requests, so it settled nothing");
        ui::event("claims", &text, Value::Null);
        Ok(None)
    }

    /// What a second look gets: the task with the user's steers, the criteria, what each of the turn's runs edited and
    /// ran, the diff of the turn's own files (redacted, at most LOOK_DIFF bytes; outside git, their names) and the report.
    async fn brief(&self, start: &str, began: usize, report: &str) -> String {
        let turn = &self.session.entries[began..];
        let text = |e: &Value| e["content"].as_str().unwrap_or_default().to_string();
        let mut task = text(&turn[0]);
        for steer in turn.iter().filter(|e| e["meta"]["steer"] == true) {
            task += &format!("\n\n[Steered by the user]\n{}", text(steer));
        }
        let criteria = self.session.state().accepted.unwrap_or_else(|| "none set".into());
        let runs = turn.iter().filter(|e| e["role"] == "tool").enumerate();
        let runs: Vec<String> =
            runs.filter_map(|(n, e)| Some(format!("run {}: {}", n + 1, did(&e["meta"])?))).collect();
        let runs = if runs.is_empty() { "-".into() } else { runs[runs.len().saturating_sub(LOOK_RUNS)..].join("\n") };
        let diff = match self.patch(start, began).await {
            Ok(diff) if diff.len() > LOOK_DIFF => {
                let (kb, cut) = (diff.len().div_ceil(1024), diff.floor_char_boundary(LOOK_DIFF));
                format!("{}\n[cut at {} KB of {kb} KB: read the files for the rest]", &diff[..cut], LOOK_DIFF / 1024)
            }
            Ok(diff) if !diff.trim().is_empty() => diff,
            _ => format!("[no diff to show: read these files]\n{}", own_changes(turn)),
        };
        format!(
            "## Task\n{task}\n\n## Criteria\n{criteria}\n\n## What the turn's runs did\n\
                 (In each run: its edits, then its shell commands, then its check()s; the order within a run is not \
                 recorded.)\n{runs}\n\n## Diff\n{diff}\n\n\
                 ## Report\n{report}"
        )
    }

    /// The turn's own changes as a patch from its snapshot to the tree now, redacted; empty outside git.
    async fn patch(&self, start: &str, began: usize) -> Result<String> {
        if !self.repo {
            return Ok(String::new());
        }
        let paths: Vec<String> = self.own_diff(began).await?.1.lines().filter_map(changed_path).collect();
        let now = self.tree().await?;
        let (Some(from), Some(to)) = (start.split(' ').next(), now.split(' ').next()) else { return Ok(String::new()) };
        if paths.is_empty() {
            return Ok(String::new());
        }
        let args = ["--literal-pathspecs", "diff-tree", "-p", "--no-color", "--no-ext-diff", from, to, "--"];
        let args: Vec<String> = args.into_iter().map(String::from).chain(paths).collect();
        let root = self.root.clone();
        let patch = tokio::task::spawn_blocking(move || git(&root, None, &args)).await??;
        Ok(redact(&patch).0)
    }

    /// Tells the model something from low, and waits for its reply.
    fn host(&mut self, text: String) -> Result<()> {
        ui::activity("waiting for the model");
        self.session.push(json!({"role": "user", "content": text, "meta": {"host": true}}))
    }

    /// Whether finish() may end the turn in place of a reply: only where the check, if one is configured, covers the
    /// report's claims and the model has read everything the run printed. In a turn without edits nothing checks the
    /// report, and an answer written in the run that gathers its evidence was written before reading it. Without a
    /// check the verdict says NO CHECK either way: refusing there only cost a request.
    fn finishing(&self, began: usize, meta: &Value, last: bool) -> Result<(), &'static str> {
        let edits = edited(meta) || self.session.entries[began..].iter().any(|e| edited(&e["meta"]));
        if meta["error"] == true {
            Err("the run raised")
        } else if !last {
            Err("more code ran after it in this reply")
        } else if !edits {
            Err("this turn changed no files, so no check covers the report")
        } else if self.config.check.is_some() && meta["check"] != true {
            Err("check() did not pass in this run")
        } else if meta["own"].as_u64().is_none_or(|own| own > 0) {
            Err("this run printed output you have not read")
        } else {
            Ok(())
        }
    }

    /// The files that changed between the end of the last turn and the start of this one, outside the conversation: the
    /// user's editor, a pull, a branch switch. None when nothing did or the last turn's tree is unknown.
    async fn moved_since(&self, start: &str) -> Option<Vec<String>> {
        let last = self.session.entries.iter().rev().find_map(|e| Report::of(e)?.tree)?;
        let (last, now) = (last.split(' ').next()?.to_string(), start.split(' ').next()?.to_string());
        if last == now || now.is_empty() {
            return None;
        }
        let (root, args) = (self.root.clone(), ["diff-tree", "-r", "--name-status", "--no-renames", "-z", &last, &now]);
        let args = args.map(String::from);
        let out = tokio::task::spawn_blocking(move || git(&root, None, &args)).await.ok()?.ok()?;
        let fields: Vec<&str> = out.split('\0').filter(|f| !f.is_empty()).collect();
        let files: Vec<String> = fields
            .chunks(2)
            .filter(|pair| pair.get(1).is_none_or(|path| self.quiet.as_deref() != Some(*path)))
            .filter_map(|pair| match pair {
                ["A", path] => Some(format!("{path} (new)")),
                ["D", path] => Some(format!("{path} (deleted)")),
                [_, path] => Some(path.to_string()),
                _ => None,
            })
            .collect();
        (!files.is_empty()).then_some(files)
    }

    /// The model profile's `fallback = "provider:model"`, when it names a configured provider other than the current one.
    fn fallback(&self) -> Option<(String, String)> {
        let (provider, model) = self.session.header()["fallback"].as_str()?.split_once(':')?;
        let other = provider != self.provider || model != self.session.header()["model"];
        (other && self.config.providers.contains_key(provider)).then(|| (provider.to_string(), model.to_string()))
    }

    /// Names a file low itself rewrites between turns, such as a --tasks checklist, so the turns don't report it.
    pub fn quiet(&mut self, file: &Path) {
        let file = file.canonicalize().unwrap_or_else(|_| self.root.join(file));
        self.quiet = file.strip_prefix(&self.root).ok().map(|p| p.to_string_lossy().into_owned());
    }

    /// Whether this turn changed files: an edit() or write(), or a change its own code made. Changes another writer in
    /// the same tree made meanwhile (the user's editor, another low) don't count.
    async fn changed(&self, start: &str, began: usize) -> bool {
        self.session.entries[began..].iter().any(|e| edited(&e["meta"]))
            || self.moved(start).await && self.own_diff(began).await.is_ok_and(|(_, own)| own != "none")
    }

    /// What changed since the turn's snapshot, and the part of it this turn made, as `changes()` lists them.
    async fn own_diff(&self, began: usize) -> Result<(String, String)> {
        let at = self.session.entries[began]["meta"]["at"].as_u64().unwrap_or(0);
        let all = self.diff(began, std::time::UNIX_EPOCH + Duration::from_millis(at)).await?;
        let own = attributed(&self.root, &self.session.entries[began..], &all);
        Ok((all, own))
    }

    /// Whether another low holds a session of this repository right now, so the tree has other writers.
    fn shared(&self) -> bool {
        let Some(dir) = self.session.path.parent() else { return false };
        let root = self.root.to_string_lossy();
        let recent =
            |p: &Path| p.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a < DAY));
        let here = |p: &Path| {
            let mut line = String::new();
            let read = std::fs::File::open(p).map(std::io::BufReader::new).and_then(|mut f| f.read_line(&mut line));
            read.is_ok() && serde_json::from_str::<Value>(&line).is_ok_and(|h| h["cwd"] == *root)
        };
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| *p != self.session.path && p.extension().is_some_and(|e| e == "jsonl"))
            .any(|p| recent(&p) && here(&p) && crate::serve::locked(&p))
    }

    fn sent(&self) -> u64 {
        serde_json::to_string(&self.session.messages()).map_or(0, |m| m.len() as u64) / 4
    }

    /// The request's size right after the session's last compaction, by bytes: the system prompt, the summary and the
    /// entries it kept. From the log, so a resumed session waits as long.
    fn compacted(&self) -> u64 {
        let entries = &self.session.entries;
        let Some(at) = self.session.compaction() else { return 0 };
        let keep = entries[at]["compact"]["keep"].as_u64().unwrap_or(0) as usize;
        let summary = entries[at]["compact"]["summary"].as_str().map_or(0, str::len);
        let system = self.session.header()["system"].as_str().map_or(0, str::len);
        let kept = entries.get(keep..at).into_iter().flatten().filter(|e| e["role"].is_string());
        let kept: usize = kept.map(|e| compact::wire(e).to_string().len()).sum();
        ((system + summary + kept) / 4) as u64
    }

    /// The session's budget (session_budget, in compact_at units) once its replies have spent it: no turn goes on then.
    pub fn over_budget(&self) -> Option<u64> {
        let total = self.session.header()["session_budget"].as_u64()?;
        (self.spent() >= self.compact_at * total).then_some(total)
    }

    /// What the session's replies have spent, weighed as a turn's budget weighs them.
    fn spent(&self) -> u64 {
        self.session.spent()
    }

    /// Logs a request that found less than half of the previous request's prompt in the provider's cache, with the cause
    /// when low knows it: a compaction, a model switch, or a new task.
    fn cache_miss(&self, usage: &Value) {
        let entries = &self.session.entries;
        let Some(prev) = entries.iter().rposition(|e| prompt_tokens(e).is_some()) else { return };
        let expected = prompt_tokens(&entries[prev]).unwrap_or(0);
        let Some(cached) = ui::cached(usage).filter(|&c| expected >= 1024 && c * 2 < expected) else { return };
        let since = &entries[prev + 1..];
        let cause = if since.iter().any(|e| e["compact"].is_object()) {
            "compaction"
        } else if since.iter().any(|e| e["switch"].is_object()) {
            "model switch"
        } else if since.iter().any(compact::task) {
            "new task"
        } else {
            "unexplained"
        };
        let text = format!("{cached} of the previous request's {expected} prompt tokens were cached ({cause})");
        ui::record("cache_miss", &text, json!({"cause": cause, "cached": cached, "expected": expected}));
    }

    /// The next request's size in tokens: the provider's count for the last one plus what has been added since, or
    /// the whole request by bytes when no count has come back since the last compaction.
    fn upcoming(&self) -> u64 {
        let entries = &self.session.entries;
        let compacted = self.session.compaction().map_or(0, |i| i + 1);
        let counted = (compacted..entries.len()).rev().find_map(|i| Some((i, prompt_tokens(&entries[i])?)));
        let Some((i, prompt)) = counted else { return self.sent() };
        let added = (i..entries.len()).filter(|&j| entries[j]["role"].is_string());
        let added: u64 = added.map(|j| compact::wire(&entries[j]).to_string().len() as u64).sum();
        prompt + added / 4
    }

    fn overflow(&mut self, window: Option<u64>) -> Result<()> {
        let (seen, estimate) = (self.session.last_prompt_tokens(), self.upcoming());
        // A fifth stays free: the reply needs room too, and the estimate counts bytes, not tokens.
        self.compact_at = self.compact_at.min(window.unwrap_or(estimate) * 4 / 5);
        let done = self.compact_to(self.compact_at as usize / 5, true)?;
        ui::record("note", &done, Value::Null);
        self.compact_at = self.compact_at.max(self.sent());
        let extra = json!({"seen": seen, "estimate": estimate, "window": window, "compact_at": self.compact_at});
        ui::event("overflow", "context window exceeded; compacted and retrying", extra);
        Ok(())
    }

    fn discard(&mut self, why: &str, thinking: &str) {
        if !thinking.trim().is_empty() {
            self.discarded = Some((self.session.entries.len(), format!("*Discarded: {why}.*\n\n{thinking}")));
        }
    }

    /// Asks again after a reply that came back unusable: lost, empty, or cut off at the output limit.
    fn retry(&mut self, what: &str) -> Result<()> {
        let content = format!("Your last reply {what}. {RETRY}");
        ui::record("nudge", &content, Value::Null);
        self.session.push(json!({"role": "user", "content": content, "meta": {"host": true, "nudge": true}}))
    }

    /// Once a turn, after the reply whose request brought it to three quarters of its token ceiling or of MAX_REQUESTS,
    /// tells the model about how many requests it has left: the tokens left over the last request's `cost`, since each
    /// costs more as the context grows. Not when that reply reached the limit itself: the handoff follows instead. If a
    /// compaction drops the note, it is not sent again.
    fn warn(&mut self, state: &mut TurnState, ceiling: u64, made: usize, cost: u64) -> Result<()> {
        let near = state.spent * 4 >= ceiling * 3 || made * 4 >= MAX_REQUESTS * 3;
        if state.warned || !near || state.spent >= ceiling || made >= MAX_REQUESTS {
            return Ok(());
        }
        state.warned = true;
        let left = (ceiling - state.spent).div_ceil(cost.max(1)).min((MAX_REQUESTS - made) as u64);
        let s = if left == 1 { "" } else { "s" };
        let text = format!("{WARN}: low stops it after about {left} more request{s} at the current size. {WARN_END}");
        let extra = json!({"left": left, "spent": state.spent, "ceiling": ceiling, "requests": made});
        ui::event("budget", &format!("three quarters of the turn's budget used, about {left} request{s} left"), extra);
        self.session.push(json!({"role": "user", "content": text, "meta": {"host": true, "budget": true}}))
    }

    /// Asks the model, once, where the work stands after a turn-scoped limit stopped the turn, and keeps its reply as
    /// the turn's answer. No code runs: a run() in the reply gets NOT_RUN for its result. An unusable reply or an error
    /// leaves the turn without a handoff, as it was before low asked for one; an empty one leaves request()'s nudge to
    /// act behind it. Its request counts toward session_budget, so a session that had only that much left gives a
    /// --tasks task no extra turn.
    async fn handoff(&mut self, state: &mut TurnState) -> Result<()> {
        ui::show("asking the model where the work stands; no more code runs this turn");
        self.host(HANDOFF.into())?;
        let message = match self.request(state).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                ui::note("no handoff: the reply was unusable");
                return Ok(());
            }
            Err(e) => {
                ui::note(&format!("no handoff: {}", error_text(&e)));
                return Ok(());
            }
        };
        let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
        self.session.push(message)?;
        if !calls.is_empty() {
            ui::event("handoff", "the model called run() after the limit; not run", Value::Null);
        }
        for call in calls {
            self.session.push(json!({"role": "tool", "tool_call_id": call["id"], "content": NOT_RUN,
                                     "meta": {"not_run": "limit"}}))?;
        }
        Ok(())
    }

    async fn call(&mut self, call: &Value) -> (String, Value) {
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or_default()).unwrap_or_default();
        match (call["function"]["name"].as_str(), args["code"].as_str()) {
            (Some("run"), Some(code)) if MARKUP.iter().any(|m| code.contains(m)) => {
                ui::event(
                    "bad_call",
                    "the model's run() had its own tool-call markup in the code; not run",
                    Value::Null,
                );
                let out = "error: your code had tool-call markup in it (<｜end▁of▁thinking｜>, <｜｜DSML｜｜ …>), so \
                           none of it ran; send the run() again with the Python alone";
                (out.into(), json!({"tool": "run", "error": true}))
            }
            (Some("run"), Some(code)) => self.run_code(code).await,
            (Some("run"), None) => {
                ui::event(
                    "bad_call",
                    "the model's run() came without code (cut off by the output limit?)",
                    Value::Null,
                );
                let out = "error: run() arrived without a code argument, probably cut off by the output limit; \
                           send it again in smaller steps";
                (out.into(), json!({"tool": "run", "error": true}))
            }
            (Some(name), _) if HELPERS.contains(&name) => {
                let args = call["function"]["arguments"].as_str().unwrap_or("{}");
                let literal = serde_json::to_string(args).unwrap_or_default();
                ui::event("bad_call", &format!("the model called {name} as a tool; running it in run()"), Value::Null);
                self.run_code(&format!("{name}(**__import__('json').loads({literal}))")).await
            }
            (name, _) => {
                let text =
                    format!("the model called {}, but the only tool is run(code)", name.unwrap_or("a nameless tool"));
                ui::event("bad_call", &text, Value::Null);
                ("error: the only tool is run(code)".into(), json!({"tool": "run", "error": true}))
            }
        }
    }

    async fn run_code(&mut self, code: &str) -> (String, Value) {
        ui::run(code);
        let (state, turn) = (self.session.state(), self.current.map(|(began, _)| began));
        // undo() restores from the snapshot this turn began with.
        let snapshot =
            turn.and_then(|began| self.session.entries[began]["meta"]["snapshot"].as_str()).unwrap_or_default();
        let state = json!({"accepted": state.accepted, "plan": state.plan, "guides": self.session.guides(), "turn": turn,
                           "snapshot": snapshot});
        let clock = Instant::now();
        let ran = async {
            let mut reply = self.worker()?.run(code, &state).await?;
            while reply["needs_check"] == true {
                if let Some(requests) = reply["web"].as_array().cloned() {
                    let out = self.web(&requests).await;
                    reply = self.worker()?.resume(true, &out).await?;
                    continue;
                }
                // The sandbox hides the session log, so low reads the entry the run asks for. A number that names no
                // entry, such as -1, is refused as #0 is, never taken for a check() request.
                if !reply["recall"].is_null() {
                    let n = reply["recall"].as_u64().and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
                    let (ok, out) = match crate::compact::recall(&self.session.entries, self.session.keep(), n) {
                        Ok(text) => (true, text),
                        Err(why) => (false, why),
                    };
                    reply = self.worker()?.resume(ok, &out).await?;
                    continue;
                }
                if stale(&reply) {
                    self.passed = None;
                }
                let (ok, out) = match self.config.check.clone() {
                    Some(check) => self.verify(&check, false).await,
                    None => (false, "no check command configured (set check in low.toml)".into()),
                };
                reply = self.worker()?.resume(ok, &out).await?;
            }
            anyhow::Ok(reply)
        };
        match ran.await {
            Ok(reply) => {
                self.restarts = 0;
                let (out, mut meta) = finish(reply);
                if stale(&meta) {
                    self.passed = None;
                }
                meta["ms"] = ui::ms(clock).into();
                ui::ran(&meta, &out);
                (out, meta)
            }
            Err(e) => {
                (self.worker, self.passed) = (None, None);
                self.restarts += usize::from(e.downcast_ref::<worker::StartFailed>().is_some());
                if self.restarts >= RESTARTS {
                    self.broken = Some(format!("{e:#} ({RESTARTS} times in a row; ending the turn)"));
                }
                let out = format!("error: {e:#}; the worker was restarted and its variables are gone");
                ui::event("worker_restart", &out, Value::Null);
                (out, json!({"tool": "run", "error": true, "ms": ui::ms(clock)}))
            }
        }
    }

    /// A run's search() and fetch() requests, made here since the sandbox has no network: DeepSeek's tools API, with the
    /// provider that config.toml's search names, WEB_AT_ONCE at a time. The answer is each request's [ok, response or
    /// why it failed], in the order asked.
    async fn web(&mut self, requests: &[Value]) -> String {
        let provider = self.config.search.as_ref().and_then(|name| self.config.providers.get(name));
        let provider = provider.map(|p| (p.base_url.clone(), p.api_key.clone()));
        let _busy = ui::busy(&match requests {
            [one] => format!("{} on the web", if one["fetch"].is_string() { "fetching" } else { "searching" }),
            many => format!("{} web requests at once", many.len()),
        });
        // A search() earlier in this run is still queued for the log, where fetchable() looks.
        let _ = self.session.flush();
        let mut answers = vec![(false, "the request did not finish".to_string()); requests.len()];
        let mut asked = vec![];
        for (i, request) in requests.iter().enumerate() {
            match self.web_request(request, i) {
                Ok(call) if provider.is_none() && direct(&call).is_none() => {
                    answers[i].1 = UNSET[usize::from(call.0 == "fetch")].into()
                }
                Ok(call) => asked.push((i, call)),
                Err(refused) => answers[i].1 = refused,
            }
        }
        let (mut asked, mut running) = (asked.into_iter(), tokio::task::JoinSet::new());
        loop {
            while running.len() < WEB_AT_ONCE
                && let Some((i, call)) = asked.next()
            {
                let provider = provider.clone();
                running.spawn(async move {
                    // A fetch() on an open host is low's own request first; the tools API gets what that sends on.
                    let elsewhere = match direct(&call) {
                        Some(url) => match fetch::direct(url, open).await {
                            Ok(Fetched::Text(text)) => {
                                let raw = json!({"content": text, "raw": true}).to_string();
                                return (i, call, Ok(raw), "direct", None);
                            }
                            Err(e) => return (i, call, Err(e), "direct", None),
                            Ok(Fetched::Elsewhere(why)) => Some(why),
                        },
                        None => None,
                    };
                    let (tool, body) = (call.0, &call.1);
                    let result = match &provider {
                        Some((base, key)) => llm::tool(base, key.as_deref(), tool, body).await,
                        None => Err(anyhow::anyhow!(
                            "{}; low reads that only through DeepSeek's tools API, and config.toml's search names no \
                             provider",
                            elsewhere.as_deref().unwrap_or_default()
                        )),
                    };
                    (i, call, result, "tools API", elsewhere)
                });
            }
            let Some(done) = running.join_next().await else { break };
            let Ok((i, (tool, body, what), result, via, elsewhere)) = done else { continue };
            let (ok, out) = match result {
                Ok(text) => (true, text),
                Err(e) => (false, format!("{tool} failed: {e:#}")),
            };
            // The results' URLs are what a later fetch() may get, in this run or after a resume.
            let found = serde_json::from_str::<Value>(&out).ok().filter(|_| ok && tool == "search");
            let urls: Vec<&str> = found
                .iter()
                .flat_map(|f| f["results"].as_array().into_iter().flatten())
                .filter_map(|r| r["url"].as_str())
                .collect();
            let extra =
                json!({"request": body, "ok": ok, "bytes": out.len(), "urls": urls, "via": via, "direct": elsewhere});
            ui::record("web", &what, extra);
            answers[i] = (ok, out);
        }
        json!(answers).to_string()
    }

    /// The tools API call a run's request makes, as (tool, body, what the log says), or why low refuses it. The request
    /// would leave the machine, so low refuses one whose argument holds what looks like a secret, and fetch() only takes
    /// a URL that a search() returned, that the user wrote, or on an OPEN host: one the run made up for any other host
    /// could carry what it read to a server whose owner reads it. `i` is its place in the run's list.
    fn web_request(&self, request: &Value, i: usize) -> Result<(&'static str, Value, String), String> {
        if i >= WEB_LIST {
            return Err(format!("only {WEB_LIST} requests go in one list"));
        }
        let (tool, body, what) = match request["fetch"].as_str() {
            Some(url) => ("fetch", json!({"url": url}), format!("fetch {url}")),
            None => (
                "search",
                json!({"query": request["search"], "count": request["count"]}),
                format!("search {}", request["search"]),
            ),
        };
        let (what, hidden) = redact(&what);
        if hidden > 0 {
            ui::record("web", &what, json!({"ok": false, "refused": "secret"}));
            return Err(format!("{tool}() refused: its argument holds what looks like a secret"));
        }
        if let Some(url) = request["fetch"].as_str()
            && !self.fetchable(url)
        {
            ui::record("web", &what, json!({"ok": false, "refused": "url"}));
            let why = "fetch() takes a URL that a search() in this session returned, that the task gives, or on";
            let next = "a URL you build for another host stays refused: fetch one from search()'s results";
            return Err(format!("{why} {}; {next}", OPEN.join(", ")));
        }
        Ok((tool, body, what))
    }

    /// Whether fetch() may get this URL: it is on an OPEN host, a search() in this session returned it, or a task of the
    /// user's names it.
    fn fetchable(&self, url: &str) -> bool {
        if open(url) {
            return true;
        }
        let url = bare(url);
        self.session.entries.iter().any(|e| {
            let urls = e["urls"].as_array().filter(|_| e["event"] == "web").into_iter().flatten();
            let task = e["content"].as_str().filter(|_| e["role"] == "user" && e["meta"]["host"].is_null());
            urls.filter_map(Value::as_str).any(|u| bare(u) == url) || task.is_some_and(|t| t.contains(url))
        })
    }

    fn worker(&mut self) -> Result<&mut Worker> {
        let worker = match self.worker.take() {
            Some(worker) => worker,
            None => Worker::start(&self.root, self.yolo, self.config.check.as_deref(), self.config.fmt.as_deref())?,
        };
        Ok(self.worker.insert(worker))
    }

    async fn verify(&mut self, check: &str, gate: bool) -> (bool, String) {
        let clock = Instant::now();
        let before = self.tree().await;
        if self.repo
            && let (Ok(tree), Some((passed, out))) = (&before, &self.passed)
            && tree == passed
        {
            let out = format!("{out}\n[reused: the tree is unchanged since this check passed]");
            ui::log(json!({"check": {"ok": true, "gate": gate, "reused": true, "ms": ui::ms(clock)}}));
            return (true, out);
        }
        if gate {
            ui::run("check()");
        } else {
            ui::activity("running check()");
        }
        let index = self.session.index();
        let (ok, out, after) = trial(&self.root, &index, self.repo, self.yolo, check, &before, CHECK_TIMEOUT).await;
        if gate && ok {
            ui::settle();
        } else if gate {
            ui::ran(&json!({"actions": [{"tool": "check", "ok": false}]}), &out);
        }
        self.passed = after.ok().filter(|_| ok).map(|tree| (tree, out.clone()));
        let ms = ui::ms(clock);
        ui::log(json!({"check": {"ok": ok, "gate": gate, "ms": ms, "out": compact::tail(&out, 1500)}}));
        (ok, out)
    }

    /// The working tree as a git tree id. Git runs on the blocking pool, so Ctrl-C and the status bar stay live
    /// while a large repo is snapshotted.
    async fn tree(&self) -> Result<String> {
        tree_at(&self.root, &self.session.index(), self.repo).await
    }

    async fn moved(&self, start: &str) -> bool {
        self.tree().await.map_or_else(
            |e| {
                ui::event("error", &format!("could not tell whether the tree changed: {e:#}"), Value::Null);
                true
            },
            |tree| tree != start,
        )
    }

    /// The end of a turn: a stopped one (at a limit, or silent with nothing changed) fails even when nothing changed.
    async fn report(
        &mut self,
        changed: Option<bool>,
        checked: bool,
        stopped: bool,
        began: usize,
        clock: Instant,
    ) -> Result<bool> {
        self.session.flush()?;
        // The next turn compares its starting tree with this one to tell the model what moved in between.
        let tree = if self.repo { self.tree().await.ok() } else { None };
        let (all, listed) = if self.repo {
            self.own_diff(began).await?
        } else {
            let own = own_changes(&self.session.entries[began..]);
            (own.clone(), own)
        };
        let lines = |list: &str| list.lines().filter(|l| *l != "none").map(String::from).collect::<Vec<_>>();
        let mut changes = lines(&listed);
        // Files another writer changed during the turn: neither this turn's to report nor its to undo.
        let elsewhere = lines(&all).len() - changes.len();
        let ignored = if self.repo { self.ignored(began, &changes).await } else { vec![] };
        changes.extend(ignored.iter().map(|path| format!("{path} (ignored by git)")));
        let first = &self.session.entries[began];
        let snapshot = first["meta"]["snapshot"].as_str().and_then(|s| s.split(' ').next()).map(String::from);
        let changed = changed.unwrap_or(!changes.is_empty());
        let turn = &self.session.entries[began..];
        let actions = turn.iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten());
        let own: Vec<&str> = actions.filter(|a| a["ok"] == true).filter_map(|a| a["path"].as_str()).collect();
        let paths = changes.iter().filter_map(|l| changed_path(l));
        let (mine, outside): (Vec<String>, Vec<String>) = paths.partition(|p| own.contains(&p.as_str()));
        let inputs: Vec<String> = changes.iter().filter_map(|l| changed_path(l)).filter(|p| self.checks(p)).collect();
        let question = self.question.take();
        let claims = self.claims.take();
        let configured = self.config.check.is_some();
        let detected = self.config.detected.as_ref();
        let (check, verdict) = verdict(changed, checked, configured, !inputs.is_empty(), question.is_some(), detected);
        let checked = checked && inputs.is_empty();
        let state = self.session.state();
        let task = first["content"].as_str().unwrap_or_default();
        let mut rows = vec![("task", task.lines().next().unwrap_or_default().to_string())];
        rows.push(("criteria", state.accepted.clone().unwrap_or_else(|| "-".into())));
        let steps = state.plan["steps"].as_array().map(Vec::len);
        let done = state.plan["done"].as_array().map_or(0, Vec::len);
        if let Some(steps) = steps {
            rows.push(("plan", format!("{done}/{steps} steps done")));
        }
        rows.push(("check", check.clone()));
        if !inputs.is_empty() {
            rows.push(("inputs", format!("check inputs changed: {}", inputs.join(", "))));
        }
        if let Some(claims) = &claims {
            let refuted = claims.contradicted.iter().map(|c| format!("contradicted: {c}")).collect::<Vec<_>>();
            let refuted = if refuted.is_empty() { String::new() } else { format!("\n{}", capped(&refuted, "\n")) };
            rows.push(("claims", format!("{}, by a second look at the diff{refuted}", claims.summary())));
        }
        rows.push(("changes", if changes.is_empty() { "none".into() } else { capped(&changes, "\n") }));
        if elsewhere > 0 {
            rows.push(("elsewhere", format!("{elsewhere} more changed by another writer in this tree, left out")));
        }
        if !outside.is_empty() {
            let text = format!("not through low's edit()/write(), maybe outside low: {}", capped(&outside, ", "));
            rows.push(("outside", text));
        }
        if changed && !self.repo {
            rows.push(("undo", "none: not a git repository, so these changes can't be undone".into()));
        }
        if let Some(tree) = snapshot.filter(|_| changed && self.repo) {
            rows.push(("undo", self.undo(&tree, mine, outside.is_empty() && elsewhere == 0).await?));
        }
        if !ignored.is_empty() {
            rows.push(("ignored", format!("{}: git ignores them, so no undo restores them", ignored.join(", "))));
        }
        let paths: Vec<String> = changes.iter().filter_map(|l| changed_path(l)).collect();
        let repos = if self.yolo { vec![] } else { repos(&self.root, &paths) };
        if !repos.is_empty() {
            let text = format!(
                "git repositories made inside the tree, which the sandbox cannot contain: {}",
                repos.join(", ")
            );
            rows.push(("repos", text));
        }
        let totals = crate::stats::totals(turn);
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let (requests, runs, tokens) = (n(&totals["requests"]), n(&totals["runs"]), &totals["tokens"]);
        let (prompt, cached, out) = (n(&tokens["prompt"]), n(&tokens["cached"]), n(&tokens["out"]));
        let ms = ui::ms(clock);
        let cost = format!(
            "{requests} requests · {runs} runs · {} in · {} out · {}",
            ui::tokens(prompt),
            ui::tokens(out),
            ui::minutes(ms / 1000)
        );
        rows.push(("cost", cost));
        let good = (!changed || checked || !configured) && !stopped;
        ui::report(&rows, verdict, good);
        let answer = turn.iter().rev().find(|e| e["role"] == "assistant").and_then(|e| e["content"].as_str());
        let (status, exit) = outcome(good, stopped, question.is_some());
        let undo = rows.iter().find(|(label, _)| *label == "undo").map(|(_, undo)| undo.clone());
        let report = TurnReport {
            session: self.session.path.clone(),
            id: self.session.header()["id"].as_str().unwrap_or_default().into(),
            verdict: verdict.into(),
            good,
            answer: answer.map(String::from),
            question,
            criteria: state.accepted.clone(),
            plan: state.plan,
            check,
            claims: claims.clone(),
            changes: changes.clone(),
            repos,
            elsewhere,
            limit: self.limit.take(),
            undo,
            requests,
            tokens: Tokens { prompt, cached, out },
            cost: totals["cost"].as_f64(),
            ms,
            rows: rows.into_iter().map(|(label, value)| (label.into(), value)).collect(),
        };
        self.last = Some(Outcome { turn: Some(report), status: status.into(), exit, error: None });
        let record = Report {
            verdict: verdict.into(),
            tree,
            changed,
            checked,
            stopped,
            check_inputs: inputs,
            claims,
            outside,
            elsewhere,
            criteria: state.accepted,
            plan_steps: steps,
            plan_done: done,
            changes,
            requests,
            runs,
            prompt,
            cached,
            out,
            ms,
        };
        self.session.push(json!({"report": record}))?;
        Ok(good)
    }

    /// Whether the check runs this file (its command names it) or it is low.toml: a turn that changed it changed what
    /// verifies it.
    fn checks(&self, path: &str) -> bool {
        let check = self.config.check.as_deref().unwrap_or_default();
        let mut words = check.split(|c: char| c.is_whitespace() || "'\"();&|<>=`".contains(c));
        path == "low.toml" || words.any(|w| w.trim_start_matches("./") == path)
    }

    /// The files this turn's edit() and write() changed that git ignores: no diff or snapshot holds them, so the report
    /// lists them itself and says no undo brings them back.
    async fn ignored(&self, began: usize, changes: &[String]) -> Vec<String> {
        let listed: Vec<String> = changes.iter().filter_map(|line| changed_path(line)).collect();
        let actions =
            self.session.entries[began..].iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten());
        let mut paths: Vec<String> = actions
            .filter(|a| a["ok"] == true)
            .filter_map(|a| a["path"].as_str())
            .filter(|path| !listed.iter().any(|l| l == path))
            .map(String::from)
            .collect();
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            return paths;
        }
        let args: Vec<String> = ["check-ignore", "--"].into_iter().map(String::from).chain(paths).collect();
        let root = self.root.clone();
        let out = tokio::task::spawn_blocking(move || git(&root, None, &args)).await.ok().and_then(Result::ok);
        out.unwrap_or_default().lines().map(String::from).collect()
    }

    /// The undo row: restore from the turn's snapshot the whole tree when nothing else changed it meanwhile (`alone`),
    /// else only low's edit()/write() paths, since restoring the rest would also undo what other writers did.
    async fn undo(&self, tree: &str, mine: Vec<String>, alone: bool) -> Result<String> {
        let short = tree.get(..12).unwrap_or(tree);
        let known = if mine.is_empty() {
            String::new()
        } else {
            let mut args =
                ["--literal-pathspecs", "ls-tree", "-z", "--name-only", tree, "--"].map(String::from).to_vec();
            args.extend(mine);
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || git(&root, None, &args)).await?.unwrap_or_default()
        };
        let modified: Vec<&str> = known.split('\0').filter(|p| !p.is_empty()).collect();
        Ok(match (alone, modified.is_empty()) {
            (true, _) => format!("git restore --source={short} --worktree -- .  (new files stay)"),
            (false, false) => format!(
                "git restore --source={short} --worktree -- {}  (low's edit()/write() paths; new files stay)",
                modified.iter().map(|p| quoted(p)).collect::<Vec<_>>().join(" ")
            ),
            (false, true) => format!("git restore --source={short} --worktree -- <path>  (new files stay)"),
        })
    }

    /// What changed since the turn's snapshot, a line per file as the turn report lists them ("none" when nothing did).
    async fn diff(&self, began: usize, since: std::time::SystemTime) -> Result<String> {
        let snapshot = self.session.entries[began]["meta"]["snapshot"].as_str().and_then(|s| s.split(' ').next());
        let (root, base, index) = (self.root.clone(), snapshot.map(String::from), self.session.index());
        let _busy = ui::busy("diffing the working tree");
        Ok(tokio::task::spawn_blocking(move || changes(&root, base.as_deref().map(|t| (t, index.as_path())), since))
            .await?)
    }

    pub fn cost(&self) {
        let t = crate::stats::totals(&self.session.entries);
        let tokens = &t["tokens"];
        println!(
            "{} requests · in {} (cached {}) · out {}",
            t["requests"], tokens["prompt"], tokens["cached"], tokens["out"]
        );
        let asides: Vec<Value> = self.session.entries.iter().filter(|e| e["meta"]["aside"] == true).cloned().collect();
        if !asides.is_empty() {
            let a = crate::stats::totals(&asides);
            let tokens = &a["tokens"];
            println!(
                "of which {} for second looks at claims · in {} (cached {}) · out {}",
                a["requests"], tokens["prompt"], tokens["cached"], tokens["out"]
            );
        }
        println!(
            "{} runs · {} rejected before acting (syntax, unknown name, wrong arguments)",
            t["runs"], t["rejected"]
        );
    }

    pub fn think(&self) {
        let mut entries = self.session.entries.iter().enumerate().rev();
        let stored =
            entries.find_map(|(i, e)| e["reasoning_content"].as_str().filter(|r| !r.is_empty()).map(|r| (i, r)));
        let last = match (&self.discarded, stored) {
            (Some((at, lost)), stored) if stored.is_none_or(|(i, _)| *at > i) => Some(lost.as_str()),
            (_, stored) => stored.map(|(_, r)| r),
        };
        let mut markdown = crate::render::Markdown::default();
        markdown.push(last.unwrap_or("no reasoning recorded yet"));
        markdown.finish();
    }

    /// /compact: compacts the context now and says what it did.
    pub fn compact(&mut self) -> Result<()> {
        let done = self.compact_to(self.compact_at as usize / 5, false)?;
        ui::note(&done);
        self.session.flush()
    }

    /// Compacts the context and returns what it did, for /compact to show and for a compaction low decided on to log.
    fn compact_to(&mut self, keep_tokens: usize, whole: bool) -> Result<String> {
        let prev = self.session.keep();
        let keep = compact::cut(&self.session.entries, prev, keep_tokens, whole);
        if keep <= prev {
            let kept = ui::tokens(keep_tokens as u64);
            return Ok(format!("nothing to compact: the conversation fits in the {kept} compaction keeps"));
        }
        let summary = compact::summary(&self.session.entries, keep);
        let done = format!("compacted entries #1-#{} into a {} byte summary", keep - 1, summary.len());
        self.session.push(json!({"compact": {"keep": keep, "summary": summary, "at": self.compact_at}}))?;
        Ok(done)
    }
}

struct Group(Vec<i32>);

impl Drop for Group {
    fn drop(&mut self) {
        ui::CHECK.store(0, Ordering::Relaxed);
        worker::kill_groups(self.0.iter().copied());
    }
}

/// A provider must have a usable base_url before a session record names it.
fn web(name: &str, base_url: &str) -> Result<()> {
    anyhow::ensure!(
        crate::providers::web_url(base_url),
        "provider {name}: base_url {} is not an http(s) URL (/providers changes it)",
        ui::printable(base_url)
    );
    Ok(())
}

/// `provider:model` naming a configured provider: that provider and the model id. Anything else, such as an id with a
/// colon of its own (qwen3:8b), is a plain model id.
fn qualified<'a>(providers: &Providers, model: &'a str) -> Option<(String, &'a str)> {
    model.split_once(':').filter(|(provider, _)| providers.contains_key(*provider)).map(|(p, m)| (p.to_string(), m))
}

/// The provider that lists a model.
fn holding<'a>(providers: &'a Providers, model: &str) -> Option<&'a String> {
    providers.iter().find(|(_, p)| p.models.iter().any(|m| m == model)).map(|(name, _)| name)
}

fn connect(providers: &Providers, session: &Session) -> Result<(String, Llm)> {
    let header = session.header();
    let model = header["model"].as_str().unwrap_or_default();
    let recorded = header["provider"].as_str();
    let listed = holding(providers, model).map(String::as_str);
    // A provider that lists models, not one there only for search() (such as [deepseek]), unless none does.
    let serving = providers.iter().find(|(_, p)| !p.models.is_empty()).map(|(name, _)| name.as_str());
    let fallback = serving.or(providers.keys().next().map(String::as_str));
    let name = recorded
        .filter(|n| providers.contains_key(*n))
        .or(listed)
        .or(fallback)
        .with_context(|| format!("no provider: {}", crate::providers::example(model)))?;
    match recorded {
        Some(old) if old != name => {
            ui::note(&format!("provider {old} is gone; sending {model} to {name} (/model or /providers changes it)"))
        }
        None if listed.is_none() && providers.len() > 1 => {
            ui::note(&format!("no provider lists {model}; using {name} (/model or /providers changes it)"))
        }
        _ => {}
    }
    let p = &providers[name];
    web(name, &p.base_url)?;
    let url = llm::endpoint(&p.base_url);
    let extra = json!({"provider": name, "model": model, "base_url": url, "key": crate::providers::key(p)});
    ui::record("connect", &format!("{model} on {name} at {url}"), extra);
    let llm = Llm::new(&p.base_url, p.api_key.as_deref(), header);
    llm.recall(&session.entries);
    Ok((name.into(), llm))
}

/// The working tree as a git tree id, through the private index `index`. Git runs on the blocking pool, so Ctrl-C and
/// the status bar stay live while a large repo is snapshotted.
pub async fn tree_at(root: &Path, index: &Path, repo: bool) -> Result<String> {
    // Outside a git repository there is no snapshot: only edit() and write() show what changed.
    if !repo {
        return Ok(String::new());
    }
    let _busy = ui::busy("indexing the working tree");
    let (root, index) = (root.to_path_buf(), index.to_path_buf());
    tokio::task::spawn_blocking(move || snapshot(&root, &index)).await?
}

/// One run of `check` in the sandbox, on the tree `before`: whether it passed and left the tree as it was, its output
/// ending in low's verdict line, and the tree after it.
pub async fn trial(
    root: &Path,
    index: &Path,
    repo: bool,
    yolo: bool,
    check: &str,
    before: &Result<String>,
    timeout: Duration,
) -> (bool, String, Result<String>) {
    let script = format!("exec 2>&1\n{check}");
    let cmd = worker::sandboxed(root, yolo, "/bin/sh", &["-c", &script]);
    let (passed, out) = match tokio::time::timeout(timeout, grouped(cmd)).await {
        Ok(Ok((passed, out))) => (passed, redact(&out).0),
        Ok(Err(e)) => (false, format!("{e:#}")),
        Err(_) => (false, format!("timeout after {}s", timeout.as_secs())),
    };
    let after = tree_at(root, index, repo).await;
    let why = match (before, &after) {
        (Err(e), _) | (_, Err(e)) => format!("{e:#}"),
        (Ok(a), Ok(b)) if a != b => "the check changed the working tree".into(),
        _ => redact(check).0,
    };
    let ok = passed && matches!((before, &after), (Ok(a), Ok(b)) if a == b);
    let verdict = if ok { "passed" } else { "FAILED" };
    let out = format!("{}\n[check {verdict}: {why}]", out.trim_end()).trim_start().to_string();
    (ok, out, after)
}

async fn grouped(mut cmd: tokio::process::Command) -> Result<(bool, String)> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).process_group(0).spawn()?;
    let pid = child.id().context("the check exited before it started")?.cast_signed();
    ui::CHECK.store(pid, Ordering::Relaxed);
    let mut group = Group(vec![pid]);
    let mut stdout = child.stdout.take().context("the check's output")?;
    // The output is read as it comes and only its end is kept, so a check that prints without end fills no disk.
    let (mut kept, mut size, mut buf, mut open) = (Vec::new(), 0, vec![0; 1 << 16], true);
    let mut keep = |kept: &mut Vec<u8>, chunk: &[u8]| {
        size += chunk.len() as u64;
        kept.extend_from_slice(chunk);
        if kept.len() as u64 > 2 * CHECK_OUT {
            kept.drain(..kept.len() - CHECK_OUT as usize);
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            read = stdout.read(&mut buf), if open => match read {
                Ok(n @ 1..) => keep(&mut kept, &buf[..n]),
                _ => open = false,
            },
            _ = tick.tick() => {
                // ps blocks for tens of milliseconds: off the runtime's one thread, so the display and Ctrl-C keep up.
                let seen = tokio::task::spawn_blocking(move || worker::descendants(pid)).await.unwrap_or_default();
                for g in seen {
                    if !group.0.contains(&g) {
                        group.0.push(g);
                    }
                }
            }
        }
    };
    drop(group);
    // What the check printed last, and its processes before low stopped them; one that slipped away is not waited for.
    let rest = async {
        while let Ok(n @ 1..) = stdout.read(&mut buf).await {
            keep(&mut kept, &buf[..n]);
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), rest).await;
    let tail = &kept[kept.len().saturating_sub(CHECK_OUT as usize)..];
    let out = String::from_utf8_lossy(tail).into_owned();
    let out = if size > CHECK_OUT { format!("[… {} bytes omitted]\n{out}", size - CHECK_OUT) } else { out };
    Ok((status.success(), out))
}

/// The exit status for an error, as low gives it for a turn's outcome: see outcome().
pub const ERROR: i64 = 3;

/// The reasoning efforts /effort and --effort set, sent as the request's reasoning_effort. DeepSeek knows low, high
/// and max, and runs medium and xhigh as high; other providers may tell them apart.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// What a turn came to and the exit status low gives it, the same for a one-shot run, low --send and --tasks:
/// 0 done, 1 not verified, 4 a question waiting for its answer, 5 stopped short. 2 is a bad argument and 3 an error
/// (ERROR).
fn outcome(good: bool, stopped: bool, question: bool) -> (&'static str, i64) {
    match (stopped, good) {
        _ if question => ("question", 4),
        (true, _) => ("stopped", 5),
        (false, true) => ("done", 0),
        (false, false) => ("unverified", 1),
    }
}

/// The result of a task that ended in an error: the turn's report if one was written, marked as the error.
pub fn failed(last: Option<&Outcome>, e: &anyhow::Error) -> Outcome {
    let turn = last.and_then(|outcome| outcome.turn.clone());
    Outcome { turn, status: "error".into(), exit: ERROR, error: Some(error_text(e)) }
}

/// A task's result as `low --json`, `low --send` and `--tasks` give it: how it ended, and its turn's report when one was
/// written (an error can come before any, and then the result is only status, exit and error).
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    #[serde(flatten)]
    pub turn: Option<TurnReport>,
    pub status: String,
    pub exit: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl std::fmt::Display for Outcome {
    /// The result as one line of JSON.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}

/// What a turn's report gives a result: the verdict, the final answer or question, and the report's rows, including
/// the ones the screen leaves out because the turn already showed them.
#[derive(Clone, Serialize, Deserialize)]
pub struct TurnReport {
    pub session: PathBuf,
    pub id: String,
    pub verdict: String,
    pub good: bool,
    pub answer: Option<String>,
    pub question: Option<String>,
    pub criteria: Option<String>,
    pub plan: Value,
    pub check: String,
    /// What a second look made of the report's claims, in a turn that changed files with no check passing.
    pub claims: Option<Claims>,
    pub changes: Vec<String>,
    /// Git repositories the turn made inside the tree, which git would run code from outside the sandbox.
    pub repos: Vec<String>,
    pub elsewhere: usize,
    pub limit: Option<String>,
    pub undo: Option<String>,
    pub requests: u64,
    pub tokens: Tokens,
    pub cost: Option<f64>,
    pub ms: u64,
    pub rows: Vec<(String, String)>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub prompt: u64,
    pub cached: u64,
    pub out: u64,
}

/// A turn's `{"report": …}` line in the session log, which the next turn, compaction and the run's totals read back.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    pub verdict: String,
    /// The tree at the end of the turn, which the next turn compares with its own start.
    pub tree: Option<String>,
    pub changed: bool,
    pub checked: bool,
    pub stopped: bool,
    pub check_inputs: Vec<String>,
    pub claims: Option<Claims>,
    pub outside: Vec<String>,
    pub elsewhere: usize,
    pub criteria: Option<String>,
    pub plan_steps: Option<usize>,
    pub plan_done: usize,
    pub changes: Vec<String>,
    pub requests: u64,
    pub runs: u64,
    pub prompt: u64,
    pub cached: u64,
    pub out: u64,
    pub ms: u64,
}

impl Report {
    /// The report a session log entry holds, if it is a report line.
    pub fn of(entry: &Value) -> Option<Self> {
        Self::deserialize(entry.get("report")?).ok()
    }
}

/// What a second look made of a report's claims: the line it gave each claim it checked, by verdict.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Claims {
    pub held: Vec<String>,
    pub contradicted: Vec<String>,
    pub uncertain: Vec<String>,
}

impl Claims {
    /// The verdict lines in a second look's answer (`held: …`, `contradicted: …`, `uncertain: …`, bulleted, numbered or
    /// bold), or None when it has none.
    fn of(answer: &str) -> Option<Self> {
        let mut claims = Self::default();
        for line in answer.lines() {
            let line =
                line.trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_digit() || "-*•.)`".contains(c));
            let Some((verdict, claim)) = line.split_once(':') else { continue };
            let claim = claim.trim_start_matches(['*', ' ']).trim().to_string();
            if claim.is_empty() {
                continue;
            }
            match verdict.trim_end_matches(['*', '`']).trim().to_lowercase().as_str() {
                "held" | "holds" => claims.held.push(claim),
                "contradicted" | "contradicts" => claims.contradicted.push(claim),
                "uncertain" => claims.uncertain.push(claim),
                _ => {}
            }
        }
        let any = !claims.held.is_empty() || !claims.contradicted.is_empty() || !claims.uncertain.is_empty();
        any.then_some(claims)
    }

    /// How many claims held, were contradicted or stayed uncertain, leaving out the verdicts no claim got.
    fn summary(&self) -> String {
        let counts =
            [(self.held.len(), "held"), (self.contradicted.len(), "contradicted"), (self.uncertain.len(), "uncertain")];
        let counts: Vec<String> =
            counts.iter().filter(|(n, _)| *n > 0).map(|(n, verdict)| format!("{n} {verdict}")).collect();
        counts.join(", ")
    }
}

/// The first LISTED items joined by `sep`, then how many more there are.
fn capped(items: &[String], sep: &str) -> String {
    let more = items.len().saturating_sub(LISTED);
    let shown = items[..items.len() - more].join(sep);
    if more > 0 { format!("{shown}{sep}and {more} more") } else { shown }
}

/// A turn report's check row and verdict. A check low detected says so, and so does one whose baseline failed.
fn verdict(
    changed: bool,
    checked: bool,
    configured: bool,
    inputs: bool,
    question: bool,
    detected: Option<&Detected>,
) -> (String, &'static str) {
    let from = detected.filter(|_| configured).map(|d| format!(" (detected from {})", d.from)).unwrap_or_default();
    let (check, verdict) = match (changed, checked, configured) {
        _ if question => ("not run: the model asked a question".into(), "QUESTION"),
        (false, _, _) => ("-".into(), "NO CHANGES"),
        (true, true, _) if inputs => ("passed, but this turn changed what it runs".into(), "UNVERIFIED"),
        (true, true, _) => (format!("passed on the final tree{from}"), "VERIFIED"),
        (true, false, false) => match detected.and_then(|d| Some((d.failed.last()?, &d.from))) {
            Some((failed, from)) => {
                let failed = redact(failed).0;
                (
                    format!(
                        "none: `{failed}`, detected from {from}, failed on the tree as found; set check in low.toml"
                    ),
                    "NO CHECK",
                )
            }
            None => ("none configured: set check in low.toml".into(), "NO CHECK"),
        },
        (true, false, true) => (format!("not passed on the final tree{from}"), "UNVERIFIED"),
    };
    (check, verdict)
}

/// What a turn has used so far: its nudges and retries, and the tokens it spent against its budget.
#[derive(Default)]
struct TurnState {
    /// The check gate asked the model to fix a failing check.
    nudged: bool,
    /// The last request overflowed the context window, and the context was compacted for it.
    overflowed: bool,
    /// Unusable replies in a row: lost, looping, empty or cut off.
    retries: usize,
    /// A reply was still unusable after its retries.
    stalled: bool,
    /// The model was asked about files it changed without edit() or write().
    strayed: bool,
    /// The model was asked about a git repository it made inside the tree.
    planted: bool,
    /// The model was asked about claims a second look found the code contradicts.
    refuted: bool,
    /// The turn went on with the profile's fallback provider.
    failed_over: bool,
    /// Tokens spent, cached input counting a quarter.
    spent: u64,
    /// The model was told the turn has used three quarters of its budget.
    warned: bool,
    /// The last run's code and result, hashed, and how many runs in a row had the same.
    last_run: u64,
    repeats: usize,
}

impl TurnState {
    /// Counts a run of `code` that gave `out`. A traceback names its run (<run 7>), so the number doesn't count.
    fn ran(&mut self, code: &str, out: &str) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::hash::DefaultHasher::new();
        code.hash(&mut hasher);
        let mut parts = out.split("<run ");
        parts.next().hash(&mut hasher);
        for part in parts {
            part.trim_start_matches(|c: char| c.is_ascii_digit()).hash(&mut hasher);
        }
        let run = hasher.finish();
        self.repeats = if self.repeats > 0 && run == self.last_run { self.repeats + 1 } else { 1 };
        self.last_run = run;
    }
}

/// What a reply counts against a budget: cached input is billed at a fraction of fresh input, so a long, well-cached
/// session isn't stopped for re-sending its own context.
pub fn weighted(meta: &Value) -> u64 {
    let usage = &meta["usage"];
    let attempts = meta["timing"]["attempts"].as_u64().unwrap_or(1).max(1);
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = ui::cached(usage).unwrap_or(0).min(prompt);
    (prompt - cached + cached / 4) * attempts + usage["completion_tokens"].as_u64().unwrap_or(0)
}

/// An error as low shows and logs it: the whole chain, without control characters or secrets.
pub fn error_text(e: &anyhow::Error) -> String {
    redact(&ui::printable(&format!("{e:#}"))).0
}

/// The folders among these changed paths and their parents that git would take for a repository: one with a .git entry,
/// or laid out as a bare repository (HEAD, objects/, refs/). The sandbox denies a new .git on macOS, but a bare layout
/// under another name is data to it, and git runs the hooks and config it finds there outside the sandbox.
fn repos(root: &Path, paths: &[String]) -> Vec<String> {
    let mut found: Vec<String> = vec![];
    for path in paths {
        let mut dir = Path::new(path.trim_end_matches('/'));
        while !dir.as_os_str().is_empty() {
            let full = root.join(dir);
            let bare = full.join("HEAD").is_file() && full.join("objects").is_dir() && full.join("refs").is_dir();
            let named = dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case(".git"));
            let shown = dir.to_string_lossy();
            if (bare || named || full.join(".git").exists()) && !found.iter().any(|f| *f == shown) {
                found.push(shown.into_owned());
            }
            dir = dir.parent().unwrap_or(Path::new(""));
        }
    }
    found
}

/// The URL of a fetch() that low makes itself, one on an OPEN host; None for what goes to the tools API.
fn direct<'a>(call: &'a (&'static str, Value, String)) -> Option<&'a str> {
    call.1["url"].as_str().filter(|url| call.0 == "fetch" && open(url))
}

/// A URL on an OPEN host, over https, with no credentials or port of its own.
fn open(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
            && u.host_str().is_some_and(|host| OPEN.contains(&host))
    })
}

/// A URL as fetch() compares it: without its fragment or a trailing slash.
fn bare(url: &str) -> &str {
    let url = url.split('#').next().unwrap_or(url);
    url.strip_suffix('/').unwrap_or(url)
}

fn quoted(path: &str) -> String {
    if path.chars().all(|c| c.is_ascii_alphanumeric() || "._/-+".contains(c)) {
        path.into()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// What a reader of the session log needs to know about this process: the build, git, the sandbox, the check.
fn context(root: &Path, config: &Config, yolo: bool) -> serde_json::Map<String, Value> {
    let rev = |args: &[&str]| git(root, None, args).ok().map(|out| out.trim().to_string()).filter(|s| !s.is_empty());
    let sandbox = match (yolo, cfg!(target_os = "macos")) {
        (true, _) => "none (--yolo)",
        (false, true) => "sandbox-exec",
        (false, false) => "bwrap",
    };
    let context = json!({
        "low": crate::VERSION,
        "pid": std::process::id(), "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "branch": rev(&["symbolic-ref", "--short", "-q", "HEAD"]),
        "head": rev(&["rev-parse", "-q", "--verify", "HEAD"]),
        "sandbox": sandbox, "env": config.env,
        "check": config.check.as_deref().map(|c| redact(c).0), "fmt": config.fmt.as_deref().map(|f| redact(f).0),
    });
    let mut context = context.as_object().cloned().unwrap_or_default();
    // Where a detected check came from, and what failed on the tree as found: a resumed session keeps that decision.
    if let Some(detected) = &config.detected {
        context.insert("check_from".into(), detected.from.clone().into());
        context.insert("check_failed".into(), detected.failed.iter().map(|c| redact(c).0).collect());
    }
    context
}

/// A resumed session keeps the check its start detected, as its system prompt says, unless low.toml now names one or
/// turns detection off; the tree may have moved since, so its baseline's pass isn't reused.
fn adopt(config: &mut Config, header: &Value) {
    let Some(from) = header["check_from"].as_str().filter(|_| config.detect && config.check.is_none()) else { return };
    config.check = header["check"].as_str().map(String::from);
    let failed = header["check_failed"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(String::from));
    let detected = Detected { from: from.into(), check: config.check.clone(), failed: failed.collect(), passed: None };
    config.detected = Some(detected);
}

/// The lines of `changes` (as `changes()` writes them) that this turn made: paths it wrote with edit() or write(), and
/// files that last changed while one of its runs or checks was executing (for a deleted file, its folder). The rest
/// came from another writer in the same tree, such as the user's editor or another low.
fn attributed(root: &Path, turn: &[Value], changes: &str) -> String {
    let actions = turn.iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten());
    let own: HashSet<&str> = actions.filter(|a| a["ok"] == true).filter_map(|a| a["path"].as_str()).collect();
    // A run's result is logged as it ends, with its duration; a check from the gate is logged the same way.
    let windows: Vec<(u64, u64)> = turn
        .iter()
        .filter_map(|e| {
            let (at, ms) =
                if e["role"] == "tool" { (&e["meta"]["at"], &e["meta"]["ms"]) } else { (&e["at"], &e["check"]["ms"]) };
            let (at, ms) = (at.as_u64()?, ms.as_u64()?);
            Some((at.saturating_sub(ms + SLACK_MS), at + SLACK_MS))
        })
        .collect();
    let during = |path: &Path| {
        let time = path.symlink_metadata().and_then(|m| m.modified()).ok();
        let ms = time.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64);
        ms.is_some_and(|ms| windows.iter().any(|&(from, to)| (from..=to).contains(&ms)))
    };
    let lines: Vec<&str> = changes
        .lines()
        .filter(|line| {
            let Some(path) = changed_path(line) else { return false };
            let full = root.join(&path);
            own.contains(path.as_str())
                || if full.symlink_metadata().is_ok() { during(&full) } else { full.parent().is_some_and(during) }
        })
        .collect();
    if lines.is_empty() { "none".into() } else { lines.join("\n") }
}

/// What edit() and write() changed in a turn, listed as changes() lists it: outside a git repository that is all low
/// can see.
fn own_changes(turn: &[Value]) -> String {
    let mut files: BTreeMap<&str, (u64, u64, bool)> = BTreeMap::new();
    for action in turn.iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten()) {
        if let Some(path) = action["path"].as_str().filter(|_| action["ok"] == true) {
            let file = files.entry(path).or_default();
            file.0 += action["added"].as_u64().unwrap_or(0);
            file.1 += action["removed"].as_u64().unwrap_or(0);
            file.2 |= action["tool"] == "write" && action["replaced"] != true;
        }
    }
    let lines: Vec<String> = files
        .iter()
        .map(
            |(path, (added, removed, new))| {
                if *new { format!("{path} (new)") } else { format!("{path} +{added} −{removed}") }
            },
        )
        .collect();
    if lines.is_empty() { "none".into() } else { lines.join("\n") }
}

/// The path in one of changes()'s lines.
fn changed_path(line: &str) -> Option<String> {
    let marks = [" (new)", " (deleted)", " (binary)", " (ignored by git)"];
    let marked = marks.iter().find_map(|mark| line.strip_suffix(mark));
    marked.or_else(|| line.rsplit_once(" +").map(|(path, _)| path)).map(String::from)
}

fn edited(meta: &Value) -> bool {
    meta["actions"].as_array().is_some_and(|a| a.iter().any(|a| a["ok"] == true && a["path"].is_string()))
}

/// Whether anything since a run's last check() (or its start) may have changed files: an edit, a write, or any shell
/// command, which can touch gitignored files the snapshot never sees. Writes from Python itself go unseen.
fn stale(meta: &Value) -> bool {
    let actions = meta["actions"].as_array().map_or(&[][..], Vec::as_slice);
    let check = actions.iter().rposition(|a| a["tool"] == "check");
    let (after, ran) = check.map_or((actions, 0), |i| (&actions[i + 1..], actions[i]["cmds"].as_u64().unwrap_or(0)));
    let cmds = meta["cmds"].as_array().map_or(0, Vec::len);
    after.iter().any(|a| a["ok"] == true && a["path"].is_string()) || cmds > ran as usize
}

/// What one of a turn's runs did, for a second look: its edits and writes, its shell commands with their exit status,
/// and its check()s. None for a run that did none of these.
fn did(meta: &Value) -> Option<String> {
    let actions = meta["actions"].as_array().map_or(&[][..], Vec::as_slice);
    let mut did: Vec<String> = actions
        .iter()
        .filter_map(|a| {
            let failed = if a["ok"] == true { "" } else { " (failed)" };
            Some(format!("{} {}{failed}", a["tool"].as_str()?, a["path"].as_str()?))
        })
        .collect();
    for cmd in meta["cmds"].as_array().into_iter().flatten() {
        let line: String = ui::printable(cmd[0].as_str().unwrap_or_default()).trim().chars().take(200).collect();
        did.push(format!("$ {line} → exit {}", cmd[1]));
    }
    for check in actions.iter().filter(|a| a["tool"] == "check") {
        did.push(format!("check() {}", if check["ok"] == true { "passed" } else { "failed" }));
    }
    (!did.is_empty()).then(|| did.join(" · "))
}

/// Runs a second look's code in its read-only worker, started on first use, and shows it as any run is shown. Its
/// check(), search() and fetch() are turned down.
async fn look_run(worker: &mut Option<Worker>, root: &Path, yolo: bool, code: &str) -> (String, Value) {
    ui::run(code);
    let clock = Instant::now();
    let ran = async {
        let started = match worker.take() {
            Some(started) => started,
            None => Worker::reader(root, yolo)?,
        };
        let worker = worker.insert(started);
        let mut reply = worker.run(code, &json!({})).await?;
        while reply["needs_check"] == true {
            reply = worker.resume(false, LOOK_OFF).await?;
        }
        anyhow::Ok(reply)
    };
    match ran.await {
        Ok(reply) => {
            let (out, mut meta) = finish(reply);
            meta["ms"] = ui::ms(clock).into();
            ui::ran(&meta, &out);
            (out, meta)
        }
        Err(e) => {
            *worker = None;
            let out = format!("error: {e:#}; the worker was restarted and its variables are gone");
            ui::show(&out);
            (out, json!({"error": true}))
        }
    }
}

fn finish(mut meta: Value) -> (String, Value) {
    let (out, mut hidden) = redact(meta["out"].as_str().unwrap_or_default());
    if let Some(fields) = meta.as_object_mut() {
        fields.remove("out");
    }
    for cmd in meta["cmds"].as_array_mut().into_iter().flatten().filter_map(|c| c.get_mut(0)) {
        if let Some(text) = cmd.as_str() {
            let (clean, n) = redact(text);
            if n > 0 {
                (*cmd, hidden) = (clean.into(), hidden + n);
            }
        }
    }
    if hidden > 0 {
        meta["redacted"] = hidden.into();
    }
    meta["tool"] = "run".into();
    (out, meta)
}

/// Today's date in the user's time zone, as YYYY-MM-DD: a task carries it, so the model knows what "recent" means for
/// search() and the code, while the system prompt, written once per session, stays the same and cached.
fn today() -> Option<String> {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r fills tm whenever it returns non-null.
    if unsafe { libc::localtime_r(&now, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some(format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday))
}

fn system_prompt(root: &Path, yolo: bool, config: &Config, repo: bool, search: bool) -> String {
    let mut prompt = include_str!("prompt.md").to_string();
    if !search {
        prompt = prompt.replace(WEB, &WEB_OPEN.replace("HOSTS", &OPEN.join(", ")));
    }
    if !repo {
        prompt = prompt.replace(UNDO, "");
    }
    let from = config.detected.as_ref().map(|d| d.from.as_str());
    prompt += &match (config.check.as_deref(), from) {
        (Some(check), Some(from)) => {
            format!(
                "\ncheck() runs `{}`, which low detected from {from}; it passed on the tree as found.",
                redact(check).0
            )
        }
        (Some(check), None) => format!("\ncheck() runs `{}`.", redact(check).0),
        (None, _) => "\nNo check command is configured, so check() runs nothing.".into(),
    };
    let read_only = !yolo && worker::read_only(root);
    if read_only {
        prompt += "\n\nThis folder is the user's home directory or above it: you can read files here but not change them. \
                   Put scratch files in a temp dir.";
    } else if !repo {
        prompt += "\n\nThis folder is not a git repository, so nothing you change can be undone: change only what the task \
                   needs. Git commands don't work here; list files with pathlib.";
    }
    if !yolo {
        prompt += SANDBOX;
        prompt += if cfg!(target_os = "macos") { MACOS } else { LINUX };
        prompt += PERMANENT;
        prompt += REGISTRIES;
    }
    // One that is there but can't be read is noted, and the next one tried.
    for name in INSTRUCTIONS {
        if let Some((text, size)) = repo_text(root, name, AGENTS_MAX) {
            prompt += &format!("\n\n# Project instructions ({name})\n\n{text}");
            if size > AGENTS_MAX as u64 {
                let (kept, all) = (AGENTS_MAX / 1024, size.div_ceil(1024));
                prompt +=
                    &format!("\n\n[{name} clipped to its first {kept} KB of {all} KB; read the file for the rest]");
            }
            break;
        }
        if std::fs::symlink_metadata(root.join(name)).is_ok() {
            let why = "outside the repo, sandbox-protected, unreadable, or not a regular file";
            ui::note(&format!("left out of the prompt ({why}): {name}"));
        }
    }
    prompt += &listing(root);
    prompt
}

/// The tracked files for the system prompt: a path and size per line within LISTING bytes, else a line per folder
/// within TREE bytes; past both, a repo of over FILES files shows its first FILES two-level folders with file counts.
/// Logs which form the model got and its size.
fn listing(root: &Path) -> String {
    let tracked = git(root, None, &["ls-files"]).unwrap_or_default();
    let files: Vec<&str> = tracked.lines().collect();
    let sizes: Vec<u64> = files.iter().map(|f| std::fs::metadata(root.join(f)).map_or(0, |m| m.len())).collect();
    let plain: String = files.iter().zip(&sizes).map(|(file, size)| format!("{file} {size}\n")).collect();
    let head = |how: &str| format!("\n\n# Repository files at session start ({} tracked, {how})\n", files.len());
    let shown = |form: &str, text: String| {
        let (n, bytes) = (files.len(), text.len());
        let size = format!("{:.1} KB, ~{}", bytes as f64 / 1024.0, ui::tokens(bytes as u64 / 4));
        let extra = json!({"files": n, "form": form, "bytes": bytes});
        ui::record("listing", &format!("repo map for the model: {n} tracked files as {form} ({size})"), extra);
        text
    };
    if files.is_empty() {
        return String::new();
    }
    if plain.len() <= LISTING {
        return shown("a list with sizes", head("size in bytes") + &plain);
    }
    // Each folder's own files, as indexes into files, and its subfolders.
    let mut dirs = BTreeMap::<&str, (Vec<usize>, Vec<&str>)>::new();
    for (i, file) in files.iter().enumerate() {
        let mut parent = "";
        for (j, _) in file.match_indices('/') {
            let dir = &file[..=j];
            if !dirs.contains_key(dir) {
                dirs.entry(parent).or_default().1.push(dir);
            }
            dirs.entry(dir).or_default();
            parent = dir;
        }
        dirs.entry(parent).or_default().0.push(i);
    }
    let tree: String = dirs
        .iter()
        .map(|(dir, (own, subs))| {
            let own = own.iter().map(|&i| format!("{} {}K", &files[i][dir.len()..], sizes[i].div_ceil(1024)));
            let names: Vec<String> = own.chain(subs.iter().map(|sub| sub[dir.len()..].to_string())).collect();
            format!("{}: {}\n", if dir.is_empty() { "./" } else { dir }, names.join(", "))
        })
        .collect();
    if tree.len() <= TREE {
        return shown(
            "a folder tree",
            head("sizes in KB; one line per folder: its files, then its subfolders") + &tree,
        );
    }
    if files.len() <= FILES {
        return shown("a list with sizes", head("size in bytes") + &plain);
    }
    let mut counts = BTreeMap::new();
    for file in &files {
        let key = file.match_indices('/').nth(1).map_or(*file, |(j, _)| &file[..=j]);
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut out = format!("\n\n# Repository at session start ({} tracked files; folders show counts)\n", files.len());
    for (path, n) in counts.iter().take(FILES) {
        out += &if path.ends_with('/') { format!("{path} ({n} files)\n") } else { format!("{path}\n") };
    }
    let first =
        if counts.len() > FILES { format!(", the first {FILES} of {} entries", counts.len()) } else { String::new() };
    shown(&format!("folder counts{first}"), out)
}

/// A file for the system prompt, if it resolves to a regular file inside the repo and outside the sandbox's secret
/// paths: at most `max` bytes, not ending mid-character, with the file's full size.
fn repo_text(root: &Path, rel: &str, max: usize) -> Option<(String, u64)> {
    let path = root.join(rel).canonicalize().ok().filter(|p| p.starts_with(root) && !worker::secret(p))?;
    file_text(&path, max)
}

/// A resolved regular file's first `max` bytes, cut at a character, and its size; not through a link at the file itself.
fn file_text(path: &Path, max: usize) -> Option<(String, u64)> {
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(flags).open(path).ok()?;
    let size = file.metadata().ok().filter(std::fs::Metadata::is_file)?.len();
    let mut bytes = Vec::new();
    file.take(max as u64).read_to_end(&mut bytes).ok()?;
    let partial = bytes.utf8_chunks().last().map_or(0, |chunk| {
        let tail = chunk.invalid();
        if std::str::from_utf8(tail).is_err_and(|e| e.error_len().is_none()) { tail.len() } else { 0 }
    });
    bytes.truncate(bytes.len() - partial);
    Some((String::from_utf8_lossy_owned(bytes), size))
}

fn tools() -> Value {
    json!([{"type": "function", "function": {"name": "run",
        "description": "Run Python in the persistent worker; only printed output comes back.",
        "parameters": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}}}])
}

#[cfg(test)]
mod tests {
    /// The prompt names the web helpers in exactly the text WEB removes when search is off, and every OPEN host in it;
    /// undo() in the text UNDO removes outside a git repository.
    #[test]
    fn web_lines_match_the_prompt() {
        assert_eq!(include_str!("prompt.md").matches(super::WEB).count(), 1);
        assert_eq!(include_str!("prompt.md").matches(super::UNDO).count(), 1);
        for host in super::OPEN {
            assert!(super::WEB.contains(host), "{host} is open but the prompt doesn't name it");
        }
    }

    /// A second look's verdict lines count however they are marked up, and lines that name no verdict, or no claim,
    /// don't.
    #[test]
    fn claims_of_a_second_look() {
        let answer = "Checked 3 claims.\n- held: add() adds — calc.py:2\n**Contradicted**: sub() is unchanged — calc.py:5 \
                      adds now\n2. uncertain: `cargo test` passes — no run after the last edit\nheld:\nnote: none";
        let claims = super::Claims::of(answer).unwrap();
        assert_eq!(claims.held, ["add() adds — calc.py:2"]);
        assert_eq!(claims.contradicted, ["sub() is unchanged — calc.py:5 adds now"]);
        assert_eq!(claims.uncertain, ["`cargo test` passes — no run after the last edit"]);
        assert_eq!(claims.summary(), "1 held, 1 contradicted, 1 uncertain");
        assert!(super::Claims::of("All good: the change is right.").is_none());
        let held = super::Claims::of("**held:** it builds — Cargo.toml:3").unwrap();
        assert_eq!((held.summary(), held.held), ("1 held".to_string(), vec!["it builds — Cargo.toml:3".to_string()]));
    }

    /// The e2e mock tells a second look's requests by LOOK's opening words.
    #[test]
    fn mock_knows_a_second_look() {
        let mock = include_str!("../tests/e2e/mock.py");
        let words = mock.split("\nLOOK = '").nth(1).and_then(|rest| rest.split('\'').next()).unwrap_or_default();
        assert!(!words.is_empty() && super::LOOK.starts_with(words), "mock.py's LOOK is {words:?}");
    }

    /// Any https URL on an OPEN host is fetchable; one on another host, with credentials or a port, or not https is not.
    #[test]
    fn open_hosts_only() {
        assert!(super::open("https://registry.npmjs.org/three/latest"));
        assert!(super::open("https://API.github.com/repos/mrdoob/three.js/releases/latest"));
        assert!(super::open("https://cdn.jsdelivr.net:443/npm/three@0.186.1/build/three.module.js"));
        for url in [
            "http://registry.npmjs.org/three",
            "https://registry.npmjs.org.evil.example/three",
            "https://evil.example/?u=https://registry.npmjs.org/",
            "https://user:pass@pypi.org/pypi/requests/json",
            "https://pypi.org:8443/pypi/requests/json",
            "https://github.com/mrdoob/three.js/releases",
            "registry.npmjs.org/three",
        ] {
            assert!(!super::open(url), "{url}");
        }
    }
}
