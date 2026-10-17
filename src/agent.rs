use crate::git::{changes, git, guard_git, snapshot};
use crate::llm::{self, Llm};
use crate::providers::Providers;
use crate::redact::redact;
use crate::session::Session;
use crate::worker::{self, Worker};
use crate::{Config, compact, ui};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const COMPACT_AT: u64 = 100_000;
const MAX_REQUESTS: usize = 200;
const FILES: usize = 200;
const LISTING: usize = 8 * 1024;
const TREE: usize = 10 * 1024;
const AGENTS_MAX: usize = 32 * 1024;
const RESTARTS: usize = 2;
const CHECK_TIMEOUT: Duration = Duration::from_mins(30);
const CHECK_OUT: u64 = 1 << 20;
const TURN_BUDGET: u64 = 60;
const GATE: &str = "The project check failed on the current tree. Fix what fails, run check(), then finish again. \
                    If it cannot pass, say why in one line. The check output ended with:";
const RETRY: &str = "Send one run() now with the next step, or a shorter answer.";
const STRAY: &str = "This turn changed files without edit() or write():";
const STRAY_END: &str = "Delete the ones that are scratch output from running code, keep any that are part of your answer, \
                         then finish again.";
const HELPERS: &[&str] = &["sh", "show", "accept", "edit", "write", "undo", "check", "plan", "done", "finish"];
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
}

