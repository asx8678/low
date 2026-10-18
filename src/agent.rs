use crate::git::{changes, git, guard_git, snapshot};
use crate::llm::{self, Llm};
use crate::providers::Providers;
use crate::redact::redact;
use crate::session::Session;
use crate::worker::{self, Worker};
use crate::{Config, compact, ui};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read};
use tokio::io::AsyncReadExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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
/// How long a session log stays a candidate for another low working in this repository.
const DAY: Duration = Duration::from_secs(86_400);
/// Margin around a run's recorded start and end when matching file times to it.
const SLACK_MS: u64 = 250;
const GATE: &str = "The project check failed on the current tree. Fix what fails, run check(), then finish again. \
                    If it cannot pass, say why in one line. The check output ended with:";
const RETRY: &str = "Send one run() now with the next step, or a shorter answer.";
const STRAY: &str = "This turn changed files without edit() or write():";
const STRAY_END: &str = "Delete the ones that are scratch output from running code, keep any that are part of your answer, \
                         then finish again.";
const PLANTED: &str = "This turn made a git repository inside the tree:";
const PLANTED_END: &str = "The user's git would run hooks and config from there, outside the sandbox. Delete it if it is \
                           scratch, or say in your final message why it belongs, then finish again.";
const HELPERS: &[&str] =
    &["sh", "show", "accept", "edit", "write", "check", "plan", "done", "finish", "ask", "search", "fetch"];
/// The helpers that reach the web, in the prompt only when config.toml's search names a provider.
const WEB: &str = "search(query, count=10) -> list  # web search: [{title, url, summary, date, site_name}]; site:, \"phrase\", \
                   after:2025 work\nfetch(url) -> str             # a web page as markdown\n";
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
    /// Whether the root is a git repository; outside one there are no snapshots and no undo.
    repo: bool,
    /// The last turn's result: its verdict, report rows and final answer.
    last: Option<Outcome>,
    /// The question an ask() ended the turn with, for its report.
    question: Option<String>,
    /// A file low itself rewrites between turns (a --tasks checklist), relative to the root: not news to the model.
    quiet: Option<String>,
    /// The limit that stopped the current turn, for its result: a --tasks run continues such a task.
    limit: Option<String>,
}

impl Agent {
    pub fn new(
        root: &Path,
        home: &Path,
        config: Config,
        resume: bool,
        id: Option<&str>,
        yolo: bool,
        repo: bool,
    ) -> Result<Self> {
        guard_git(root);
        let context = context(root, &config, yolo);
        let session = if resume || id.is_some() {
            Session::resume(home, root, context, id)?
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
            let system = system_prompt(root, yolo, config.check.as_deref(), repo, config.search.is_some());
            Session::create(home, root, provider, &model, &system, profile, context)?
        };
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
            repo,
            last: None,
            question: None,
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
        let (provider, model) = match qualified(&self.config.providers, model) {
            Some((provider, id)) => (provider, id),
            None => (holding(&self.config.providers, model).cloned().unwrap_or_else(|| self.provider.clone()), model),
        };
        if provider == self.provider && model == self.model() {
            return Ok(());
        }
        self.switch(&provider, model)
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
        ui::event(
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
        for key in ["compact_at", "keep_warm", "turn_budget", "fallback"] {
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
        ui::note(&format!("reasoning effort {effort} for {model}"));
        Ok(())
    }

    pub fn switch(&mut self, provider: &str, model: &str) -> Result<()> {
        anyhow::ensure!(self.config.providers.contains_key(provider), "no provider named {provider}");
        web(provider, &self.config.providers[provider].base_url)?;
        let profile = self.config.models.get(model).cloned().unwrap_or_default();
        let web = self.config.search.is_some();
        let system = system_prompt(&self.root, self.yolo, self.config.check.as_deref(), self.repo, web);
        self.session.switch(provider, model, &system, profile)?;
        (self.provider, self.llm) = connect(&self.config.providers, &self.session)?;
        self.compact_at = self.session.header()["compact_at"].as_u64().unwrap_or(COMPACT_AT);
        ui::note(&format!("now using {model} on {provider}"));
        Ok(())
    }

    pub async fn run(&mut self, task: &str) -> Result<bool> {
        self.last = None;
        let _bar = ui::Turn::start();
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
        self.unfinished(false).await?;
        Ok(false)
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
        (self.pending, self.restarts, self.broken, self.passed) = (None, 0, None, None);
        self.session.flush()?;
        let clock = Instant::now();
        let why = "low's change detection (a private git index, not your staging area) could not snapshot the tree";
        let start = self.tree().await.context(why)?;
        let snapshot_ms = ui::ms(clock);
        self.session
            .push(json!({"role": "user", "content": task, "meta": {"snapshot": start, "snapshot_ms": snapshot_ms}}))?;
        // The push first writes what the snapshot logged, so the turn begins at the task itself.
        let began = self.session.entries.len() - 1;
        self.current = Some((began, clock));
        if let Some(moved) = self.moved_since(&start).await {
            ui::event("moved", &moved, Value::Null);
            self.session.push(json!({"role": "user", "content": format!("[{moved}]"), "meta": {"host": true}}))?;
        }
        let mut state = TurnState::default();
        let budget = self.session.header()["turn_budget"].as_u64().unwrap_or(TURN_BUDGET);
        let ceiling = self.compact_at * budget;
        let mut limit = format!("stopped after {MAX_REQUESTS} model requests in one turn");
        for _ in 0..MAX_REQUESTS {
            if state.spent >= ceiling {
                let rule = "cached input counts a quarter";
                limit =
                    format!("stopped after {} in one turn ({budget} × compact_at; {rule})", ui::tokens(state.spent));
                break;
            }
            if self.upcoming() >= self.compact_at {
                self.compact()?;
            }
            let Some(message) = self.request(&mut state).await? else { continue };
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            self.session.push(message)?;
            let (finished, asked) = self.act(began, &calls).await?;
            // The model waits for an answer, which comes as the next task: no nudges or check gate before it.
            if let Some(question) = asked {
                self.reply(&question, "ask()")?;
                self.question = Some(question);
                let changed = self.changed(&start, began).await;
                self.current = None;
                return self.report(Some(changed), false, false, began, clock).await;
            }
            if let Some(report) = finished {
                self.reply(&report, "finish()")?;
            } else if !calls.is_empty() {
                ui::activity("waiting for the model");
                continue;
            }
            let Some((changed, checked)) = self.gates(&mut state, &start, began).await? else { continue };
            self.current = None;
            return self.report(Some(changed), checked, state.stalled && !changed, began, clock).await;
        }
        ui::event("limit", &limit, Value::Null);
        (self.current, self.limit) = (None, Some(limit));
        let changed = self.changed(&start, began).await;
        self.report(Some(changed), false, true, began, clock).await
    }

    /// Asks the model for its next reply. What can be retried is handled here: a context overflow (compact and ask
    /// again), a provider out of credit (go on with the profile's fallback, once a turn), a lost stream, and an empty
    /// or cut-off reply (ask again, twice in a row at most). None when the turn asks again; otherwise the reply, counted
    /// against the turn's budget.
    async fn request(&mut self, state: &mut TurnState) -> Result<Option<Value>> {
        let result = self.llm.complete(self.session.messages(), &tools()).await;
        if let Some(lost) = result.as_ref().err().and_then(llm::lost) {
            self.discard("the provider cut the stream", &lost.thinking);
        }
        let mut message = match result {
            Err(e) if e.is::<llm::ContextOverflow>() && !state.overflowed => {
                state.overflowed = true;
                self.overflow(e.downcast_ref::<llm::ContextOverflow>().and_then(|o| o.window))?;
                return Ok(None);
            }
            Err(e) if e.is::<llm::ContextOverflow>() => {
                let system = self.session.header()["system"].as_str().map_or(0, str::len);
                let agents = repo_text(&self.root, "AGENTS.md", 0).map_or(0, |(_, size)| size as usize);
                anyhow::bail!(
                    "{e}; still over after compacting: the request is {} KB, of which the system prompt is {} KB \
                     (including {} KB of AGENTS.md)",
                    self.sent() * 4 / 1024,
                    system / 1024,
                    agents.min(AGENTS_MAX) / 1024
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
            result => result?,
        };
        state.overflowed = false;
        if !message["meta"]["usage"]["prompt_tokens"].is_u64() {
            let usage = &mut message["meta"]["usage"];
            usage["prompt_tokens"] = self.sent().into();
            usage["estimated"] = true.into();
        }
        let usage = &message["meta"]["usage"];
        let attempts = message["meta"]["timing"]["attempts"].as_u64().unwrap_or(1).max(1);
        // The budget follows what a request costs: cached input is billed at a fraction of fresh input, so a long,
        // well-cached session isn't stopped for re-sending its own context.
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let cached = ui::cached(usage).unwrap_or(0).min(prompt);
        state.spent += (prompt - cached + cached / 4) * attempts + usage["completion_tokens"].as_u64().unwrap_or(0);
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
    async fn act(&mut self, began: usize, calls: &[Value]) -> Result<(Option<String>, Option<String>)> {
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
        // another low in this tree its files can land inside our runs' windows too, so it is not asked about then.
        if changed && !state.strayed && !self.session.entries[began..].iter().any(|e| edited(&e["meta"])) {
            state.strayed = true;
            let paths: Vec<String> = self.own_diff(began).await?.1.lines().filter_map(changed_path).collect();
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
        Ok(Some((changed, checked)))
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
    async fn moved_since(&self, start: &str) -> Option<String> {
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
        let more = files.len().saturating_sub(20);
        let more = if more > 0 { format!(" and {more} more") } else { String::new() };
        let shown = files.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
        (!files.is_empty())
            .then(|| format!("Files changed since your last turn, outside this conversation: {shown}{more}"))
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

    /// Logs a request that found less than half of the previous request's prompt in the provider's cache, with the cause
    /// when low knows it: a compaction, a model switch, or a new task.
    fn cache_miss(&self, usage: &Value) {
        let entries = &self.session.entries;
        let Some(prev) = entries.iter().rposition(|e| e["meta"]["usage"]["prompt_tokens"].is_u64()) else { return };
        let expected = entries[prev]["meta"]["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let Some(cached) = ui::cached(usage).filter(|&c| expected >= 1024 && c * 2 < expected) else { return };
        let since = &entries[prev + 1..];
        let cause = if since.iter().any(|e| e["compact"].is_object()) {
            "compaction"
        } else if since.iter().any(|e| e["switch"].is_object()) {
            "model switch"
        } else if since.iter().any(|e| e["role"] == "user" && e["meta"]["host"].is_null()) {
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
        let compacted = entries.iter().rposition(|e| e["compact"].is_object()).map_or(0, |i| i + 1);
        let counted = (compacted..entries.len()).rev().find(|&i| entries[i]["meta"]["usage"]["prompt_tokens"].is_u64());
        let Some(i) = counted else { return self.sent() };
        let added = (i..entries.len()).filter(|&j| entries[j]["role"].is_string());
        let added: u64 = added.map(|j| compact::wire(&entries[j]).to_string().len() as u64).sum();
        entries[i]["meta"]["usage"]["prompt_tokens"].as_u64().unwrap_or(0) + added / 4
    }

    fn overflow(&mut self, window: Option<u64>) -> Result<()> {
        let (seen, estimate) = (self.session.last_prompt_tokens(), self.upcoming());
        // A fifth stays free: the reply needs room too, and the estimate counts bytes, not tokens.
        self.compact_at = self.compact_at.min(window.unwrap_or(estimate) * 4 / 5);
        self.compact_to(self.compact_at as usize / 5, true)?;
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
        ui::event("nudge", &content, Value::Null);
        self.session.push(json!({"role": "user", "content": content, "meta": {"host": true, "nudge": true}}))
    }

    async fn call(&mut self, call: &Value) -> (String, Value) {
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or_default()).unwrap_or_default();
        match (call["function"]["name"].as_str(), args["code"].as_str()) {
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
        let state = self.session.state();
        let state = json!({"accepted": state.accepted, "plan": state.plan});
        let clock = Instant::now();
        let ran = async {
            let mut reply = self.worker()?.run(code, &state).await?;
            while reply["needs_check"] == true {
                if reply["web"].is_object() {
                    let (ok, out) = self.web(&reply["web"]).await;
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

    /// A search() or fetch() from a run, made here since the sandbox has no network: DeepSeek's tools API, with the
    /// provider that config.toml's search names. The request leaves the machine, so low refuses one whose argument holds
    /// what looks like a secret, and fetch() only takes a URL that a search() returned or the user wrote: one the run made
    /// up could carry what it read.
    async fn web(&mut self, request: &Value) -> (bool, String) {
        let provider = self.config.search.as_ref().and_then(|name| self.config.providers.get(name));
        let Some((base, key)) = provider.map(|p| (p.base_url.clone(), p.api_key.clone())) else {
            return (false, "web search is not set up: config.toml's search names no provider".into());
        };
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
            return (false, format!("{tool}() refused: its argument holds what looks like a secret"));
        }
        if let Some(url) = request["fetch"].as_str() {
            // A search() earlier in this run is still queued for the log.
            let _ = self.session.flush();
            if !self.fetchable(url) {
                ui::record("web", &what, json!({"ok": false, "refused": "url"}));
                let why = "fetch() takes a URL that a search() in this session returned or that the task gives";
                return (false, format!("{why}; search() for the page first"));
            }
        }
        let result = llm::tool(&base, key.as_deref(), tool, &body).await;
        let (ok, out) = match result {
            Ok(text) => (true, text),
            Err(e) => (false, format!("{tool} failed: {e:#}")),
        };
        // The results' URLs are what a later fetch() may get, in this run or after a resume.
        let found = serde_json::from_str::<Value>(&out).ok().filter(|_| ok && tool == "search");
        let urls: Vec<&str> =
            found.iter().flat_map(|f| f["results"].as_array().into_iter().flatten()).filter_map(|r| r["url"].as_str()).collect();
        ui::record("web", &what, json!({"request": body, "ok": ok, "bytes": out.len(), "urls": urls}));
        (ok, out)
    }

    /// Whether fetch() may get this URL: a search() in this session returned it, or a task of the user's names it.
    fn fetchable(&self, url: &str) -> bool {
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
        let script = format!("exec 2>&1\n{check}");
        let cmd = worker::sandboxed(&self.root, self.yolo, "/bin/sh", &["-c", &script]);
        let (passed, out) = match tokio::time::timeout(CHECK_TIMEOUT, grouped(cmd)).await {
            Ok(Ok((passed, out))) => (passed, redact(&out).0),
            Ok(Err(e)) => (false, format!("{e:#}")),
            Err(_) => (false, format!("timeout after {}s", CHECK_TIMEOUT.as_secs())),
        };
        let after = self.tree().await;
        let why = match (&before, &after) {
            (Err(e), _) | (_, Err(e)) => format!("{e:#}"),
            (Ok(a), Ok(b)) if a != b => "the check changed the working tree".into(),
            _ => redact(check).0,
        };
        let ok = passed && matches!((&before, &after), (Ok(a), Ok(b)) if a == b);
        let verdict = if ok { "passed" } else { "FAILED" };
        let out = format!("{}\n[check {verdict}: {why}]", out.trim_end()).trim_start().to_string();
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
        // Outside a git repository there is no snapshot: only edit() and write() show what changed.
        if !self.repo {
            return Ok(String::new());
        }
        let _busy = ui::busy("indexing the working tree");
        let (root, index) = (self.root.clone(), self.session.index());
        tokio::task::spawn_blocking(move || snapshot(&root, &index)).await?
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
        let configured = self.config.check.is_some();
        let (check, verdict) = verdict(changed, checked, configured, !inputs.is_empty(), question.is_some());
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
        rows.push(("check", check.into()));
        if !inputs.is_empty() {
            rows.push(("inputs", format!("check inputs changed: {}", inputs.join(", "))));
        }
        rows.push(("changes", if changes.is_empty() { "none".into() } else { changes.join("\n") }));
        if elsewhere > 0 {
            rows.push(("elsewhere", format!("{elsewhere} more changed by another writer in this tree, left out")));
        }
        if !outside.is_empty() {
            let text = format!("not through low's edit()/write(), maybe outside low: {}", outside.join(", "));
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
            let text = format!("git repositories made inside the tree, which the sandbox cannot contain: {}", repos.join(", "));
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
            check: check.into(),
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
        let actions = self.session.entries[began..].iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten());
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

    pub fn compact(&mut self) -> Result<()> {
        self.compact_to(self.compact_at as usize / 5, false)
    }

    fn compact_to(&mut self, keep_tokens: usize, whole: bool) -> Result<()> {
        let prev = self.session.keep();
        let keep = compact::cut(&self.session.entries, prev, keep_tokens, whole);
        if keep <= prev {
            let kept = ui::tokens(keep_tokens as u64);
            ui::note(&format!("nothing to compact: the conversation fits in the {kept} compaction keeps"));
            return Ok(());
        }
        let summary = compact::summary(&self.session.entries, keep);
        ui::note(&format!("compacted entries #1-#{} into a {} byte summary", keep - 1, summary.len()));
        self.session.push(json!({"compact": {"keep": keep, "summary": summary, "at": self.compact_at}}))
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
                let seen = worker::descendants(pid);
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

/// What a turn's report gives a result: the verdict, the final answer or question, and the rows low printed.
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

/// A turn report's check row and verdict.
fn verdict(
    changed: bool,
    checked: bool,
    configured: bool,
    inputs: bool,
    question: bool,
) -> (&'static str, &'static str) {
    match (changed, checked, configured) {
        _ if question => ("not run: the model asked a question", "QUESTION"),
        (false, _, _) => ("-", "NO CHANGES"),
        (true, true, _) if inputs => ("passed, but this turn changed what it runs", "UNVERIFIED"),
        (true, true, _) => ("passed on the final tree", "VERIFIED"),
        (true, false, false) => ("none configured: set check in low.toml", "NO CHECK"),
        (true, false, true) => ("not passed on the final tree", "UNVERIFIED"),
    }
}

/// What a turn has used so far: its nudges and retries, and the tokens it spent against its budget.
#[derive(Default)]
struct TurnState {
    /// The check gate asked the model to fix a failing check.
    nudged: bool,
    /// The last request overflowed the context window, and the context was compacted for it.
    overflowed: bool,
    /// Unusable replies in a row: lost, empty or cut off.
    retries: usize,
    /// A reply was still unusable after its retries.
    stalled: bool,
    /// The model was asked about files it changed without edit() or write().
    strayed: bool,
    /// The model was asked about a git repository it made inside the tree.
    planted: bool,
    /// The turn went on with the profile's fallback provider.
    failed_over: bool,
    /// Tokens spent, cached input counting a quarter.
    spent: u64,
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
    context.as_object().cloned().unwrap_or_default()
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

fn system_prompt(root: &Path, yolo: bool, check: Option<&str>, repo: bool, web: bool) -> String {
    let mut prompt = include_str!("prompt.md").to_string();
    if !web {
        prompt = prompt.replace(WEB, "");
    }
    prompt += &match check {
        Some(check) => format!("\ncheck() runs `{}`.", redact(check).0),
        None => "\nNo check command is configured, so check() runs nothing.".into(),
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
    }
    if let Some((agents, size)) = repo_text(root, "AGENTS.md", AGENTS_MAX) {
        prompt += "\n\n# Project instructions (AGENTS.md)\n\n";
        prompt += &agents;
        if size > AGENTS_MAX as u64 {
            let (kept, all) = (AGENTS_MAX / 1024, size.div_ceil(1024));
            prompt +=
                &format!("\n\n[AGENTS.md clipped to its first {kept} KB of {all} KB; read the file for the rest]");
        }
    } else if std::fs::symlink_metadata(root.join("AGENTS.md")).is_ok() {
        let why = "outside the repo, sandbox-protected, unreadable, or not a regular file";
        ui::note(&format!("left out of the prompt ({why}): AGENTS.md"));
    }
    prompt += &listing(root);
    prompt
}

/// The tracked files for the system prompt: a path and size per line within LISTING bytes, else a line per folder
/// within TREE bytes; past both, a repo of over FILES files shows its first FILES two-level folders with file counts.
/// Tells the user which form the model got and its size.
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
        ui::event("listing", &format!("repo map for the model: {n} tracked files as {form} ({size})"), extra);
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