impl Agent {
    pub fn new(root: &Path, home: &Path, config: Config, resume: bool, yolo: bool, repo: bool) -> Result<Self> {
        guard_git(root);
        let context = context(root, &config, yolo);
        let session = if resume {
            Session::resume(home, root, context)?
        } else {
            let model = config
                .model
                .clone()
                .or_else(|| config.providers.values().find_map(|p| p.models.first().cloned()))
                .context(
                    "no model: pass -m, set model in ~/.config/low/config.toml, or list one under a provider in \
                     ~/.config/low/providers.toml",
                )?;
            let only = config.providers.keys().next().filter(|_| config.providers.len() == 1);
            let Some(provider) = holding(&config.providers, &model).or(only) else {
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
            let system = system_prompt(root, yolo, config.check.as_deref(), repo);
            Session::create(home, root, provider, &model, &system, profile, context)?
        };
        let (provider, llm) = connect(&config.providers, session.header())?;
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
        if model == self.model() {
            return Ok(());
        }
        let provider = holding(&self.config.providers, model).cloned().unwrap_or_else(|| self.provider.clone());
        self.switch(&provider, model)
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
        match connect(&self.config.providers, self.session.header()) {
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

    pub fn switch(&mut self, provider: &str, model: &str) -> Result<()> {
        anyhow::ensure!(self.config.providers.contains_key(provider), "no provider named {provider}");
        web(provider, &self.config.providers[provider].base_url)?;
        let profile = self.config.models.get(model).cloned().unwrap_or_default();
        let system = system_prompt(&self.root, self.yolo, self.config.check.as_deref(), self.repo);
        self.session.switch(provider, model, &system, profile)?;
        (self.provider, self.llm) = connect(&self.config.providers, self.session.header())?;
        self.compact_at = self.session.header()["compact_at"].as_u64().unwrap_or(COMPACT_AT);
        ui::note(&format!("now using {model} on {provider}"));
        Ok(())
    }

    pub async fn run(&mut self, task: &str) -> Result<bool> {
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
                if let Err(e) = self.unfinished().await {
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
        self.unfinished().await?;
        Ok(false)
    }

    async fn unfinished(&mut self) -> Result<()> {
        match self.current.take() {
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
        let (mut nudged, mut overflowed, mut retries, mut stalled, mut strayed) = (false, false, 0, false, false);
        let ceiling = self.compact_at * TURN_BUDGET;
        let mut spent = 0;
        let mut limit = format!("stopped after {MAX_REQUESTS} model requests in one turn");
        for _ in 0..MAX_REQUESTS {
            if spent >= ceiling {
                limit = format!("stopped after {} in one turn ({TURN_BUDGET} × compact_at)", ui::tokens(spent));
                break;
            }
            if self.upcoming() >= self.compact_at {
                self.compact()?;
            }
            let result = self.llm.complete(self.session.messages(), &tools()).await;
            if let Some(lost) = result.as_ref().err().and_then(llm::lost) {
                self.discard("the provider cut the stream", &lost.thinking);
            }
            let mut message = match result {
                Err(e) if e.is::<llm::ContextOverflow>() && !overflowed => {
                    overflowed = true;
                    self.overflow(e.downcast_ref::<llm::ContextOverflow>().and_then(|o| o.window))?;
                    continue;
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
                Err(e) if retries < 2 && llm::lost(&e).is_some_and(|l| !l.stalled) => {
                    (spent, retries) = (spent + self.sent(), retries + 1);
                    self.retry("was lost: the provider cut the stream")?;
                    continue;
                }
                result => result?,
            };
            overflowed = false;
            if !message["meta"]["usage"]["prompt_tokens"].is_u64() {
                let usage = &mut message["meta"]["usage"];
                usage["prompt_tokens"] = self.sent().into();
                usage["estimated"] = true.into();
            }
            let usage = &message["meta"]["usage"];
            let attempts = message["meta"]["timing"]["attempts"].as_u64().unwrap_or(1).max(1);
            spent += usage["prompt_tokens"].as_u64().unwrap_or(0) * attempts
                + usage["completion_tokens"].as_u64().unwrap_or(0);
            ui::usage(usage);
            self.cache_miss(usage);
            let calls = message["tool_calls"].as_array().cloned().unwrap_or_default();
            let empty = calls.is_empty() && message["content"].as_str().is_none_or(|c| c.trim().is_empty());
            let cut = calls.is_empty() && !empty && message["meta"]["finish"] == "length";
            if (empty || cut) && retries < 2 {
                retries += 1;
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
                continue;
            }
            // Retries count in a row: a reply that is kept starts them over, and one that is still unusable after
            // them ends the turn as stopped.
            if empty || cut {
                let (kind, what) = if empty { ("empty", "an empty reply") } else { ("length", "a cut-off reply") };
                ui::event(kind, &format!("the model sent {what} again; finishing the turn"), Value::Null);
                stalled = true;
            } else {
                retries = 0;
            }
            self.session.push(message)?;
            let mut finished = None;
            for (i, call) in calls.iter().enumerate() {
                self.pending = Some(call["id"].clone());
                let (mut content, mut meta) = self.call(call).await;
                if let Some(report) = meta["finish"].as_str() {
                    match self.finishing(began, &meta, i + 1 == calls.len()) {
                        Ok(()) => finished = Some(report.to_string()),
                        Err(why) => {
                            content += &format!("\n[finish() ignored: {why}; reply when you are ready]");
                            meta["finish_ignored"] = why.into();
                        }
                    }
                }
                self.session
                    .push(json!({"role": "tool", "tool_call_id": call["id"], "content": content, "meta": meta}))?;
                self.pending = None;
            }
            if let Some(why) = self.broken.take() {
                anyhow::bail!(why);
            }
            if let Some(report) = finished {
                ui::prose();
                let mut markdown = crate::render::Markdown::default();
                markdown.push(&report);
                markdown.finish();
                self.session.push(json!({"role": "assistant", "content": report, "meta": {"via": "finish()"}}))?;
            } else if !calls.is_empty() {
                ui::activity("waiting for the model");
                continue;
            }
            let changed = self.changed(&start, began).await;
            // A turn that changed files without edit() or write() most likely left output from running code behind.
            if changed && !strayed && !self.session.entries[began..].iter().any(|e| edited(&e["meta"])) {
                strayed = true;
                let since = std::time::SystemTime::now() - clock.elapsed();
                let paths: Vec<String> = self.diff(began, since).await?.lines().filter_map(changed_path).collect();
                if !paths.is_empty() {
                    stalled = false;
                    ui::event(
                        "nudge",
                        "files changed outside edit()/write(): asking the model about them",
                        Value::Null,
                    );
                    ui::activity("waiting for the model");
                    let text = format!("{STRAY} {}. {STRAY_END}", paths.join(", "));
                    self.session.push(json!({"role": "user", "content": text, "meta": {"host": true}}))?;
                    continue;
                }
            }
            let mut checked = false;
            if changed && let Some(check) = &self.config.check {
                let (ok, out) = self.verify(&check.clone(), true).await;
                if !ok && !nudged {
                    (nudged, stalled) = (true, false);
                    ui::event("nudge", "the check failed on the final tree: asking the model to fix it", Value::Null);
                    ui::activity("waiting for the model");
                    let gate = format!("{GATE}\n{}", compact::tail(&out, 1500));
                    self.session.push(json!({"role": "user", "content": gate, "meta": {"host": true}}))?;
                    continue;
                }
                checked = ok;
            }
            self.current = None;
            return self.report(Some(changed), checked, stalled && !changed, began, clock).await;
        }
        ui::event("limit", &limit, Value::Null);
        self.current = None;
        let changed = self.changed(&start, began).await;
        self.report(Some(changed), false, true, began, clock).await
    }

    /// Whether finish() may end the turn in place of a reply: only where the check covers the report's claims and the
    /// model has read everything the run printed. In a turn without edits nothing checks the report, and an answer
    /// written in the run that gathers its evidence was written before reading it.
    fn finishing(&self, began: usize, meta: &Value, last: bool) -> Result<(), &'static str> {
        let edits = edited(meta) || self.session.entries[began..].iter().any(|e| edited(&e["meta"]));
        if meta["error"] == true {
            Err("the run raised")
        } else if !last {
            Err("more code ran after it in this reply")
        } else if !edits {
            Err("this turn changed no files, so no check covers the report")
        } else if self.config.check.is_none() {
            Err("no check is configured")
        } else if meta["check"] != true {
            Err("check() did not pass in this run")
        } else if meta["own"].as_u64().is_none_or(|own| own > 0) {
            Err("this run printed output you have not read")
        } else {
            Ok(())
        }
    }

    async fn changed(&self, start: &str, began: usize) -> bool {
        self.session.entries[began..].iter().any(|e| edited(&e["meta"])) || self.moved(start).await
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
        let (state, turn) = (self.session.state(), self.current.map(|(began, _)| began));
        // undo() restores from the snapshot this turn began with.
        let snapshot =
            turn.and_then(|began| self.session.entries[began]["meta"]["snapshot"].as_str()).unwrap_or_default();
        let state = json!({"accepted": state.accepted, "plan": state.plan, "turn": turn, "snapshot": snapshot});
        let clock = Instant::now();
        let ran = async {
            let mut reply = self.worker()?.run(code, &state).await?;
            while reply["needs_check"] == true {
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
        let changes = if self.repo {
            self.diff(began, std::time::SystemTime::now() - clock.elapsed()).await?
        } else {
            own_changes(&self.session.entries[began..])
        };
        let first = &self.session.entries[began];
        let snapshot = first["meta"]["snapshot"].as_str().and_then(|s| s.split(' ').next());
        let changed = changed.unwrap_or(changes != "none");
        let path = changed_path;
        let turn = &self.session.entries[began..];
        let actions = turn.iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten());
        let own: Vec<&str> = actions.filter(|a| a["ok"] == true).filter_map(|a| a["path"].as_str()).collect();
        let (mine, outside): (Vec<&str>, Vec<&str>) =
            changes.lines().filter(|l| *l != "none").partition(|l| path(l).is_some_and(|p| own.contains(&p.as_str())));
        let outside: Vec<String> = outside.into_iter().filter_map(path).collect();
        let named = |path: &str| {
            let words = self
                .config
                .check
                .as_deref()
                .unwrap_or_default()
                .split(|c: char| c.is_whitespace() || "'\"();&|<>=`".contains(c));
            path == "low.toml" || words.map(|w| w.trim_start_matches("./")).any(|w| w == path)
        };
        let inputs: Vec<String> = changes.lines().filter_map(path).filter(|p| named(p)).collect();
        let (check, verdict) = match (changed, checked, &self.config.check) {
            (false, _, _) => ("-", "NO CHANGES"),
            (true, true, _) if !inputs.is_empty() => ("passed, but this turn changed what it runs", "UNVERIFIED"),
            (true, true, _) => ("passed on the final tree", "VERIFIED"),
            (true, false, None) => ("none configured: set check in low.toml", "NO CHECK"),
            (true, false, Some(_)) => ("not passed on the final tree", "UNVERIFIED"),
        };
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
        rows.push(("changes", changes.clone()));
        if !outside.is_empty() {
            let text = format!("not through low's edit()/write(), maybe outside low: {}", outside.join(", "));
            rows.push(("outside", text));
        }
        if changed && !self.repo {
            rows.push(("undo", "none: not a git repository, so these changes can't be undone".into()));
        }
        if let Some(tree) = snapshot.filter(|_| changed && self.repo) {
            let short = tree.get(..12).unwrap_or(tree);
            let mine: Vec<String> = mine.into_iter().filter_map(path).collect();
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
            let undo = match (outside.is_empty(), modified.is_empty()) {
                (true, _) => format!("git restore --source={short} --worktree -- .  (new files stay)"),
                (false, false) => format!(
                    "git restore --source={short} --worktree -- {}  (low's edit()/write() paths; new files stay)",
                    modified.iter().map(|p| quoted(p)).collect::<Vec<_>>().join(" ")
                ),
                (false, true) => format!("git restore --source={short} --worktree -- <path>  (new files stay)"),
            };
            rows.push(("undo", undo));
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
        let good = (!changed || checked || self.config.check.is_none()) && !stopped;
        ui::report(&rows, verdict, good);
        let changes: Vec<&str> = changes.lines().filter(|l| *l != "none").collect();
        self.session.push(json!({"report": {
            "verdict": verdict, "changed": changed, "checked": checked, "stopped": stopped, "check_inputs": inputs, "outside": outside, "criteria": state.accepted,
            "plan_steps": steps, "plan_done": done, "changes": changes, "requests": requests, "runs": runs,
            "prompt": prompt, "cached": cached, "out": out, "ms": ms,
        }}))?;
        Ok(good)
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
        let stored = entries.find_map(|(i, e)| e["reasoning_content"].as_str().map(|r| (i, r)));
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

/// The provider that lists a model.
fn holding<'a>(providers: &'a Providers, model: &str) -> Option<&'a String> {
    providers.iter().find(|(_, p)| p.models.iter().any(|m| m == model)).map(|(name, _)| name)
}

fn connect(providers: &Providers, header: &Value) -> Result<(String, Llm)> {
    let model = header["model"].as_str().unwrap_or_default();
    let recorded = header["provider"].as_str();
    let listed = holding(providers, model).map(String::as_str);
    let fallback = providers.keys().next().map(String::as_str);
    let name = recorded
        .filter(|n| providers.contains_key(*n))
        .or(listed)
        .or(fallback)
        .with_context(|| format!("no provider: {}", crate::providers::example(model)))?;
    match recorded {
        Some(old) if old != name => {
            ui::note(&format!("provider {old} is gone; sending {model} to {name} (/models or /providers changes it)"))
        }
        None if listed.is_none() && providers.len() > 1 => {
            ui::note(&format!("no provider lists {model}; using {name} (/models or /providers changes it)"))
        }
        _ => {}
    }
    let p = &providers[name];
    web(name, &p.base_url)?;
    let url = llm::endpoint(&p.base_url);
    let extra = json!({"provider": name, "model": model, "base_url": url, "key": crate::providers::key(p)});
    ui::record("connect", &format!("{model} on {name} at {url}"), extra);
    Ok((name.into(), Llm::new(&p.base_url, p.api_key.as_deref(), header)))
}

async fn grouped(mut cmd: tokio::process::Command) -> Result<(bool, String)> {
    let path = std::env::temp_dir().join(format!("low-check-{}-{}", std::process::id(), ui::now_ms()));
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    let _ = std::fs::remove_file(&path);
    let mut child = cmd.stdin(Stdio::null()).stdout(file.try_clone()?).process_group(0).spawn()?;
    let pid = child.id().context("the check exited before it started")?.cast_signed();
    ui::CHECK.store(pid, Ordering::Relaxed);
    let mut group = Group(vec![pid]);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
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
    let size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(size.saturating_sub(CHECK_OUT)))?;
    let mut out = vec![];
    file.read_to_end(&mut out)?;
    let out = String::from_utf8_lossy_owned(out);
    let out = if size > CHECK_OUT { format!("[… {} bytes omitted]\n{out}", size - CHECK_OUT) } else { out };
    Ok((status.success(), out))
}

/// An error as low shows and logs it: the whole chain, without control characters or secrets.
pub fn error_text(e: &anyhow::Error) -> String {
    redact(&ui::printable(&format!("{e:#}"))).0
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

/// What edit() and write() changed in a turn, listed as changes() lists it: outside a git repository that is all low
/// can see.
fn own_changes(turn: &[Value]) -> String {
    let mut files: BTreeMap<&str, (u64, u64, bool)> = BTreeMap::new();
    for action in turn.iter().flat_map(|e| e["meta"]["actions"].as_array().into_iter().flatten()) {
        if let Some(path) = action["path"].as_str().filter(|_| action["ok"] == true) {
            let file = files.entry(path).or_default();
            file.0 += action["added"].as_u64().unwrap_or(0);
            file.1 += action["removed"].as_u64().unwrap_or(0);
            file.2 |= action["tool"] == "write";
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
    line.strip_suffix(" (new)").or_else(|| line.rsplit_once(" +").map(|(path, _)| path)).map(String::from)
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

fn system_prompt(root: &Path, yolo: bool, check: Option<&str>, repo: bool) -> String {
    let mut prompt = include_str!("prompt.md").to_string();
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
